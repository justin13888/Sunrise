import XCTest

/// A task row dragged onto a sibling row in the Inbox is moved above it
/// (#1297): the parity matrix's iOS *Task → Task (reorder)* cell, measured
/// rather than inferred from the shared code.
///
/// An iOS `List` offers a drag one of its own rows started to no drop target
/// inside it (#1234). Measured here on the iPhone 17 Pro simulator before the
/// fix, with the reorder still the row's `dropDestination`: the row lifted
/// over its sibling and the drop was refused on every press. So on iOS the
/// reorder is the `List`'s own `onMove` (`TaskRows.rowList`), and this is its
/// regression test: a refused drop leaves both rows where they were, and the
/// order assertion goes red.
final class TaskReorderUITests: SunriseUITestCase {
    func testDraggingAnInboxRowOntoASiblingMovesItAboveThatRow() {
        createVault()
        openInbox()
        capture("Alpha")
        capture("Beta")
        dismissKeyboard()

        let alpha = app.staticTexts["Alpha"]
        let beta = app.staticTexts["Beta"]
        XCTAssertTrue(alpha.appears(within: 10) && beta.appears(within: 10), "both rows are on screen")
        // Whichever is lower is the one dragged, so the test does not depend
        // on the order the Inbox lists fresh captures in.
        let (upper, lower) = alpha.frame.minY < beta.frame.minY ? (alpha, beta) : (beta, alpha)
        let lowerTitle = lower.label

        drag(lower, onto: upper)

        XCTAssertTrue(
            waitUntil(timeout: 10) { lower.frame.minY < upper.frame.minY },
            "the dropped row \(lowerTitle) is drawn above the row it was dropped on"
        )
    }

    private func openInbox() {
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        // The cell, not the identifier's own element (`TabShellUITests`).
        let inbox = app.cells.containing(.staticText, identifier: "sidebar.inbox").firstMatch
        activate(inbox, named: "the Inbox row in Browse")
        XCTAssertTrue(
            app.textFields["capture.field"].appears(within: 10),
            "the Inbox's capture bar is on screen"
        )
    }

    /// Long-press `row` until it lifts, drag it onto `target`, and let go.
    ///
    /// The row has a context menu as well as a drag, and both start on a long
    /// press, so the press is retried at the lengths `UpcomingUITests` measured
    /// for the same row view, dismissing the menu when a press opens it. A
    /// refused drop refuses every attempt, so the caller's assertion still
    /// fails after the last one.
    private func drag(_ row: XCUIElement, onto target: XCUIElement) {
        let presses: [TimeInterval] = [0.5, 0.6, 0.7, 0.4]
        let menu = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Defer a week")).firstMatch
        for (attempt, press) in presses.enumerated() {
            let moved = XCTContext.runActivity(
                named: "Drag attempt \(attempt + 1) of \(presses.count), pressing \(press) s"
            ) { _ -> Bool in
                row.press(
                    forDuration: press, thenDragTo: target, withVelocity: .slow, thenHoldForDuration: 1.0
                )
                if menu.exists {
                    app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.05)).tap()
                    XCTAssertTrue(
                        waitUntil(timeout: 5) { !menu.exists },
                        "the context menu a press opened is dismissed before pressing again"
                    )
                    return false
                }
                return waitUntil(timeout: 5) { row.frame.minY < target.frame.minY }
            }
            if moved { return }
        }
    }
}
