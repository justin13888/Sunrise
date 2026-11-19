import XCTest

/// A block dragged across a second block moves the second out of its way,
/// and the drop lands as one undo step that names both (#344).
///
/// On the phone because `SunriseUITests` is `skipped: true` in the Mac
/// scheme, so a Mac-side test would be a test nothing runs (see
/// `BlockSheetUITests`).
///
/// What this can and cannot see. XCUITest's drag is one blocking call, so
/// nothing can be queried between the press and the release: the ghosts a
/// drag draws while it is in flight are not observable from here. What is
/// observable is what the drop committed, and the drop commits exactly the
/// diff the ghosts were drawn from (`CalendarModel.drop`), so the Undo item
/// naming "moved 2" is the displaced block's ghost made permanent. The
/// ghosts themselves, and that the committed positions equal them, are
/// asserted against a real vault in `PlannedDragTests`.
final class PlannedDragUITests: SunriseUITestCase {
    func testDraggingABlockOntoAnotherRipplesItAndUndoesInOneStep() {
        createVault()
        activate(app.tabBars.buttons["Calendar"], named: "the Calendar tab")
        XCTAssertTrue(
            app.segmentedControls["calendar-toolbar"].appears(within: 10),
            "the calendar is on screen before anything is dragged on it"
        )
        // Tomorrow, so neither block is in the past, which the planner holds
        // fixed and would never move out of the way.
        activate(app.buttons["calendar-next"], named: "the calendar's Next button")

        drawBlock("Standup", from: Self.firstTop, to: Self.firstBottom)
        drawBlock("Focus", from: Self.secondTop, to: Self.secondBottom)

        // Standup's middle, dragged down by the distance between the two
        // blocks' tops, so it lands on Focus.
        let middle = (Self.firstTop + Self.firstBottom) / 2
        let from = app.coordinate(withNormalizedOffset: CGVector(dx: Self.dragX, dy: middle))
        let to = app.coordinate(
            withNormalizedOffset: CGVector(dx: Self.dragX, dy: middle + Self.secondTop - Self.firstTop)
        )
        from.press(forDuration: 0.6, thenDragTo: to, withVelocity: .slow, thenHoldForDuration: 1.0)

        XCTAssertTrue(
            waitUntil(timeout: 10) { !self.app.descendants(matching: .any)["calendar-plan-summary"].exists },
            "the drop committed the plan and cleared it from the grid"
        )
        let undoMove = "Undo move ‘Standup’ (moved 2)"
        openOverflow()
        XCTAssertTrue(
            app.buttons[undoMove].appears(within: 10),
            "the drop is one undo step that moved Standup and Focus both"
        )
        activate(app.buttons[undoMove], named: "the Undo item")

        openOverflow()
        XCTAssertTrue(
            app.buttons["Undo create a block"].appears(within: 10),
            "one undo took the whole drop back, both blocks, to the step before it"
        )
    }

    /// Drag down the day column to draw a block, name it, and add it.
    private func drawBlock(_ title: String, from top: CGFloat, to bottom: CGFloat) {
        let start = app.coordinate(withNormalizedOffset: CGVector(dx: Self.dragX, dy: top))
        let end = app.coordinate(withNormalizedOffset: CGVector(dx: Self.dragX, dy: bottom))
        start.press(forDuration: 0.6, thenDragTo: end)

        let field = app.textFields["block-title"]
        activate(field, named: "the draft sheet's title field")
        field.typeText(title)
        activate(app.buttons["Add block"], named: "the draft sheet's Add block")
        XCTAssertTrue(
            app.staticTexts[title].appears(within: 10),
            "\(title) reached the grid"
        )
    }

    /// The overflow menu, from Browse, where the phone keeps Undo.
    private func openOverflow() {
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        activate(app.buttons["more"], named: "Browse's More menu")
    }

    private static let dragX: CGFloat = 0.6
    private static let firstTop: CGFloat = 0.40
    private static let firstBottom: CGFloat = 0.47
    private static let secondTop: CGFloat = 0.52
    private static let secondBottom: CGFloat = 0.59
}
