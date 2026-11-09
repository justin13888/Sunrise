import Foundation

/// Which set of action buttons a reminder carries.
///
/// A Block cannot be completed and cannot be deferred — the commands do not
/// exist — so it gets its own category rather than three buttons that would
/// have to fail. `docs/08-features/notifications.md` §Action buttons says
/// "where the OS supports actions", and this is the same reservation one level
/// down: where the *entity* supports them.
enum ReminderCategory: Equatable, Hashable, Sendable {
    /// A Task (or a materialized routine occurrence, which is a Task by the
    /// time it is due): done, defer, snooze, open.
    case task
    /// A time block: open only.
    case block
    /// One of the other kinds of the catalog: the deadline, the briefs, the
    /// triage nudge, wind-down and the alerts. Its buttons are the catalog's
    /// own (``catalogActions``), read from the core rather than listed here.
    case catalog(NotificationKind)

    /// Every category this app registers: the two reminder ones, then one per
    /// kind of the catalog, in catalog order.
    static var all: [ReminderCategory] {
        [.task, .block] + NotificationCatalog.kinds.map { .catalog($0.kind) }
    }

    /// What the OS knows it by.
    var identifier: String {
        switch self {
        case .task: "sunrise.reminder.task"
        case .block: "sunrise.reminder.block"
        case let .catalog(kind): "sunrise.category.\(NotificationCatalog.tag(of: kind))"
        }
    }

    /// The snooze spans this category offers, in button order.
    ///
    /// The two the spec names, and no third: a notification with five buttons
    /// is a menu, and macOS collapses everything past the second into one
    /// anyway.
    var snoozeSpans: [SnoozeSpan] {
        switch self {
        case .task: [.oneHour, .tomorrow]
        case .block, .catalog: []
        }
    }

    var offersCompletion: Bool { self == .task }

    /// A catalog kind's buttons, in the catalog's order; none for the two
    /// reminder categories, whose buttons are ``offersCompletion`` and
    /// ``snoozeSpans``.
    var catalogActions: [NotificationAction] {
        guard case let .catalog(kind) = self else { return [] }
        return NotificationCatalog.info(kind)?.actions ?? []
    }
}

/// The core's catalog of notification kinds, read once
/// (`docs/08-features/notifications.md` §Catalog).
///
/// Data, not a list kept here: a kind's key, default, audience and buttons
/// are the core's, and Settings and the category registration read them from
/// the same table the planner does.
enum NotificationCatalog {
    /// Every kind, in catalog order.
    static let kinds: [NotificationKindInfo] = notificationCatalog()

    /// The identifier namespaces of the scheduled kinds and of the alerts.
    static let prefixes: NotificationPrefixes = notificationPrefixes()

    /// The one identifier wind-down is held under. The core's, because a
    /// cancel names none and must withdraw exactly what an add scheduled.
    static let windDown: String = windDownIdentifier()

    /// One kind's row.
    static func info(_ kind: NotificationKind) -> NotificationKindInfo? {
        kinds.first { $0.kind == kind }
    }

    /// A kind's name in keys and identifiers: `morning_brief`.
    static func tag(of kind: NotificationKind) -> String {
        info(kind)?.tag ?? "unknown"
    }

    /// The catalog kind a reminder source is.
    static func kind(of reminder: ReminderKind) -> NotificationKind {
        switch reminder {
        case .task: .taskReminder
        case .block: .blockStart
        case .routine: .routineDue
        }
    }

    /// Whether `kind` is on in `policy`. A kind the policy does not list is
    /// on: the policy lists every kind, so that only happens across a version
    /// skew, where staying quiet would be the surprise.
    static func isOn(_ kind: NotificationKind, in policy: CatalogPolicyItem) -> Bool {
        policy.toggles.first { $0.kind == kind }?.enabled ?? true
    }

    /// `policy` with `kind` switched to `enabled`, as a write of its key
    /// leaves it. A kind the policy does not list is left out, as before.
    static func switching(
        _ kind: NotificationKind,
        to enabled: Bool,
        in policy: CatalogPolicyItem
    ) -> CatalogPolicyItem {
        var out = policy
        for index in out.toggles.indices where out.toggles[index].kind == kind {
            out.toggles[index].enabled = enabled
        }
        return out
    }

    /// Whether a device that is not the primary one shows `kind`.
    ///
    /// Security alerts go to every device and the device-local kinds to the
    /// one concerned; the rest are the primary device's alone (§Multi-device
    /// dedup).
    static func showsOffPrimary(_ kind: NotificationKind) -> Bool {
        info(kind)?.audience != .primary
    }
}

