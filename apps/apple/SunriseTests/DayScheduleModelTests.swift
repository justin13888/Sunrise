import Foundation
import Testing

@testable import Sunrise

/// The day schedule against a real vault: what the settings screen and the
/// calendar's override menu write comes back from the core as the entry they
/// named, and nothing else moves.
@MainActor
struct DayScheduleModelTests {
    private func model(_ vault: borrowing TestVault) async -> DayScheduleModel {
        let model = DayScheduleModel(bridge: vault.bridge)
        await model.refresh()
        return model
    }

    private let late = DayWindowItem(wake: "07:00:00", sleep: "01:00:00")

    @Test
    func aFreshVaultHasNoSchedule() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        #expect(model.schedule.weekday.isEmpty)
        #expect(model.schedule.monthDay.isEmpty)
        #expect(model.schedule.date.isEmpty)
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    @Test
    func aWeekdaySetAndClearedTouchesOnlyThatWeekday() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.set(.weekday(day: .mo), window: late)
        await model.set(.weekday(day: .fr), window: DayScheduleModel.defaultWindow)
        #expect(model.window(for: .mo) == late, "a sleep past midnight is stored as written")
        #expect(model.window(for: .fr) == DayScheduleModel.defaultWindow)
        #expect(model.window(for: .tu) == nil)

        await model.clear(.weekday(day: .mo))
        #expect(model.window(for: .mo) == nil)
        #expect(model.window(for: .fr) == DayScheduleModel.defaultWindow)
        await vault.bridge.shutdown()
    }

    @Test
    func theLastDayOfTheMonthIsItsOwnEntry() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.set(.monthDay(day: -1), window: late)
        await model.set(.monthDay(day: 1), window: DayScheduleModel.defaultWindow)
        #expect(model.window(forMonthDay: -1) == late)
        #expect(model.window(forMonthDay: 1) == DayScheduleModel.defaultWindow)
        #expect(model.window(forMonthDay: 31) == nil)
        await vault.bridge.shutdown()
    }

    /// The calendar's three actions on one date: its own hours, none at all,
    /// and back to the usual schedule.
    @Test
    func aDateOverrideHoldsHoursOrNoneAndClears() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let christmas: CivilDate = "2026-12-25"

        await model.set(.date(date: christmas), window: late)
        #expect(model.override(on: christmas)?.window == late)

        await model.set(.date(date: christmas), window: nil)
        let none = try #require(model.override(on: christmas))
        #expect(none.window == nil, "no schedule is an override, not an absence")

        await model.clear(.date(date: christmas))
        #expect(model.override(on: christmas) == nil)
        await vault.bridge.shutdown()
    }

    /// `wake == sleep` is the core's to refuse; the model only reports it.
    @Test
    func aWindowWithNoLengthIsRefusedAndNothingIsWritten() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.set(.weekday(day: .we), window: DayWindowItem(wake: "07:00:00", sleep: "07:00:00"))
        #expect(model.errorMessage != nil)
        #expect(model.window(for: .we) == nil)

        model.dismissError()
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    @Test
    func civilValuesCrossInTheFormJiffParses() {
        #expect(DayScheduleModel.minutes("07:30:00") == 450)
        #expect(DayScheduleModel.minutes("23:59:59") == 1439)
        // 2026-12-25T23:30:00Z is still the 25th in New York and already the
        // 26th in Tokyo: a column's date is read in the grid's zone.
        let ms: Int64 = 1_798_241_400_000
        #expect(DayScheduleModel.civilDate(ms: ms, timeZone: "America/New_York") == "2026-12-25")
        #expect(DayScheduleModel.civilDate(ms: ms, timeZone: "Asia/Tokyo") == "2026-12-26")
    }

    @Test
    func aClockTimeOffTheGridStaysPickable() {
        #expect(DayScheduleModel.clockChoices(including: 450).count == 96)
        let choices = DayScheduleModel.clockChoices(including: 410)
        #expect(choices.count == 97)
        #expect(choices.contains(410))
        #expect(choices == choices.sorted())
    }
}
