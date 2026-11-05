import SwiftUI

/// Settings → Day schedule: the weekday defaults and the day-of-month
/// defaults (`docs/02-domain/day-schedule.md`).
///
/// Every control writes one entry at once, so there is no Save: the entry is
/// a vault preference of its own, and a screen that batched its edits would
/// overwrite whatever another device changed in the meantime.
struct DayScheduleSections: View {
    let model: DayScheduleModel

    var body: some View {
        Section(L10n.DaySchedule.title) {
            ForEach(DayScheduleModel.weekdays, id: \.self) { day in
                weekdayRow(day)
            }
            Text(L10n.DaySchedule.caption)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(L10n.DaySchedule.sleepCaption)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        Section(L10n.DaySchedule.monthDays) {
            ForEach(model.schedule.monthDay, id: \.day) { entry in
                HStack {
                    DayWindowPickers(title: Self.monthDayTitle(entry.day), window: entry.window) {
                        await model.set(.monthDay(day: entry.day), window: $0)
                    }
                    Button(L10n.Action.remove, systemImage: "minus.circle") {
                        Task { await model.clear(.monthDay(day: entry.day)) }
                    }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                }
            }
            addMonthDayMenu
            Text(L10n.DaySchedule.monthDaysCaption)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    /// A weekday: a switch, and its two times once it is on. Off clears the
    /// entry, which is "no default for this weekday", not a window of zero.
    @ViewBuilder
    private func weekdayRow(_ day: Weekday) -> some View {
        let window = model.window(for: day)
        let isOn = Binding(
            get: { window != nil },
            set: { on in
                Task {
                    if on {
                        await model.set(.weekday(day: day), window: DayScheduleModel.defaultWindow)
                    } else {
                        await model.clear(.weekday(day: day))
                    }
                }
            }
        )
        if let window {
            HStack {
                Toggle(Self.weekdayName(day), isOn: isOn)
                    .fixedSize()
                DayWindowPickers(title: nil, window: window) {
                    await model.set(.weekday(day: day), window: $0)
                }
            }
        } else {
            Toggle(Self.weekdayName(day), isOn: isOn)
        }
    }

    private var addMonthDayMenu: some View {
        let taken = Set(model.schedule.monthDay.map(\.day))
        return Menu(L10n.DaySchedule.addMonthDay) {
            ForEach(DayScheduleModel.monthDays.filter { !taken.contains($0) }, id: \.self) { day in
                Button(Self.monthDayTitle(day)) {
                    Task { await model.set(.monthDay(day: day), window: DayScheduleModel.defaultWindow) }
                }
            }
        }
        .disabled(taken.count == DayScheduleModel.monthDays.count)
    }

    /// The weekday's name in the user's language, standalone ("Monday").
    static func weekdayName(_ day: Weekday) -> String {
        let symbols = Calendar.current.standaloneWeekdaySymbols
        let index = CalendarModel.firstWeekday(day) - 1
        return symbols.indices.contains(index) ? symbols[index] : ""
    }

    static func monthDayTitle(_ day: Int8) -> String {
        day == -1 ? L10n.DaySchedule.lastDay : L10n.DaySchedule.monthDay(day: String(day))
    }
}

/// A wake picker and a sleep picker over one window, each choice written at
/// once.
struct DayWindowPickers: View {
    let title: String?
    let window: DayWindowItem
    let save: (DayWindowItem) async -> Void

    var body: some View {
        HStack {
            if let title {
                Text(title)
                Spacer()
            }
            ClockPicker(label: L10n.DaySchedule.wake, minutes: DayScheduleModel.minutes(window.wake)) {
                await save(DayWindowItem(wake: DayScheduleModel.civilTime($0), sleep: window.sleep))
            }
            ClockPicker(label: L10n.DaySchedule.sleep, minutes: DayScheduleModel.minutes(window.sleep)) {
                await save(DayWindowItem(wake: window.wake, sleep: DayScheduleModel.civilTime($0)))
            }
        }
    }
}

/// One clock time, every quarter hour, in the user's own clock format.
struct ClockPicker: View {
    let label: String
    let minutes: Int
    let save: (Int) async -> Void

    var body: some View {
        Picker(
            label,
            selection: Binding(get: { minutes }, set: { new in Task { await save(new) } })
        ) {
            ForEach(DayScheduleModel.clockChoices(including: minutes), id: \.self) {
                Text(NotificationPreferences.clockLabel(minutesPastMidnight: $0)).tag($0)
            }
        }
        .fixedSize()
    }
}

/// The calendar's per-date override: for each day on the grid, give it its
/// own hours, give it none, or send it back to its usual schedule.
///
/// Keyed by the column's civil date, because a calendar column is a civil
/// date (`day-schedule.md` §In the tree: the grid stays civil).
struct DateOverrideMenu: View {
    let schedule: DayScheduleModel
    let calendar: CalendarModel

    @State private var editing: DateHoursEdit?

    var body: some View {
        Menu(L10n.DaySchedule.title, systemImage: "bed.double") {
            if calendar.span.dayCount == 1 {
                items(dayOffset: 0)
            } else {
                ForEach(0..<calendar.span.dayCount, id: \.self) { offset in
                    Menu(dayTitle(offset)) { items(dayOffset: offset) }
                }
            }
        }
        .sheet(item: $editing) { edit in
            DateHoursSheet(edit: edit) { window in
                await schedule.set(.date(date: edit.date), window: window)
            }
        }
    }

    @ViewBuilder
    private func items(dayOffset: Int) -> some View {
        let date = civilDate(dayOffset)
        let current = schedule.dateOverride(on: date)
        Text(stateTitle(current))
        Button(L10n.DaySchedule.dateHours) {
            editing = DateHoursEdit(
                date: date,
                title: dayTitle(dayOffset),
                window: current?.window ?? DayScheduleModel.defaultWindow
            )
        }
        Button(L10n.DaySchedule.dateNone) {
            Task { await schedule.set(.date(date: date), window: nil) }
        }
        if current != nil {
            Button(L10n.DaySchedule.dateClear) {
                Task { await schedule.clear(.date(date: date)) }
            }
        }
    }

    private func civilDate(_ offset: Int) -> CivilDate {
        DayScheduleModel.civilDate(ms: calendar.dayStartMs(offset: offset), timeZone: calendar.timeZone)
    }

    private func dayTitle(_ offset: Int) -> String {
        let formatter = DateFormatter()
        formatter.timeZone = TimeZone(identifier: calendar.timeZone)
        formatter.setLocalizedDateFormatFromTemplate("EEEEdMMMM")
        let start = Double(calendar.dayStartMs(offset: offset)) / 1000
        return formatter.string(from: Date(timeIntervalSince1970: start))
    }

    private func stateTitle(_ current: DateOverrideItem?) -> String {
        guard let current else { return L10n.DaySchedule.dateUsual }
        guard let window = current.window else { return L10n.DaySchedule.dateNoSchedule }
        return L10n.DaySchedule.dateWindow(
            wake: DayScheduleModel.clockLabel(window.wake),
            sleep: DayScheduleModel.clockLabel(window.sleep)
        )
    }
}

/// Wake and sleep for one date, written on Save.
struct DateHoursSheet: View {
    let edit: DateHoursEdit
    let save: (DayWindowItem) async -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var wake = 0
    @State private var sleep = 0

    var body: some View {
        Form {
            Section(L10n.DaySchedule.dateSheetTitle(date: edit.title)) {
                Picker(L10n.DaySchedule.wake, selection: $wake) { choices(including: wake) }
                Picker(L10n.DaySchedule.sleep, selection: $sleep) { choices(including: sleep) }
                Text(L10n.DaySchedule.sleepCaption)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button(L10n.Action.cancel) { dismiss() }
            }
            ToolbarItem(placement: .confirmationAction) {
                Button(L10n.Action.save) {
                    Task {
                        await save(DayWindowItem(
                            wake: DayScheduleModel.civilTime(wake),
                            sleep: DayScheduleModel.civilTime(sleep)
                        ))
                        dismiss()
                    }
                }
            }
        }
        .macSheetFrame(width: 380)
        .onAppear {
            wake = DayScheduleModel.minutes(edit.window.wake)
            sleep = DayScheduleModel.minutes(edit.window.sleep)
        }
    }

    private func choices(including current: Int) -> some View {
        ForEach(DayScheduleModel.clockChoices(including: current), id: \.self) {
            Text(NotificationPreferences.clockLabel(minutesPastMidnight: $0)).tag($0)
        }
    }
}
