import XCTest

/// A task dragged from one day of Upcoming to a later one lands under that
/// day, and one undo puts it back (#351).
///
/// On the phone because `SunriseUITests` is `skipped: true` in the Mac
/// scheme, so a Mac-side test would be a test nothing runs (see
/// `PlannedDragUITests`).
///
/// What this can and cannot see. XCUITest's drag is one blocking call, so the
/// plan a hover previews on the day's heading cannot be queried between the
/// press and the release. What is observable is what the drop committed, and
/// the drop commits exactly the diff the hover drew (`UpcomingPlanner.drop`).
/// The preview itself, and that a preview writes nothing, are asserted
/// against a real vault in `UpcomingModelTests`.
final class UpcomingDragUITests: SunriseUITestCase {
    func testDraggingATaskToALaterDayMovesItAndUndoReturnsIt() {
        createVault()

        // Typed into Today, where the app opens: Upcoming has no capture bar.
        // Not `listedHere`: Today does not list a task planned for tomorrow,
        // so it is found under its day in Upcoming below instead.
        capture("Write report ^tomorrow", listedHere: false)
        // The capture bar keeps focus after Add, and the software keyboard it
        // holds up covers the tab bar (`TabShellUITests`), so put it away
        // before reaching for Browse.
        dismissKeyboard()
        openUpcoming()
        let tomorrow = civilDate(daysFromToday: 1)
        let later = civilDate(daysFromToday: 3)
        let row = task("Write report", on: tomorrow)
        XCTAssertTrue(row.appears(within: 10), "the captured task is under tomorrow")

        let heading = app.descendants(matching: .any)["upcoming-day-\(later)"]
        XCTAssertTrue(heading.appears(within: 10), "the day three days on has a heading to drop on")
        row.press(forDuration: 1.0, thenDragTo: heading, withVelocity: .slow, thenHoldForDuration: 1.0)

        XCTAssertTrue(
            task("Write report", on: later).appears(within: 10),
            "the drop rescheduled the task onto the day it was dropped on"
        )

        // Undo lives in Browse's More menu, on Browse's root.
        activate(app.navigationBars.buttons.element(boundBy: 0), named: "the back button to Browse")
        activate(app.buttons["more"], named: "Browse's More menu")
        let undo = app.buttons["Undo reschedule ‘Write report’"]
        activate(undo, named: "the Undo item naming the reschedule")

        openUpcoming()
        XCTAssertTrue(
            task("Write report", on: tomorrow).appears(within: 10),
            "one undo put the task back on the day it came from"
        )
    }

    private func openUpcoming() {
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        activate(app.descendants(matching: .any)["sidebar.upcoming"], named: "Upcoming in Browse")
        XCTAssertTrue(
            app.descendants(matching: .any)["upcoming-span"].appears(within: 10),
            "Upcoming is on screen"
        )
    }

    /// The task titled `title` drawn under `date`'s heading.
    private func task(_ title: String, on date: String) -> XCUIElement {
        app.staticTexts
            .matching(identifier: "upcoming-row-day-\(date)")
            .matching(NSPredicate(format: "label == %@", title))
            .firstMatch
    }

    /// The civil date `days` from today in the device's zone, as the core
    /// writes it. The test runs on the simulator the app does, in its zone.
    private func civilDate(daysFromToday days: Int) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .current
        let day = calendar.date(byAdding: .day, value: days, to: Date()) ?? Date()
        let formatter = DateFormatter()
        formatter.calendar = calendar
        formatter.timeZone = .current
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyy-MM-dd"
        return formatter.string(from: day)
    }
}
