import Foundation
import UserNotifications

/// What ``ReminderScheduler`` needs from the notification service.
///
/// A protocol rather than `UNUserNotificationCenter.current()` reached for
/// directly, and not for style. `current()` talks to a system service that a
/// unit test cannot answer for: the pending list belongs to the *machine*, so
/// two tests running in the same host would see each other's requests, and
/// asking for authorization would put a real prompt on the developer's screen.
/// Behind this seam the reconciliation is deterministic and isolated, which is
/// what `CLAUDE.md` asks of core logic.
protocol NotificationCenterClient: Sendable {
    /// What the system says right now — re-read rather than remembered, since
    /// the user can revoke permission in System Settings while the app runs.
    func authorization() async -> NotificationAuthorization
    /// Ask. Only meaningful once; afterwards the system answers from its own
    /// record without showing anything.
    func requestAuthorization() async -> NotificationAuthorization
    /// Install the action buttons. Idempotent, and cheap enough to repeat.
    func registerCategories() async
    /// Route button presses and body taps back to the app.
    ///
    /// The notification centre holds its delegate **weakly**, so whoever calls
    /// this owns the object's lifetime — a responder created inline here would
    /// be deallocated before the first button was ever pressed.
    func install(delegate: ReminderResponder) async
    /// Every request the OS is currently holding for this app, with the
    /// revision each was scheduled with where it carries one.
    func pendingRequests() async -> [PendingReminder]
    /// Hand one over.
    func schedule(_ notification: PlannedNotification) async throws
    /// Withdraw scheduled requests by identifier.
    func cancel(identifiers: [String]) async
}

extension NotificationCenterClient {
    /// Identifiers of every request the OS is currently holding for this app.
    func pendingIdentifiers() async -> [String] {
        await pendingRequests().map(\.identifier)
    }
}

/// The real one.
///
/// A `struct` with no stored state, so `Sendable` is free and honest: every
/// method reaches for `UNUserNotificationCenter.current()`, which is the
/// process-wide singleton and is safe to call from anywhere.
struct SystemNotificationCenter: NotificationCenterClient {
    private var center: UNUserNotificationCenter { .current() }

    func authorization() async -> NotificationAuthorization {
        NotificationAuthorization(await center.notificationSettings().authorizationStatus)
    }

    func requestAuthorization() async -> NotificationAuthorization {
        do {
            _ = try await center.requestAuthorization(options: [.alert, .sound, .badge])
        } catch {
            // A refusal is *not* an error — it comes back as `false` and then
            // as `.denied` below. Reaching here means the service itself would
            // not answer, which is a different sentence in Settings.
            return .unavailable(error.localizedDescription)
        }
        return await authorization()
    }

    func registerCategories() async {
        center.setNotificationCategories(Set(ReminderCategory.all.map(Self.category)))
    }

    func install(delegate: ReminderResponder) async {
        center.delegate = delegate
    }

    func pendingRequests() async -> [PendingReminder] {
        await center.pendingNotificationRequests().map {
            PendingReminder(
                identifier: $0.identifier,
                revision: $0.content.userInfo[ReminderPayload.revision] as? String
            )
        }
    }

    func schedule(_ notification: PlannedNotification) async throws {
        try await center.add(Self.request(for: notification))
    }

    func cancel(identifiers: [String]) async {
        guard !identifiers.isEmpty else { return }
        center.removePendingNotificationRequests(withIdentifiers: identifiers)
    }

    /// One category, with the buttons `docs/08-features/notifications.md`
    /// §Action buttons puts on a reminder.
    ///
    /// "Open in app" is not among them: the same doc's platform table in
    /// `docs/07-clients/interaction-patterns.md` wires Open to "tap body",
    /// which is the platform idiom and needs no button of its own. Tapping the
    /// body carries ``PlannedNotification/link``.
    static func category(_ kind: ReminderCategory) -> UNNotificationCategory {
        var actions: [UNNotificationAction] = []
        if kind.offersCompletion {
            actions.append(
                UNNotificationAction(
                    identifier: ReminderAction.complete.rawValue,
                    title: L10n.Notifications.markDone,
                    options: []
                )
            )
        }
        for span in kind.snoozeSpans {
            actions.append(
                UNNotificationAction(
                    identifier: ReminderAction.snooze(span).rawValue,
                    title: span.buttonTitle,
                    options: []
                )
            )
        }
        // A catalog kind's buttons. One that opens a screen brings the app
        // forward; one that acts (Mark Done, a snooze, Keep) runs without it.
        for action in kind.catalogActions {
            actions.append(
                UNNotificationAction(
                    identifier: action.identifier,
                    title: action.buttonTitle,
                    options: action.opensApp ? [.foreground] : []
                )
            )
        }
        return UNNotificationCategory(
            identifier: kind.identifier,
            actions: actions,
            intentIdentifiers: [],
            options: []
        )
    }

