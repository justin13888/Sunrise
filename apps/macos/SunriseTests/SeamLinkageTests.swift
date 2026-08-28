import Foundation
import Testing

@testable import Sunrise

/// Proof that the app links the Rust core and can call it.
///
/// Cheap, and the first thing to break when the xcframework, the module map
/// or the generated bindings go wrong — all three of which fail at link time
/// with errors that say nothing about which of them it was.
struct SeamLinkageTests {
    @Test
    func theSharedVocabularyIsCallableFromSwift() {
        #expect(shortDuration(secs: 5400) == "1h30")
        #expect(durationClock(ms: 3_661_000) == "1:01:01")
    }

    /// The overdue boundary is `due_at < start_of_today_local`, not
    /// `due_at < now`. Asserting it here as well as in Rust is deliberate: it
    /// is the fact a client is most likely to reimplement, and this test is
    /// what fails if someone ever does.
    @Test
    func aDeadlineEarlierTodayIsStillDueToday() {
        let tz = "America/New_York"
        let nineAM = TimeValue.zoned(civil: "2026-03-10T09:00:00", tz: tz)
        let fivePM = epochMs(iso: "2026-03-10T17:00:00", tz: tz)

        #expect(todaySection(scheduledAt: nil, dueAt: nineAM, nowMs: fivePM, tz: tz) == .due)
        #expect(
            todaySection(
                scheduledAt: nil,
                dueAt: .allDay(date: "2026-03-09"),
                nowMs: fivePM,
                tz: tz
            ) == .overdue
        )
    }

    /// Epoch milliseconds for a civil time in a named zone.
    private func epochMs(iso: String, tz: String) -> UInt64 {
        var components = DateComponents()
        let parts = iso.split(separator: "T")
        let date = parts[0].split(separator: "-").compactMap { Int($0) }
        let time = parts[1].split(separator: ":").compactMap { Int($0) }
        components.year = date[0]
        components.month = date[1]
        components.day = date[2]
        components.hour = time[0]
        components.minute = time[1]
        components.second = time[2]
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: tz) ?? .gmt
        let instant = calendar.date(from: components) ?? Date(timeIntervalSince1970: 0)
        return UInt64(instant.timeIntervalSince1970 * 1000)
    }
}
