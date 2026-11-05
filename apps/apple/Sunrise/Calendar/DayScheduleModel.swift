import Foundation

/// The day schedule (`docs/02-domain/day-schedule.md`): when the user usually
/// wakes and sleeps, per weekday, per day of the month and per date.
///
/// A seam and nothing more. The schedule is read with `CoreQuery.daySchedule`
/// and every entry is its own vault preference, written with
/// `SetPreference` / `ClearPreference` under the key `dayScheduleKey` names,
/// so two devices editing two entries both keep their edit. Which entry wins
/// for a date, what a sleep before wake means and whether `wake == sleep` is
/// allowed are the core's: a refused write comes back as ``errorMessage``.
@MainActor
@Observable
final class DayScheduleModel {
    /// Every entry, as the core last returned it.
    private(set) var schedule = DayScheduleItem(weekday: [], monthDay: [], date: [])
    private(set) var errorMessage: String?

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// The weekdays in the order the settings list them, Monday first, as
    /// the core lists its weekday entries.
    static let weekdays: [Weekday] = [.mo, .tu, .we, .th, .fr, .sa, .su]

    /// The days a day-of-month default can name: `1`…`31`, then `-1`, the
    /// last day of the month.
    static let monthDays: [Int8] = Array(1...31) + [-1]

    /// The window a new entry starts with: 07:00 to 23:00.
    static let defaultWindow = DayWindowItem(wake: "07:00:00", sleep: "23:00:00")

    // MARK: - Reading

    func refresh() async {
        let now = await bridge.nowMs()
        do {
            guard case let .daySchedule(day) = try await bridge.query(.daySchedule(atMs: now)) else {
                return
            }
            schedule = day.schedule
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Read once, then again on every change batch: an entry another device
    /// set arrives by sync, and a screen that kept its own copy would show
    /// the stale one.
    func follow() async {
        await refresh()
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    func window(for day: Weekday) -> DayWindowItem? {
        schedule.weekday.first { $0.day == day }?.window
    }

    func window(forMonthDay day: Int8) -> DayWindowItem? {
        schedule.monthDay.first { $0.day == day }?.window
    }

    /// The date's own override, if it has one. Absent means the date follows
    /// its day of the month and weekday.
    func dateOverride(on date: CivilDate) -> DateOverrideItem? {
        schedule.date.first { $0.date == date }
    }

    // MARK: - Writing

    /// Set one entry. `nil` writes "no schedule", which only a date can hold.
    func set(_ entry: DayScheduleEntryItem, window: DayWindowItem?) async {
        let value: PreferenceValue = window.map { .dayWindow(wake: $0.wake, sleep: $0.sleep) }
            ?? .noSchedule
        await write(entry) { key in .setPreference(key: key, value: value, target: .vault) }
    }

    /// Remove one entry, and only that one: the level below it applies again.
    func clear(_ entry: DayScheduleEntryItem) async {
        await write(entry) { key in .clearPreference(key: key, target: .vault) }
    }

    func dismissError() { errorMessage = nil }

    private func write(_ entry: DayScheduleEntryItem, _ command: (String) -> CoreCommand) async {
        do {
            let key = try dayScheduleKey(entry: entry)
            _ = try await bridge.submit(command(key))
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    // MARK: - Civil values

    /// `HH:MM:SS` minutes past midnight, the minute a picker shows.
    static func minutes(_ time: CivilTime) -> Int {
        let parts = time.split(separator: ":").prefix(2).compactMap { Int($0) }
        guard parts.count == 2 else { return 0 }
        return parts[0] * 60 + parts[1]
    }

    /// Minutes past midnight as `HH:MM:SS`; quiet hours' conversion, so the
    /// two pickers cannot disagree about it.
    static func civilTime(_ minutes: Int) -> CivilTime {
        NotificationPreferences.civilTime(minutesPastMidnight: minutes)
    }

    /// A civil time as the user reads one, in their own clock format.
    static func clockLabel(_ time: CivilTime) -> String {
        NotificationPreferences.clockLabel(minutesPastMidnight: minutes(time))
    }

    /// The civil date the instant `ms` falls on in `timeZone`, as `jiff`
    /// parses it back: `2026-12-25`. A calendar column is a civil date, so this
    /// is the date its override is keyed by.
    static func civilDate(ms: Int64, timeZone: String) -> CivilDate {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: timeZone) ?? .current
        let parts = calendar.dateComponents(
            [.year, .month, .day],
            from: Date(timeIntervalSince1970: Double(ms) / 1000)
        )
        return String(format: "%04d-%02d-%02d", parts.year ?? 1970, parts.month ?? 1, parts.day ?? 1)
    }

    /// The clock times a picker offers: every quarter hour, plus `current`
    /// when another device set one off that grid, so the picker never shows
    /// a blank.
    static func clockChoices(including current: Int) -> [Int] {
        let grid = Array(stride(from: 0, to: 1440, by: 15))
        return grid.contains(current) ? grid : (grid + [current]).sorted()
    }
}

/// The date whose hours the calendar's sheet sets, and what it starts from.
struct DateHoursEdit: Identifiable {
    let date: CivilDate
    /// The date as the menu wrote it out, for the sheet's heading.
    let title: String
    let window: DayWindowItem
    var id: CivilDate { date }
}
