import Foundation

/// Which set of action buttons a reminder carries.
///
/// A Block cannot be completed and cannot be deferred — the commands do not
/// exist — so it gets its own category rather than three buttons that would
/// have to fail. `docs/08-features/notifications.md` §Action buttons says
/// "where the OS supports actions", and this is the same reservation one level
/// down: where the *entity* supports them.
enum ReminderCategory: String, Equatable, Sendable, CaseIterable {
    /// A Task (or a materialized routine occurrence, which is a Task by the
    /// time it is due): done, defer, snooze, open.
    case task = "sunrise.reminder.task"
    /// A time block: open only.
    case block = "sunrise.reminder.block"

    /// The snooze spans this category offers, in button order.
    ///
    /// The two the spec names, and no third: a notification with five buttons
    /// is a menu, and macOS collapses everything past the second into one
    /// anyway.
    var snoozeSpans: [SnoozeSpan] {
        switch self {
        case .task: [.oneHour, .tomorrow]
        case .block: []
        }
    }

    var offersCompletion: Bool { self == .task }
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
    /// The entity a button acts on.
    let entity: EntityRef
    /// What tapping the body opens.
    let link: DeepLink
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
    static let quietHoursNote = "Held for quiet hours"

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
                : .task(reminder.entity, .open)
        )
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
        if reminder.kind == .block { parts.append("Time block starting") }
        if let from = reminder.deferredFrom {
            parts.append("\(quietHoursNote) · due \(clock(from, in: timeZone))")
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