/// One local notification this app wants the OS to be holding.
///
/// Deliberately not a `UNNotificationRequest`: everything about *what* to
/// schedule is decided here, where it can be asserted without a notification
/// service, and the `UserNotifications` types appear only in the thin client
/// that hands it over.
struct PlannedNotification: Equatable, Sendable {
    /// Stable across reschedules. See ``ReminderPlan/identifier(for:)``.
    let identifier: String
    let title: String
    /// Empty when the title says everything.
    let body: String
    let category: ReminderCategory
    /// Epoch ms, after lead time and quiet hours — the core applied both.
    let fireAtMs: Int64
    /// The entity a button acts on; empty when it is about no entity (a brief).
    let entity: EntityRef
    /// What tapping the body opens; `nil` opens the app where it was.
    let link: DeepLink?
    /// The core's revision for a catalog kind, stored with the request and
    /// handed back on the next re-plan so an unchanged one is left alone.
    /// `nil` for a reminder, whose identifier carries its fire time.
    var revision: String?
    /// How loud.
    var level: InterruptionLevel = .active
    /// Post at once rather than at ``fireAtMs``, when that is already due:
    /// the alerts, which are raised on an event rather than ahead of one.
    var postsWhenDue = false
}

/// What one re-plan of the rest of the catalog — everything but the reminders
/// — says to do. See ``ReminderScheduler/reconcile()``.
struct CatalogPass {
    /// This install's device id in the open vault: what the ledger is kept
    /// under.
    var device: String
    /// What the OS should hold for them once this pass is done.
    var held: [PlannedNotification] = []
    /// What to hand over: new, replaced, and the alerts to post.
    var schedule: [PlannedNotification] = []
    /// What to withdraw.
    var cancel: [String] = []
    /// The ledger to keep once the alerts are posted.
    var ledger: String?
    /// Which kinds are on.
    var policy: CatalogPolicyItem?
}

/// The difference between what the OS is holding and what it should hold.
struct ReminderReconciliation: Equatable {
    /// Not yet scheduled.
    var add: [PlannedNotification] = []
    /// Ours, scheduled, and no longer wanted.
    var cancel: [String] = []
    /// Already scheduled and still correct — left completely alone.
    var kept: [String] = []

    var isEmpty: Bool { add.isEmpty && cancel.isEmpty }
}

/// Turning `Query::ReminderIntents` into an OS schedule, and keeping it there.
///
/// Every function is pure. That is the point: the interesting behaviour is not
/// "can we call `UNUserNotificationCenter`" but "does a sync burst that fires
/// the change feed forty times produce forty duplicate alerts", and the second
/// question is answered by ``reconcile(desired:pending:)`` alone.
enum ReminderPlan {
    /// Every identifier this app owns starts with it.
    ///
    /// Load-bearing: reconciliation cancels what it does not want, and without
    /// a namespace that would mean cancelling notifications posted by anything
    /// else this app ever schedules.
    static let identifierPrefix = "sunrise.reminder."

    /// What the body says when quiet hours moved a reminder.
    static var quietHoursNote: String { L10n.Notifications.quietHoursNote }

    /// How far ahead to schedule.
    ///
    /// A day. Long enough that a Mac asleep overnight still wakes with the
    /// morning's reminders pending, short enough that the pending list stays
    /// well inside the OS cap below.
    static let horizonMs: UInt64 = 24 * 60 * 60 * 1000

    /// The most reminders to leave pending.
    ///
    /// The system keeps at most 64 scheduled local notifications per app and
    /// silently discards the rest — so the cap is honoured here, where the
    /// earliest are kept, rather than by the OS, which would drop an arbitrary
    /// set. The margin below 64 is for anything else this app may schedule.
    static let maxScheduled = 56

    /// A stable identity for one intent.
    ///
    /// Entity, source and fire time. All three are needed and none is
    /// redundant: a task can move (new fire time, new identity, the old one
    /// cancelled), and one entity can be both a task reminder and a block
    /// start on the same minute.
    ///
    /// The consequence is the one that matters — **the same intent computed
    /// twice produces the same identifier**, so a reschedule that has nothing
    /// to say adds nothing.
    static func identifier(for reminder: Reminder) -> String {
        "\(identifierPrefix)\(tag(for: reminder.kind)).\(reminder.entity).\(reminder.fireAt)"
    }