    /// One planned notification, as the OS wants it.
    ///
    /// A calendar trigger rather than a time-interval one. The difference
    /// shows up on a laptop that was asleep at the fire time: an interval
    /// trigger counts down from when it was armed, a calendar trigger names an
    /// instant, and a reminder for 09:00 that arrives at 09:00 is the entire
    /// feature.
    ///
    /// An alert that is already due has no trigger at all, which the OS reads
    /// as "now".
    static func request(
        for notification: PlannedNotification,
        now: Date = .now
    ) -> UNNotificationRequest {
        let content = UNMutableNotificationContent()
        content.title = notification.title
        if !notification.body.isEmpty { content.body = notification.body }
        content.categoryIdentifier = notification.category.identifier
        content.sound = .default
        content.interruptionLevel = interruptionLevel(notification.level)
        var info: [String: String] = [
            ReminderPayload.entity: notification.entity,
            ReminderPayload.link: notification.link?.url?.absoluteString ?? ""
        ]
        if let revision = notification.revision { info[ReminderPayload.revision] = revision }
        content.userInfo = info
        let fire = Date(timeIntervalSince1970: Double(notification.fireAtMs) / 1000)
        let trigger: UNNotificationTrigger? = notification.postsWhenDue && fire <= now
            ? nil
            : UNCalendarNotificationTrigger(dateMatching: components(of: fire), repeats: false)
        return UNNotificationRequest(
            identifier: notification.identifier,
            content: content,
            trigger: trigger
        )
    }

    /// The calendar components of an instant, **with their zone**, so the
    /// trigger fires at that instant after the device changes zone rather than
    /// at the same wall clock in the new one.
    static func components(of fire: Date, in zone: TimeZone = .current) -> DateComponents {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = zone
        return calendar.dateComponents(
            [.timeZone, .year, .month, .day, .hour, .minute, .second],
            from: fire
        )
    }

    /// The catalog's level, as the OS spells it. Time-sensitive needs the
    /// entitlement to be honoured; without it the system delivers the request
    /// as active, which is the documented degradation (§Platform mechanisms).
    static func interruptionLevel(_ level: InterruptionLevel) -> UNNotificationInterruptionLevel {
        switch level {
        case .passive: .passive
        case .active: .active
        case .timeSensitive: .timeSensitive
        }
    }
}

/// The keys in a notification's `userInfo`.
enum ReminderPayload {
    static let entity = "sunrise.entity"
    static let link = "sunrise.link"
    /// The core's revision for a catalog kind, read back by the next re-plan.
    static let revision = "sunrise.revision"
}

extension NotificationAction {
    /// Every button of the catalog, for reading one back from the OS.
    static let known: [NotificationAction] = [
        .markDone, .snoozeOneHour, .snoozeTomorrow, .snoozeFiveMinutes, .snoozeFifteenMinutes,
        .defer, .skip, .startFocus, .planMyDay, .openTriage, .moveOpenToTomorrow, .open,
        .seeTomorrow, .addFiveMinutes, .startBreak, .reviewTimes, .keep, .startReview,
        .openSyncStatus, .reviewDevices, .reviewSecurity, .reconnect, .openHereNow
    ]

    /// The prefix of a catalog button's identifier. Apart from
    /// ``ReminderAction``'s, because a catalog snooze re-posts the notification
    /// where a reminder's snooze moves the task.
    static let identifierPrefix = "sunrise.catalog."

    /// The identifier the OS hands back when this button is pressed.
    var identifier: String { "\(Self.identifierPrefix)\(tag)" }

    /// Read a button back from its identifier.
    init?(identifier: String) {
        guard identifier.hasPrefix(Self.identifierPrefix) else { return nil }
        let tag = identifier.dropFirst(Self.identifierPrefix.count)
        guard let action = Self.known.first(where: { $0.tag == tag }) else { return nil }
        self = action
    }

