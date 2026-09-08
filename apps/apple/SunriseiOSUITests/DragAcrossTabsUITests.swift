import XCTest

/// Whether a drag started on a task row can reach the calendar grid.
///
/// `docs/07-clients/parity-matrix.md` grades *Task → Calendar block* **No** on
/// iOS, and one link in that argument was never established: the source
/// (`TaskRowView.swift:74` `.draggable`) and the target
/// (`CalendarView.swift:221` `.dropDestination`) both ship unguarded, and they
/// are never on screen together — but iOS drag sessions outlive the view that
/// started them, so *something* holding the item across a tab switch would
/// bridge them.
///
/// This is the run that reading the tree could not replace. What it can settle
/// is the one-handed gesture: drag the row onto the Calendar tab and hold
/// there. If the tab bar spring-loads, the grid comes up under the finger and
/// the drop completes; if it does not, the gesture ends where it started and
/// the cell stays **No**.
///
/// What it **cannot** settle is the two-handed gesture — hold the row with one
/// finger, tap a tab with the other. XCUITest drives one gesture at a time and
/// has no API for two independent simultaneous touches, so that half stays
/// unmeasured and the matrix says so rather than guessing.
@MainActor
final class DragAcrossTabsUITests: SunriseUITestCase {
    func testTheTabBarDoesNotSpringLoadADraggedTaskOntoTheCalendar() throws {
        createVault()
        capture("Renew passport", landingAs: "Renew passport")
        dismissKeyboard()
        _ = waitUntil(timeout: 5) { self.app.tabBars.buttons["Calendar"].isHittable }

        let row = app.staticTexts["Renew passport"]
        XCTAssertTrue(row.waitForExistence(timeout: 10), "the captured row is on screen")
        let calendar = app.tabBars.buttons["Calendar"]
        XCTAssertTrue(calendar.waitForExistence(timeout: 10), "Calendar is in the tab bar")

        // Long enough to lift the item, slow enough for the system to treat
        // the tab as a spring-loading target, and held there for two seconds —
        // roughly four times what iOS gives a spring-loaded control.
        row.press(
            forDuration: 1.0,
            thenDragTo: calendar,
            withVelocity: .slow,
            thenHoldForDuration: 2.0
        )

        // The finding. If this ever fails, the tab bar has started
        // spring-loading and the parity matrix's *Task → Calendar block* row
        // is what has to change with it — not this assertion.
        XCTAssertFalse(
            app.navigationBars["Calendar"].waitForExistence(timeout: 3),
            "holding a dragged task over the Calendar tab does not switch to it"
        )
        XCTAssertTrue(
            app.staticTexts["Renew passport"].exists,
            "and the task is still where it was"
        )
    }
}