    /// Everything to hand the OS, earliest first, capped.
    static func plan(
        for reminders: [Reminder],
        timeZone: TimeZone = .current,
        limit: Int = maxScheduled
    ) -> [PlannedNotification] {
        reminders
            .sorted { $0.fireAt < $1.fireAt }
            .prefix(max(0, limit))
            .map { notification(for: $0, timeZone: timeZone) }
    }

    /// One intent, as a notification.
    static func notification(
        for reminder: Reminder,
        timeZone: TimeZone = .current
    ) -> PlannedNotification {
        let category: ReminderCategory = reminder.kind == .block ? .block : .task
        return PlannedNotification(
            identifier: identifier(for: reminder),
            title: reminder.title,
            body: body(for: reminder, timeZone: timeZone),
            category: category,
            fireAtMs: reminder.fireAt,
            entity: reminder.entity,
            link: category == .block
                ? .entity(reminder.entity)
                : .task(reminder.entity, .open),
            level: NotificationCatalog.info(NotificationCatalog.kind(of: reminder.kind))?.level ?? .active
        )
    }

    /// One catalog kind the core planned, as a notification.
    ///
    /// Everything is the core's: the identifier, the revision, the words, the
    /// instant, the level and the link. `alert` marks one of the posted kinds,
    /// which fires as soon as it is handed over unless focus or quiet hours
    /// moved it later.
    static func notification(for item: CatalogNotificationItem, alert: Bool = false) -> PlannedNotification {
        PlannedNotification(
            identifier: item.identifier,
            title: item.title,
            body: item.body,
            category: .catalog(item.kind),
            fireAtMs: item.fireAtMs,
            entity: item.entity ?? "",
            link: URL(string: item.link).flatMap(DeepLink.init(url:)),
            revision: item.revision,
            level: item.level,
            postsWhenDue: alert
        )
    }

    /// The wind-down the core planned, as a notification: time-sensitive, so
    /// a Sleep Focus that allows Sunrise lets it through (§Wind-down), and
    /// opening the end-of-day plan, which is where tomorrow is laid out.
    static func notification(for windDown: WindDownItem) -> PlannedNotification {
        PlannedNotification(
            identifier: windDown.identifier,
            title: windDown.title,
            body: "",
            category: .catalog(.windDown),
            fireAtMs: windDown.fireAtMs,
            entity: "",
            link: .endOfDayPlan,
            revision: windDown.revision,
            level: NotificationCatalog.info(.windDown)?.level ?? .timeSensitive
        )
    }

    /// The reminders a device with these switches delivers: a reminder whose
    /// kind is turned off is left out, here rather than after the cap, so it
    /// does not spend a slot.
    static func enabled(_ reminders: [Reminder], under policy: CatalogPolicyItem) -> [Reminder] {
        reminders.filter {
            NotificationCatalog.isOn(NotificationCatalog.kind(of: $0.kind), in: policy)
        }
    }

    /// The catalog notifications this device shows. A device that is not the
    /// primary one keeps only the kinds every device shows.
    static func audible(_ items: [CatalogNotificationItem], isPrimary: Bool) -> [CatalogNotificationItem] {
        isPrimary ? items : items.filter { NotificationCatalog.showsOffPrimary($0.kind) }
    }

    /// The identifier a snoozed copy of `identifier` is held under.
    ///
    /// Outside every reconciled namespace, so no re-plan cancels it: the
    /// snooze is the user's, and the core never planned it.
    static func snoozedIdentifier(_ identifier: String) -> String {
        "sunrise.snoozed.\(identifier)"
    }

    /// `delivered`, posted again at `fireAtMs` under its snoozed identifier.
    static func snoozed(_ delivered: PlannedNotification, until fireAtMs: Int64) -> PlannedNotification {
        PlannedNotification(
            identifier: snoozedIdentifier(delivered.identifier),
            title: delivered.title,
            body: delivered.body,
            category: delivered.category,
            fireAtMs: fireAtMs,
            entity: delivered.entity,
            link: delivered.link,
            level: delivered.level
        )
    }

    /// 12- or 24-hour, as this locale writes a time: what the core words a
    /// "held from 06:15" body in.
    static func clockStyle(locale: Locale = .current) -> ClockStyle {
        let pattern = DateFormatter.dateFormat(fromTemplate: "j", options: 0, locale: locale) ?? ""
        return pattern.contains("a") ? .h12 : .h24
    }

