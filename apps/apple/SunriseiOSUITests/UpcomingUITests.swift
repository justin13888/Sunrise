import XCTest

/// Browse reaches Upcoming on the phone, Upcoming draws a captured task under
/// its day with the empty days after it still drawn (#351), and a row dragged
/// onto a later day's heading is rescheduled there in one undo step (#1234).
///
/// The drag is the regression test for Upcoming being drawn outside a `List`
/// on iOS (`TaskRows.container`): inside one, the same drag lifts the row and
/// is refused, because the `List` offers its own rows' drags to no drop
/// target inside it. XCUITest's drag does reach drop targets, as the
/// row-outside-the-`List` case showed while that was being established. The
/// hover preview the drag shows on the way is not observable from here,
/// since XCUITest's drag is one blocking call (`PlannedDragUITests`); it is
/// asserted against a real vault in `UpcomingModelTests`.
final class UpcomingUITests: SunriseUITestCase {
    func testDraggingARowOntoALaterDayReschedulesItAndOneUndoReturnsIt() {
        createVault()
        capture("Write report ^tomorrow", listedHere: false)
        dismissKeyboard()
        openUpcoming()

        let tomorrow = civilDate(daysFromToday: 1)
        let later = civilDate(daysFromToday: 3)
        let row = task("Write report", on: tomorrow)
        XCTAssertTrue(row.appears(within: 10), "the captured task is under tomorrow before the drag")
        let heading = app.descendants(matching: .any)["upcoming-day-\(later)"]
        XCTAssertTrue(heading.appears(within: 10), "the later day's heading is on screen to drop on")

        drag(row, onto: heading)

        XCTAssertTrue(
            task("Write report", on: later).appears(within: 10),
            "the dropped task is under the day it was dropped on"
        )
        XCTAssertFalse(
            task("Write report", on: tomorrow).exists,
            "and no longer under tomorrow"
        )

        // Browse's More menu, reached from Browse's root: the tab pops the
        // pushed Upcoming on the way.
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        activate(app.buttons["more"], named: "Browse's More menu")
        let undo = app.buttons["Undo reschedule ‘Write report’"]
        activate(undo, named: "the drop's Undo item")

        openUpcoming()
        XCTAssertTrue(
            task("Write report", on: tomorrow).appears(within: 10),
            "one undo put the task back under tomorrow"
        )
        XCTAssertFalse(
            task("Write report", on: later).exists,
            "and took it off the day it was dropped on"
        )
    }

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

    /// Long-press `row` until it lifts, then drag it onto `target` and let go.
    ///
    /// The press is the one timing this depends on, because the row has a
    /// context menu as well as a drag, and both start on a long press. On the
    /// iPhone 17 Pro simulator, measured for #1234: a 0.3 s press lifts
    /// nothing, 0.4 s and 0.5 s lifted the row on every try (5 of 5 and 8 of
    /// 8), 0.6 s on 4 of 5, and 0.8 s or longer opens the menu every time. So
    /// 0.5 s, the middle of that window, first.
    ///
    /// That window was measured on a local simulator, and CI's virtualized
    /// one is slower: there the 0.5 s press left the row under its day
    /// (#1312), with no menu open and, in the run's screen recording, no
    /// lifted row drawn at any point of the drag, so the press lifted
    /// nothing. So every attempt that leaves `row` under its original day is
    /// pressed again, whether the press lifted nothing, the menu took it, or
    /// the drag never reached the target. The retries press longer first,
    /// since a press too short for CI's simulator is the case seen there:
    /// 0.7 s, unmeasured, sits between the window and the 0.8 s that opens
    /// the menu locally, and a press the menu takes costs only a dismissal;
    /// 0.4 s comes last, for a run that is not slower. A press the menu
    /// takes writes nothing and leaves the menu open, so the menu is
    /// dismissed before the next press. Each attempt's outcome is its own
    /// activity in the result bundle, so a failure shows which of those CI
    /// hit.
    ///
    /// A refused drop, which is what this test exists to catch, also leaves
    /// the row where it was. It is pressed again like the rest, and since a
    /// refusal refuses every attempt, the last one still leaves the row under
    /// its original day and the caller's assertion fails.
    private func drag(_ row: XCUIElement, onto target: XCUIElement) {
        let presses: [TimeInterval] = [0.5, 0.6, 0.7, 0.4]
        let menu = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Defer a week")).firstMatch
        for (attempt, press) in presses.enumerated() {
            // A write that landed after the last attempt's wait has moved the
            // row already; pressing again would drop it a second time.
            guard row.exists else { return }
            let moved = XCTContext.runActivity(
                named: "Drag attempt \(attempt + 1) of \(presses.count), pressing \(press) s"
            ) { _ -> Bool in
                row.press(
                    forDuration: press, thenDragTo: target, withVelocity: .slow, thenHoldForDuration: 1.0
                )
                if menu.exists {
                    XCTContext.runActivity(named: "The context menu took the press; dismissing it") { _ in
                        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.05)).tap()
                        XCTAssertTrue(
                            waitUntil(timeout: 5) { !menu.exists },
                            "the context menu a press opened is dismissed before pressing again"
                        )
                    }
                    return false
                }
                // The drop writes through the core before the row moves, so
                // give it the 10 s the caller allows the row to reach the
                // target before calling it missed: pressing again while a
                // slow write is still landing would press a row that is
                // leaving, or drop the task a second time.
                let left = waitUntil(timeout: 10) { !row.exists }
                XCTContext.runActivity(
                    named: left ? "The row left its original day" : "The row is still under its original day"
                ) { _ in }
                return left
            }
            if moved { return }
        }
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
