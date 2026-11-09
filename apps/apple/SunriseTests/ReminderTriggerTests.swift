import Foundation
import Testing
import UserNotifications

@testable import Sunrise

/// The request ``SystemNotificationCenter`` hands the OS, without the OS.
///
/// Building a `UNNotificationRequest` needs no notification service, so the
/// trigger can be asserted directly.
struct ReminderTriggerTests {
    /// **An armed trigger names an instant, not a wall-clock time.**
    ///
    /// A `zoned` or `instant` reminder keeps its `fireAtMs` across a zone
    /// change, so it keeps its identifier and is never re-armed. Its trigger
    /// has to resolve to the same instant in the new zone, or a 09:00 Toronto
    /// reminder fires at 09:00 London time after a flight.
    ///
    /// The other zone is a `Calendar`'s rather than the process default:
    /// swapping `NSTimeZone.default` would change `TimeZone.current` under
    /// every suite running in parallel with this one.
    @Test
    func anArmedTriggerFiresAtTheSameInstantInAnotherZone() throws {
        // Whole seconds three days out: the trigger matches to the second,
        // and `nextTriggerDate()` only answers for a date still ahead.
        let seconds = (Date().timeIntervalSince1970 + 3 * 24 * 60 * 60).rounded(.down)
        let fireAtMs = Int64(seconds) * 1000
        let planned = PlannedNotification(
            identifier: "\(ReminderPlan.identifierPrefix)task.zoned.\(fireAtMs)",
            title: "Board call",
            body: "",
            category: .task,
            fireAtMs: fireAtMs,
            entity: "zoned",
            link: .task("zoned", .open)
        )
        let fire = Date(timeIntervalSince1970: seconds)

        let request = SystemNotificationCenter.request(for: planned)
        let trigger = try #require(request.trigger as? UNCalendarNotificationTrigger)
        #expect(trigger.nextTriggerDate() == fire)

        let here = TimeZone.current.secondsFromGMT(for: fire)
        let elsewhere = try #require(
            ["Europe/London", "Asia/Tokyo"]
                .compactMap(TimeZone.init(identifier:))
                .first { $0.secondsFromGMT(for: fire) != here }
        )
        // `date(from:)` rather than `nextDate(after:matching:)`: the latter
        // ignores the components' own zone, the former honours it, as the
        // trigger does.
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = elsewhere
        #expect(
            calendar.date(from: trigger.dateComponents) == fire,
            "the trigger moved with the zone to \(elsewhere.identifier)"
        )
    }
}