    /// Its name in an identifier.
    var tag: String {
        switch self {
        case .markDone: "mark_done"
        case .snoozeOneHour: "snooze_one_hour"
        case .snoozeTomorrow: "snooze_tomorrow"
        case .snoozeFiveMinutes: "snooze_five_minutes"
        case .snoozeFifteenMinutes: "snooze_fifteen_minutes"
        case .defer: "defer"
        case .skip: "skip"
        case .startFocus: "start_focus"
        case .planMyDay: "plan_my_day"
        case .openTriage: "open_triage"
        case .moveOpenToTomorrow: "move_open_to_tomorrow"
        case .open: "open"
        case .seeTomorrow: "see_tomorrow"
        case .addFiveMinutes: "add_five_minutes"
        case .startBreak: "start_break"
        case .reviewTimes: "review_times"
        case .keep: "keep"
        case .startReview: "start_review"
        case .openSyncStatus: "open_sync_status"
        case .reviewDevices: "review_devices"
        case .reviewSecurity: "review_security"
        case .reconnect: "reconnect"
        case .openHereNow: "open_here_now"
        }
    }

    /// What the button says.
    var buttonTitle: String {
        switch self {
        case .markDone: L10n.Notifications.markDone
        case .snoozeOneHour: L10n.Notifications.snoozeHour
        case .snoozeTomorrow: L10n.Notifications.snoozeTomorrow
        case .snoozeFiveMinutes: L10n.Notifications.actionSnoozeFiveMinutes
        case .snoozeFifteenMinutes: L10n.Notifications.actionSnoozeFifteenMinutes
        case .defer: L10n.Notifications.actionDefer
        case .skip: L10n.Notifications.actionSkip
        case .startFocus: L10n.Notifications.actionStartFocus
        case .planMyDay: L10n.Notifications.actionPlanMyDay
        case .openTriage: L10n.Notifications.actionOpenTriage
        case .moveOpenToTomorrow: L10n.Notifications.actionMoveOpenToTomorrow
        case .open: L10n.Notifications.actionOpen
        case .seeTomorrow: L10n.Notifications.actionSeeTomorrow
        case .addFiveMinutes: L10n.Notifications.actionAddFiveMinutes
        case .startBreak: L10n.Notifications.actionStartBreak
        case .reviewTimes: L10n.Notifications.actionReviewTimes
        case .keep: L10n.Notifications.actionKeep
        case .startReview: L10n.Notifications.actionStartReview
        case .openSyncStatus: L10n.Notifications.actionOpenSyncStatus
        case .reviewDevices: L10n.Notifications.actionReviewDevices
        case .reviewSecurity: L10n.Notifications.actionReviewSecurity
        case .reconnect: L10n.Notifications.actionReconnect
        case .openHereNow: L10n.Notifications.actionOpenHereNow
        }
    }

    /// How far a snooze button puts the notification off, when it is one
    /// measured in minutes. ``snoozeTomorrow`` is a date, not a span, and is
    /// asked of the core.
    var snoozeMinutes: Int? {
        switch self {
        case .snoozeOneHour: 60
        case .snoozeFiveMinutes: 5
        case .snoozeFifteenMinutes: 15
        default: nil
        }
    }

    /// Whether pressing it opens a screen, so the app must come forward.
    var opensApp: Bool {
        switch self {
        case .markDone, .snoozeOneHour, .snoozeTomorrow, .snoozeFiveMinutes,
             .snoozeFifteenMinutes, .keep:
            false
        default:
            true
        }
    }
}

/// One of the buttons on a reminder.
enum ReminderAction: Equatable, Sendable {
    case complete
    case snooze(SnoozeSpan)

    /// The identifier the OS hands back. Built from the deep-link action name
    /// so the button and the equivalent `sunrise://` URL cannot drift apart.
    var rawValue: String { "sunrise.action.\(linkAction)" }

    var linkAction: String {
        switch self {
        case .complete: "complete"
        case let .snooze(span): span.linkAction
        }
    }

