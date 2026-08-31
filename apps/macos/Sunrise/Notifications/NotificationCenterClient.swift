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
    /// Identifiers of every request the OS is currently holding for this app.
    func pendingIdentifiers() async -> [String]
    /// Hand one over.
    func schedule(_ notification: PlannedNotification) async throws
    /// Withdraw scheduled requests by identifier.
    func cancel(identifiers: [String]) async
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
        center.setNotificationCategories(Set(ReminderCategory.allCases.map(Self.category)))
    }

    func install(delegate: ReminderResponder) async {
        center.delegate = delegate
    }

    func pendingIdentifiers() async -> [String] {
        await center.pendingNotificationRequests().map(\.identifier)
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
                    title: "Mark Done",
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
        return UNNotificationCategory(
            identifier: kind.rawValue,
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
    static func request(for notification: PlannedNotification) -> UNNotificationRequest {
        let content = UNMutableNotificationContent()
        content.title = notification.title
        if !notification.body.isEmpty { content.body = notification.body }
        content.categoryIdentifier = notification.category.rawValue
        content.sound = .default
        content.userInfo = [
            ReminderPayload.entity: notification.entity,
            ReminderPayload.link: notification.link.url?.absoluteString ?? ""
        ]
        let fire = Date(timeIntervalSince1970: Double(notification.fireAtMs) / 1000)
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .current
        let components = calendar.dateComponents(
            [.year, .month, .day, .hour, .minute, .second],
            from: fire
        )
        return UNNotificationRequest(
            identifier: notification.identifier,
            content: content,
            trigger: UNCalendarNotificationTrigger(dateMatching: components, repeats: false)
        )
    }
}

/// The keys in a notification's `userInfo`.
enum ReminderPayload {
    static let entity = "sunrise.entity"
    static let link = "sunrise.link"
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
        link: String?
    ) -> ReminderResponse {
        if actionIdentifier == UNNotificationDefaultActionIdentifier {
            guard let link, let url = URL(string: link), let parsed = DeepLink(url: url) else {
                return .dismissed
            }
            return .open(parsed)
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
            userInfo: response.notification.request.content.userInfo
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
