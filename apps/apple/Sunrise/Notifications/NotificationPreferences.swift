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
/// The core's preferences, read and written through the open vault (#489):
/// `notifications.enabled`, `notifications.task_reminder.lead_s`,
/// `notifications.quiet_hours.window` and `.policy`, each written to this
/// device's overlay, and the account's `notifications.primary_device`
/// register. ``bind(to:)`` loads them when a vault opens; until then, and in
/// a test that never binds, the values here are the spec's defaults and an
/// edit stays in memory.
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
        didSet { save(.enabled) }
    }

    /// Whether this device is the account's primary reminder device.
    ///
    /// `docs/08-features/notifications.md` §Multi-device dedup: the primary
    /// device handles reminders and the others stay silent, and the core
    /// enforces it — `ReminderIntents` returns an empty list before it reads a
    /// row. An unset register reads as primary on every device: a fresh
    /// install is usually somebody's only device, and the other reading would
    /// ship an app whose reminders silently never fire.
    ///
    /// Turning it on claims the register for this device. Turning it off
    /// writes nothing — the register names a device, never "none", so this
    /// device stops being primary only when another one claims it — and the
    /// register is read back, which turns it on again.
    var isPrimaryDevice: Bool {
        didSet {
            guard isPrimaryDevice != oldValue else { return }
            save(isPrimaryDevice ? .primary : .rereadPrimary)
        }
    }

    /// The global lead time, in minutes. The floor of the hierarchy; the spec's
    /// default is 0 — "fire at the scheduled time".
    var leadMinutes: Int {
        didSet { save(.lead) }
    }

    var quietHoursEnabled: Bool {
        didSet { save(.quiet) }
    }

    /// Local wall clock, as minutes past midnight.
    var quietStartMinutes: Int {
        didSet { save(.quiet) }
    }

    var quietEndMinutes: Int {
        didSet { save(.quiet) }
    }

    /// Queue to the end of the window, or drop. The spec's default is queue.
    var quietPolicyIsDrop: Bool {
        didSet { save(.quiet) }
    }

    /// Why the last read or write through the core failed, or `nil`.
    private(set) var errorMessage: String?

    /// Holds the posted-notification ledger, and the values an earlier build
    /// kept, until ``bind(to:)`` moves them into the core.
    private let defaults: UserDefaults
    /// What `migrate_legacy_defaults` writes client settings into.
    private let directory: URL
    /// The open vault the preferences are read from and written to.
    @ObservationIgnored private var bridge: CoreBridge?
    /// Set while ``bind(to:)`` loads, so loading writes nothing back.
    @ObservationIgnored private var loading = false
    /// The write in flight; the next one, and ``saved()``, wait for it.
    @ObservationIgnored private var writing: _Concurrency.Task<Void, Never>?

    /// Starts from the values an earlier build left in `defaults`, where there
    /// are any, so a device that has not opened its vault since still reads
    /// its own; ``bind(to:)`` replaces them with the core's.
    init(defaults: UserDefaults = .standard, directory: URL = AppSettings.standardDirectory) {
        self.defaults = defaults
        self.directory = directory
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

    /// What this device has already posted for the vault `device` belongs
    /// to: the core's `NotificationLedger`, as JSON, kept here because it is
    /// device state and never syncs. One per vault, keyed by this install's
    /// device id in it, so switching vaults does not post one vault's history
    /// as news in the other.
    func ledger(for device: String) -> String? {
        defaults.string(forKey: Key.ledger(device))
    }

    /// Keep the ledger the last re-plan returned, once its alerts are posted.
    func setLedger(_ json: String, for device: String) {
        defaults.set(json, forKey: Key.ledger(device))
    }

    /// The choices the quiet-hours pickers offer: every half hour.
    static let clockChoices = Array(stride(from: 0, to: 1440, by: 30))

    /// The lead times the picker offers, in minutes. `0` is the documented
    /// default and is a value rather than an absence.
    static let leadChoices = [0, 5, 10, 15, 30, 60]

    /// The `UserDefaults` keys an earlier build kept these under, and the
    /// ledger's, which stays there.
    enum Key {
        static let enabled = "notify.enabled"
        static let primary = "notify.isPrimaryDevice"
        static let leadMinutes = "notify.leadMinutes"
        static let quietEnabled = "notify.quietHours.enabled"
        static let quietStart = "notify.quietHours.startMinutes"
        static let quietEnd = "notify.quietHours.endMinutes"
        static let quietDrop = "notify.quietHours.drop"
        static func ledger(_ device: String) -> String { "notify.ledger.\(device)" }
        /// The old keys that held a `Bool`, and those that held an `Int`.
        static let legacyFlags = [enabled, primary, quietEnabled, quietDrop]
        static let legacyCounts = [leadMinutes, quietStart, quietEnd]
    }

    /// The core's preference keys these are read from and written to.
    enum Preference {
        static let enabled = "notifications.enabled"
        static let leadSeconds = "notifications.task_reminder.lead_s"
        static let quietWindow = "notifications.quiet_hours.window"
        static let quietPolicy = "notifications.quiet_hours.policy"
    }

    /// Which setting an edit changed.
    enum Edit { case enabled, primary, rereadPrimary, lead, quiet }

    // MARK: - Through the core

    /// Bind to the vault that just opened: move what an earlier build left in
    /// `UserDefaults` into it, once, then read the preferences back.
    ///
    /// A migration the core refuses — a vault write refused while another
    /// device has not advertised `preferences.entity`, say — keeps the old
    /// keys, and the next bind tries again rather than dropping them.
    func bind(to bridge: CoreBridge) async {
        self.bridge = bridge
        var claim = true
        do {
            if let report = try await migrateLegacyValues(into: bridge) {
                // The old switch said another device is primary: leave the
                // register to that one rather than claiming it here.
                claim = report.claimPrimary
            }
        } catch {
            errorMessage = error.localizedDescription
        }
        await reload(from: bridge, claimingIfUnset: claim)
    }

    /// Stop writing to the vault: it is closing.
    func unbind() {
        bridge = nil
    }

    /// Wait for the writes in flight to land.
    func saved() async {
        await writing?.value
    }

    /// Read the preferences back from `bridge`, claiming the primary register
    /// when it is unset and reminders are on here — the claim the core's
    /// `ReminderPolicyItem.claimPrimary` asks for.
    private func reload(from bridge: CoreBridge, claimingIfUnset claim: Bool) async {
        do {
            var read = try await bridge.reminderPolicy()
            if claim, read.claimPrimary {
                try await bridge.claimPrimaryReminderDevice()
                read = try await bridge.reminderPolicy()
            }
            // A vault switched to while this was reading is the one to show.
            guard self.bridge === bridge else { return }
            load(read)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func load(_ read: ReminderPolicyItem) {
        loading = true
        defer { loading = false }
        isEnabled = read.enabled
        isPrimaryDevice = read.settings.isPrimaryDevice
        leadMinutes = Int(read.settings.defaultLeadS / 60)
        if let window = read.settings.quietHours {
            quietHoursEnabled = true
            quietStartMinutes = Self.minutes(window.start)
            quietEndMinutes = Self.minutes(window.end)
            quietPolicyIsDrop = window.policy == .drop
        } else {
            quietHoursEnabled = false
        }
    }

    /// Write one edit through the core, after the writes before it.
    private func save(_ edit: Edit) {
        guard !loading, let bridge else { return }
        let commands = commands(for: edit)
        let previous = writing
        writing = _Concurrency.Task { @MainActor [weak self] in
            await previous?.value
            do {
                switch edit {
                case .primary: try await bridge.claimPrimaryReminderDevice()
                case .rereadPrimary: await self?.reload(from: bridge, claimingIfUnset: false)
                default: break
                }
                for command in commands { _ = try await bridge.submit(command) }
                self?.errorMessage = nil
            } catch {
                self?.errorMessage = error.localizedDescription
            }
        }
    }

    /// The writes an edit makes, built from the values as they are now. Each
    /// goes to this device's overlay: the settings are this device's, and a
    /// value in the vault would push one device's quiet hours onto another.
    /// Quiet hours off, or a zero-width window, clears this device's window,
    /// so the account's — if it has one — applies again.
    func commands(for edit: Edit) -> [CoreCommand] {
        switch edit {
        case .enabled:
            return [.setPreference(key: Preference.enabled, value: .bool(value: isEnabled), target: .device)]
        case .primary, .rereadPrimary:
            return []
        case .lead:
            let seconds = UInt64(max(0, leadMinutes)) * 60
            let lead = PreferenceValue.uint(value: seconds)
            return [.setPreference(key: Preference.leadSeconds, value: lead, target: .device)]
        case .quiet:
            guard quietHoursEnabled, quietStartMinutes != quietEndMinutes else {
                return [.clearPreference(key: Preference.quietWindow, target: .device)]
            }
            let window = PreferenceValue.timeWindow(
                start: Self.civilTime(minutesPastMidnight: quietStartMinutes),
                end: Self.civilTime(minutesPastMidnight: quietEndMinutes)
            )
            let policy = PreferenceValue.text(value: quietPolicyIsDrop ? "drop" : "queue")
            return [
                .setPreference(key: Preference.quietWindow, value: window, target: .device),
                .setPreference(key: Preference.quietPolicy, value: policy, target: .device)
            ]
        }
    }

    /// The values an earlier build left in `UserDefaults`, as the core's
    /// migration takes them.
    func legacyValues() -> [LegacyDefault] {
        let flags = Key.legacyFlags.compactMap { key in
            (defaults.object(forKey: key) as? Bool).map { LegacyDefault(key: key, value: .bool(value: $0)) }
        }
        let counts = Key.legacyCounts.compactMap { key in
            (defaults.object(forKey: key) as? Int).map {
                LegacyDefault(key: key, value: .int(value: Int64($0)))
            }
        }
        return flags + counts
    }

    /// Hand the old values to the core's `migrate_legacy_defaults`, and delete
    /// each key it took once it has written them. `nil` when there were none.
    private func migrateLegacyValues(into bridge: CoreBridge) async throws -> LegacyMigrationReport? {
        let old = legacyValues()
        guard !old.isEmpty else { return nil }
        let report = try await bridge.migrateLegacyDefaults(old, directory: directory)
        for value in old where !report.unmapped.contains(value.key) {
            defaults.removeObject(forKey: value.key)
        }
        return report
    }

    /// Minutes past midnight of a civil time's `HH:MM[:SS]` text.
    static func minutes(_ time: CivilTime) -> Int {
        let parts = time.split(separator: ":").compactMap { Int($0) }
        guard parts.count >= 2 else { return 0 }
        return parts[0] * 60 + parts[1]
    }
}