    /// What the notification says under its title.
    ///
    /// Empty for an ordinary task reminder, on purpose: the title is the
    /// task's, computed on-device by the core, and a second line repeating
    /// "Sunrise reminder" is noise on a surface that interrupts someone.
    ///
    /// It is *not* empty when there is something the user could not otherwise
    /// know — that a block is starting, or that quiet hours moved this alert
    /// off the time they set. The second is the visible half of
    /// `quiet_hours.policy = "queue"`: without it, a 07:00 alert for a 06:30
    /// task looks like a bug.
    static func body(for reminder: Reminder, timeZone: TimeZone = .current) -> String {
        var parts: [String] = []
        if reminder.kind == .block { parts.append(L10n.Notifications.blockStarting) }
        if let from = reminder.deferredFrom {
            parts.append(quietHoursNote)
            parts.append(L10n.Notifications.dueAt(time: clock(from, in: timeZone)))
        }
        return parts.joined(separator: " · ")
    }

    /// Desired against pending: what to add, what to cancel, what to leave.
    ///
    /// The whole reason this exists. `UNUserNotificationCenter.add` on an
    /// identifier it already holds *replaces* the request, which sounds
    /// idempotent and is not — it resets the trigger, and a change feed that
    /// fires forty times during a sync burst would re-arm every pending alert
    /// forty times. Reconciling first means an unchanged schedule costs one
    /// read of the pending list and nothing else.
    ///
    /// Only identifiers under ``identifierPrefix`` are ever cancelled.
    static func reconcile(
        desired: [PlannedNotification],
        pending: [String]
    ) -> ReminderReconciliation {
        let ours = Set(pending.filter { $0.hasPrefix(identifierPrefix) })
        let wanted = Set(desired.map(\.identifier))
        var plan = ReminderReconciliation()
        plan.add = desired.filter { !ours.contains($0.identifier) }
        plan.cancel = pending.filter { ours.contains($0) && !wanted.contains($0) }.sorted()
        plan.kept = ours.intersection(wanted).sorted()
        return plan
    }

    /// The source, as it appears in an identifier.
    ///
    /// Exhaustive rather than defaulted: a fourth `ReminderKind` upstream must
    /// break this build, not silently collide with an existing tag and start
    /// cancelling somebody else's alerts.
    private static func tag(for kind: ReminderKind) -> String {
        switch kind {
        case .task: "task"
        case .block: "block"
        case .routine: "routine"
        }
    }

    private static func clock(_ epochMs: Timestamp, in timeZone: TimeZone) -> String {
        let date = Date(timeIntervalSince1970: Double(epochMs) / 1000)
        var style = Date.FormatStyle.dateTime.hour().minute()
        style.timeZone = timeZone
        return date.formatted(style)
    }
}

extension NotificationKind {
    /// Its row in Settings → Notifications.
    var settingsTitle: String {
        typealias Kinds = L10n.Account.NotificationKinds
        return switch self {
        case .taskReminder: Kinds.taskReminder
        case .deadline: Kinds.deadline
        case .routineDue: Kinds.routineDue
        case .blockStart: Kinds.blockStart
        case .morningBrief: Kinds.morningBrief
        case .triageNudge: Kinds.triageNudge
        case .eveningPlan: Kinds.eveningPlan
        case .windDown: Kinds.windDown
        case .focusEnd: Kinds.focusEnd
        case .timezoneChanged: Kinds.timezoneChanged
        case .weeklyReview: Kinds.weeklyReview
        case .syncStalled: Kinds.syncStalled
        case .devicePaired: Kinds.devicePaired
        case .deviceRevoked: Kinds.deviceRevoked
        case .recoveryCode: Kinds.recoveryCode
        case .calendarReconnect: Kinds.calendarReconnect
        case .sharedChange: Kinds.sharedChange
        case .placeArrival: Kinds.placeArrival
        }
    }

    /// The command that does in the app what this kind's notification opens,
    /// where the keymap binds one — the key hint its Settings row shows
    /// (`docs/08-features/notifications.md` §Settings → Notifications): the
    /// two briefs (and the nudge, which opens the morning one), Mark Done for
    /// the task kinds, and Focus for a block.
    var keyAction: AppAction? {
        switch self {
        case .morningBrief, .triageNudge: .morningSummary
        case .eveningPlan, .windDown: .endOfDay
        case .taskReminder, .deadline, .routineDue: .markDone
        case .blockStart: .focusMode
        default: nil
        }
    }
}
