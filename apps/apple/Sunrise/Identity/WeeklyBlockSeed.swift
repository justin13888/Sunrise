#if DEBUG
import Foundation

/// The weekly block ``UITestHarness/weeklyBlockFlag`` imports (#1209).
///
/// One `.ics` event repeating every week with no end, an hour from
/// ``startHour`` on today's weekday, its first occurrence ``weeksBefore``
/// weeks back. So the week on screen at launch is mid-series: there are
/// weeks before it for *this and following events* to leave alone, and the
/// week before the first has none.
///
/// Floating times, the wall clock wherever the test runs, so the block sits
/// at ``startHour`` on the grid whatever the runner's zone. Early in the day,
/// so the block is on screen without scrolling the grid.
///
/// In its own file only because the harness's is at its length budget:
/// `#if DEBUG` like it, and reached only through it.
enum WeeklyBlockSeed {
    /// The block's title. `SunriseUITests` asserts this literal.
    static let title = "Studio hours"
    /// The event's `UID`, so a second import lands on the same block.
    static let uid = "weekly-block@ui-test.sunrise.invalid"
    /// How many weeks before today the series begins.
    static let weeksBefore = 4
    /// The hour, on the wall clock, each occurrence starts.
    static let startHour = 4

    /// Why the import did not leave one repeating block behind.
    enum SeedError: LocalizedError, Equatable {
        case notImported(written: Int, notices: [String])

        var errorDescription: String? {
            switch self {
            case let .notImported(written, notices):
                "the weekly block was not imported as one series (\(written) written; "
                    + "\(notices.joined(separator: "; ")))"
            }
        }
    }

    /// The first occurrence's day: today's weekday, ``weeksBefore`` weeks back.
    static func firstDay(now: Date, calendar: Calendar) -> Date {
        let today = calendar.startOfDay(for: now)
        return calendar.date(byAdding: .day, value: -7 * weeksBefore, to: today) ?? today
    }

    /// The document imported: one weekly `VEVENT`, CRLF-delimited as RFC 5545
    /// asks.
    static func document(now: Date, calendar: Calendar) -> String {
        let formatter = DateFormatter()
        formatter.calendar = Calendar(identifier: .gregorian)
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = calendar.timeZone
        formatter.dateFormat = "yyyyMMdd"
        let day = formatter.string(from: firstDay(now: now, calendar: calendar))
        let at = { (hour: Int) in String(format: "%02d0000", hour) }
        return [
            "BEGIN:VCALENDAR",
            "VERSION:2.0",
            "PRODID:-//Sunrise//UI test harness//EN",
            "BEGIN:VEVENT",
            "UID:\(uid)",
            "DTSTAMP:20260101T000000Z",
            "DTSTART:\(day)T\(at(startHour))",
            "DTEND:\(day)T\(at(startHour + 1))",
            "RRULE:FREQ=WEEKLY",
            "SUMMARY:\(title)",
            "END:VEVENT",
            "END:VCALENDAR",
            ""
        ].joined(separator: "\r\n")
    }

    /// Import ``document(now:calendar:)`` into `bridge`'s vault, and throw
    /// unless it wrote one block and reported nothing it could not carry: an
    /// RRULE the importer drops is a notice and a one-off block, which would
    /// leave every UI test of the series asserting against a single event.
    ///
    /// One block written, not one *created*: a vault locked and opened again
    /// in the same launch is seeded again, and the import lands on the block
    /// the first one made, by its ``uid``.
    static func seed(into bridge: CoreBridge, now: Date, calendar: Calendar) async throws {
        let report = try await bridge.importIcal(text: document(now: now, calendar: calendar))
        guard report.blocks.count == 1, report.failed == 0, report.notices.isEmpty else {
            throw SeedError.notImported(
                written: report.blocks.count,
                notices: report.notices.map(\.detail)
            )
        }
    }
}
#endif
