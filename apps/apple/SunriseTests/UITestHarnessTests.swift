#if DEBUG
import Testing

@testable import Sunrise

/// The guard that keeps ``UITestHarness/failCaptureFlag`` away from a real
/// vault: the flag refuses captures only in a launch that also names a scratch
/// vault, so a stray argument on a real launch cannot throw captures away.
/// The same guard on ``UITestHarness/weeklyBlockFlag``, and the series its
/// seed writes through the real `.ics` import.
struct UITestHarnessTests {
    private let scratch = [UITestHarness.flag, "/tmp/sunrise-ui-test-scratch"]

    @Test func theFailCaptureFlagWithoutAScratchVaultRefusesNothing() throws {
        let arguments = ["Sunrise", UITestHarness.failCaptureFlag]
        #expect(!UITestHarness.failsQuickCapture(arguments: arguments))
        try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
    }

    @Test func aScratchVaultFlagWithNoPathIsNoScratchVault() throws {
        let arguments = ["Sunrise", UITestHarness.failCaptureFlag, UITestHarness.flag]
        #expect(!UITestHarness.failsQuickCapture(arguments: arguments))
        try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
    }

    @Test func aScratchVaultWithoutTheFlagRefusesNothing() throws {
        let arguments = ["Sunrise"] + scratch
        #expect(!UITestHarness.failsQuickCapture(arguments: arguments))
        try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
    }

    @Test func theFlagWithAScratchVaultRefusesEveryCapture() {
        let arguments = ["Sunrise"] + scratch + [UITestHarness.failCaptureFlag]
        #expect(UITestHarness.failsQuickCapture(arguments: arguments))
        #expect(throws: RefusedByUITestHarness.self) {
            try UITestHarness.refuseQuickCaptureIfAsked(arguments: arguments)
        }
    }

    // MARK: - The weekly block (#1209)

    @Test func theWeeklyBlockFlagWithoutAScratchVaultSeedsNothing() {
        let flag = UITestHarness.weeklyBlockFlag
        #expect(!UITestHarness.seedsWeeklyBlock(arguments: ["Sunrise", flag]))
        #expect(!UITestHarness.seedsWeeklyBlock(arguments: ["Sunrise", flag, UITestHarness.flag]))
        #expect(!UITestHarness.seedsWeeklyBlock(arguments: ["Sunrise"] + scratch))
        #expect(UITestHarness.seedsWeeklyBlock(arguments: ["Sunrise"] + scratch + [flag]))
    }

    /// The document states the series the UI test steps through: weekly, no
    /// end, an hour on the wall clock, beginning ``WeeklyBlockSeed/weeksBefore``
    /// weeks back on today's weekday.
    @Test func theWeeklyBlockDocumentStatesAnOpenWeeklySeriesFourWeeksBack() throws {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = try #require(TimeZone(identifier: "America/Toronto"))
        // Friday 20 November 2026, 15:30 in Toronto.
        let now = try #require(
            calendar.date(from: DateComponents(year: 2026, month: 11, day: 20, hour: 15, minute: 30))
        )
        let lines = WeeklyBlockSeed.document(now: now, calendar: calendar)
            .components(separatedBy: "\r\n")

        #expect(lines.contains("DTSTART:20261023T040000"))
        #expect(lines.contains("DTEND:20261023T050000"))
        #expect(lines.contains("RRULE:FREQ=WEEKLY"))
        #expect(lines.contains("SUMMARY:\(WeeklyBlockSeed.title)"))
        #expect(lines.contains("UID:\(WeeklyBlockSeed.uid)"))
    }

    /// The seed goes through the real import and leaves a series the week
    /// grid expands: an occurrence in the first week and in this one, keyed
    /// as occurrences, and nothing the week before it began. Seeding again
    /// lands on the same block.
    @Test func seedingWritesOneWeeklySeriesTheWeekGridExpands() async throws {
        let vault = try await TestVault()
        let calendar = Calendar.current
        let now = Date()
        try await WeeklyBlockSeed.seed(into: vault.bridge, now: now, calendar: calendar)
        try await WeeklyBlockSeed.seed(into: vault.bridge, now: now, calendar: calendar)

        let first = WeeklyBlockSeed.firstDay(now: now, calendar: calendar)
        func week(of day: Date) async throws -> [BlockGridRow] {
            let noon = UInt64(day.addingTimeInterval(12 * 3600).timeIntervalSince1970 * 1000)
            guard case let .blocks(rows) = try await vault.bridge.query(.weekBlocks(weekMs: noon)) else {
                Issue.record("a week query answers with blocks")
                return []
            }
            return rows
        }

        let thisWeek = try await week(of: calendar.startOfDay(for: now))
        let firstWeek = try await week(of: first)
        let before = try await week(of: first.addingTimeInterval(-7 * 86400))
        for rows in [thisWeek, firstWeek] {
            #expect(rows.count == 1)
            #expect(rows.first?.title == WeeklyBlockSeed.title)
            #expect(rows.first?.occurrenceKey != nil)
            #expect(rows.first?.block.rrule != nil)
        }
        #expect(thisWeek.first?.block.id == firstWeek.first?.block.id)
        #expect(before.isEmpty)
    }
}
#endif
