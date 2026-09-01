import Foundation

/// Everything a reschedule depends on, in one comparable value.
///
/// `NotificationSettings` is already `Equatable` across the seam, so a screen
/// can watch this and reconcile exactly once per real change rather than once
/// per keystroke in the quiet-hours field.
struct ReminderPolicy: Equatable, Sendable {
    /// Whether the user wants reminders from this Mac at all.
    var isEnabled: Bool
    /// The per-device half the core's `ReminderIntents` query takes.
    var settings: NotificationSettings
}

/// The per-device half of the notification configuration.
///
/// `UserDefaults`, exactly like ``AppSettings`` and for the reason
/// `docs/08-features/notifications.md` gives: the lead time, the quiet window
/// and the primary-device flag "are configured per device and never synced, so
/// they arrive with the query rather than living on a replicated entity that
/// would push one device's quiet hours onto another".
///
/// The per-*task* and per-*Stream* lead times are not here. They are entity
/// fields, they describe the work rather than the machine, and they are edited
/// in the task and stream editors.
@MainActor
@Observable
final class NotificationPreferences {
    /// The master switch. Off means nothing is handed to the OS — the same
    /// end state as a non-primary device, reached from the other direction.
    var isEnabled: Bool {
        didSet { defaults.set(isEnabled, forKey: Key.enabled) }
    }

    /// Whether this Mac is the account's primary reminder device.
    ///
    /// `docs/08-features/notifications.md` §Multi-device dedup: the primary
    /// device handles reminders and the others stay silent, and the core
    /// enforces it — `ReminderIntents` returns an empty list before it reads a
    /// row. Defaulting to `true` is deliberate: a fresh install is usually
    /// somebody's only device, and a default of `false` would ship an app
    /// whose reminders silently never fire.
    var isPrimaryDevice: Bool {
        didSet { defaults.set(isPrimaryDevice, forKey: Key.primary) }
    }

    /// The global lead time, in minutes. The floor of the hierarchy; the spec's
    /// default is 0 — "fire at the scheduled time".
    var leadMinutes: Int {
        didSet { defaults.set(leadMinutes, forKey: Key.leadMinutes) }
    }

    var quietHoursEnabled: Bool {
        didSet { defaults.set(quietHoursEnabled, forKey: Key.quietEnabled) }
    }

    /// Local wall clock, as minutes past midnight.
    var quietStartMinutes: Int {
        didSet { defaults.set(quietStartMinutes, forKey: Key.quietStart) }
    }

    var quietEndMinutes: Int {
        didSet { defaults.set(quietEndMinutes, forKey: Key.quietEnd) }
    }

    /// Queue to the end of the window, or drop. The spec's default is queue.
    var quietPolicyIsDrop: Bool {
        didSet { defaults.set(quietPolicyIsDrop, forKey: Key.quietDrop) }
    }

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        isEnabled = defaults.object(forKey: Key.enabled) as? Bool ?? true
        isPrimaryDevice = defaults.object(forKey: Key.primary) as? Bool ?? true
        leadMinutes = defaults.object(forKey: Key.leadMinutes) as? Int ?? 0
        quietHoursEnabled = defaults.object(forKey: Key.quietEnabled) as? Bool ?? false
        quietStartMinutes = defaults.object(forKey: Key.quietStart) as? Int ?? (22 * 60)
        quietEndMinutes = defaults.object(forKey: Key.quietEnd) as? Int ?? (7 * 60)
        quietPolicyIsDrop = defaults.object(forKey: Key.quietDrop) as? Bool ?? false
    }

    /// What to hand `Query::ReminderIntents`, and what to watch for changes.
    var policy: ReminderPolicy {
        ReminderPolicy(isEnabled: isEnabled, settings: settings)
    }

    /// The per-device settings record the seam takes.
    ///
    /// A quiet window whose start equals its end silences nothing (the domain
    /// says so explicitly), so it is sent as "no window" rather than as a
    /// zero-width one the core would then have to ignore.
    var settings: NotificationSettings {
        let window: QuietWindow?
        if quietHoursEnabled, quietStartMinutes != quietEndMinutes {
            window = QuietWindow(
                start: Self.civilTime(minutesPastMidnight: quietStartMinutes),
                end: Self.civilTime(minutesPastMidnight: quietEndMinutes),
                policy: quietPolicyIsDrop ? .drop : .queue
            )
        } else {
            window = nil
        }
        return NotificationSettings(
            defaultLeadS: UInt32(clamping: max(0, leadMinutes) * 60),
            quietHours: window,
            isPrimaryDevice: isPrimaryDevice
        )
    }

    /// `HH:MM:SS`, which is what a `jiff::civil::Time` parses back from.
    ///
    /// The seam carries civil times as their ISO-8601 text — lossless in both
    /// directions — so a malformed string here would come back as a
    /// `BindingError.BadTime` at query time rather than at edit time. Building
    /// it from a clamped minute count is what makes that unreachable.
    static func civilTime(minutesPastMidnight minutes: Int) -> CivilTime {
        let wrapped = ((minutes % 1440) + 1440) % 1440
        return String(format: "%02d:%02d:00", wrapped / 60, wrapped % 60)
    }

    /// The same value as the user reads it, in their own locale.
    static func clockLabel(minutesPastMidnight minutes: Int) -> String {
        let wrapped = ((minutes % 1440) + 1440) % 1440
        var components = DateComponents()
        components.hour = wrapped / 60
        components.minute = wrapped % 60
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .current
        guard let date = calendar.date(from: components) else {
            return civilTime(minutesPastMidnight: wrapped)
        }
        return date.formatted(date: .omitted, time: .shortened)
    }

    /// The choices the quiet-hours pickers offer: every half hour.
    static let clockChoices = Array(stride(from: 0, to: 1440, by: 30))

    /// The lead times the picker offers, in minutes. `0` is the documented
    /// default and is a value rather than an absence.
    static let leadChoices = [0, 5, 10, 15, 30, 60]

    private enum Key {
        static let enabled = "notify.enabled"
        static let primary = "notify.isPrimaryDevice"
        static let leadMinutes = "notify.leadMinutes"
        static let quietEnabled = "notify.quietHours.enabled"
        static let quietStart = "notify.quietHours.startMinutes"
        static let quietEnd = "notify.quietHours.endMinutes"
        static let quietDrop = "notify.quietHours.drop"
    }
}
