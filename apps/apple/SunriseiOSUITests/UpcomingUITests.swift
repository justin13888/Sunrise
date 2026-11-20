import XCTest

/// Browse reaches Upcoming on the phone, and Upcoming draws a captured task
/// under its day, with the empty days after it still drawn (#351).
///
/// Not a drag. A long-press drag of a row onto a later day's heading reaches
/// no drop target inside the list on the simulator: the row lifts, but no
/// heading or row is ever told a drag is over it, and the drop is refused
/// (#1234). Until that is settled, this asserts what the phone verifiably
/// does. The planner a drop would drive is asserted against a real vault in
/// `UpcomingModelTests`.
final class UpcomingUITests: SunriseUITestCase {
    func testBrowseReachesUpcomingWithACapturedTaskUnderItsDay() {
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

        XCTAssertTrue(
            task("Write report", on: civilDate(daysFromToday: 1)).appears(within: 10),
            "the captured task is under tomorrow"
        )
        // Every day of the span is kept, empty or not, so a task always has
        // somewhere to be moved to.
        let later = civilDate(daysFromToday: 3)
        XCTAssertTrue(
            app.descendants(matching: .any)["upcoming-day-\(later)"].appears(within: 10),
            "an empty day three days on still has its heading"
        )
    }

    private func openUpcoming() {
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        // The cell, not the identifier's own element: the row is a `Label`,
        // whose image and text both inherit the identifier, and the image the
        // query resolves to first is not hittable (`TabShellUITests`).
        let upcoming = app.cells.containing(.staticText, identifier: "sidebar.upcoming").firstMatch
        activate(upcoming, named: "Upcoming in Browse")
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