    init?(rawValue: String) {
        let prefix = "sunrise.action."
        guard rawValue.hasPrefix(prefix) else { return nil }
        switch String(rawValue.dropFirst(prefix.count)) {
        case "complete": self = .complete
        case SnoozeSpan.oneHour.linkAction: self = .snooze(.oneHour)
        case SnoozeSpan.tomorrow.linkAction: self = .snooze(.tomorrow)
        case SnoozeSpan.nextWeek.linkAction: self = .snooze(.nextWeek)
        default: return nil
        }
    }
}

/// What the user did with a notification.
enum ReminderResponse: Equatable, Sendable {
    /// Tapped the body: open what the link points at.
    case open(DeepLink)
    /// Pressed a button: run it against the core, with no window.
    case act(EntityRef, ReminderAction)
    /// Pressed one of a catalog kind's buttons, on the notification it was
    /// pressed on — which a snooze posts again.
    case catalog(NotificationAction, PlannedNotification)
    /// Swiped it away, or pressed something this build does not know.
    case dismissed
}

extension ReminderResponse {
    /// Decode a response without touching `UserNotifications`.
    ///
    /// Split out so the interesting half — which button maps to which command
    /// — is testable, since constructing a `UNNotificationResponse` is not
    /// something an app can do at all.
    static func decode(
        actionIdentifier: String,
        entity: String?,
        link: String?,
        delivered: PlannedNotification? = nil
    ) -> ReminderResponse {
        if actionIdentifier == UNNotificationDefaultActionIdentifier {
            guard let link, let url = URL(string: link), let parsed = DeepLink(url: url) else {
                return .dismissed
            }
            return .open(parsed)
        }
        if let action = NotificationAction(identifier: actionIdentifier) {
            guard let delivered else { return .dismissed }
            return .catalog(action, delivered)
        }
        guard let action = ReminderAction(rawValue: actionIdentifier),
              let entity, !entity.isEmpty else { return .dismissed }
        return .act(entity, action)
    }

    /// The same, from the dictionary the OS actually hands over.
    static func decode(
        actionIdentifier: String,
        userInfo: [AnyHashable: Any]
    ) -> ReminderResponse {
        decode(
            actionIdentifier: actionIdentifier,
            entity: userInfo[ReminderPayload.entity] as? String,
            link: userInfo[ReminderPayload.link] as? String
        )
    }

    /// The same, from the request the OS delivered, which a catalog button
    /// needs whole: a snooze posts it again.
    static func decode(actionIdentifier: String, request: UNNotificationRequest) -> ReminderResponse {
        let content = request.content
        let entity = content.userInfo[ReminderPayload.entity] as? String
        let link = content.userInfo[ReminderPayload.link] as? String
        let category = ReminderCategory.all.first { $0.identifier == content.categoryIdentifier }
        let delivered = category.map {
            PlannedNotification(
                identifier: request.identifier,
                title: content.title,
                body: content.body,
                category: $0,
                fireAtMs: Int64(Date.now.timeIntervalSince1970 * 1000),
                entity: entity ?? "",
                link: link.flatMap(URL.init(string:)).flatMap(DeepLink.init(url:)),
                level: level(content.interruptionLevel)
            )
        }
        return decode(actionIdentifier: actionIdentifier, entity: entity, link: link, delivered: delivered)
    }

    private static func level(_ level: UNNotificationInterruptionLevel) -> InterruptionLevel {
        switch level {
        case .passive: .passive
        case .timeSensitive, .critical: .timeSensitive
        default: .active
        }
    }
}

/// The `UNUserNotificationCenterDelegate`, and nothing else.
///
/// `@unchecked Sendable` with a straight face: the one stored property is a
/// `let` closure that is itself `@Sendable`, and `NSObject` contributes no
/// mutable state. The alternative — isolating the class to the main actor —
/// would not compile against a delegate protocol whose isolation this app does
/// not control.
final class ReminderResponder: NSObject, UNUserNotificationCenterDelegate, @unchecked Sendable {
    private let handle: @Sendable @MainActor (ReminderResponse) async -> Void

    init(handle: @escaping @Sendable @MainActor (ReminderResponse) async -> Void) {
        self.handle = handle
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let decoded = ReminderResponse.decode(
            actionIdentifier: response.actionIdentifier,
            request: response.notification.request
        )
        await handle(decoded)
    }

    /// Show reminders even while Sunrise is the front app.
    ///
    /// The default is to swallow them, which would mean the one alert most
    /// worth seeing — the block starting in fifteen minutes, while you are
    /// looking at the app — is the one that never appears.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }
}
