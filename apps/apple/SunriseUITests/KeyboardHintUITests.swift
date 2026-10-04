import XCTest

/// `docs/08-features/keyboard.md` Rule 1, read off the real window: a toolbar
/// button's tooltip names its key after its title.
///
/// The tooltip is read as macOS draws it — hover, then the tooltip element —
/// rather than from the view's source, because the claim is about what a user
/// hovering the icon is shown.
@MainActor
final class KeyboardHintUITests: SunriseUITestCase {
    func testTheUndoTooltipNamesItsKey() throws {
        createVault()
        capture("Renew passport")

        let undo = app.buttons["toolbar.undo"]
        XCTAssertTrue(undo.waitForExistence(timeout: 10), "the toolbar has an Undo button")
        XCTAssertTrue(
            waitUntil(timeout: 10) { undo.isEnabled },
            "the capture left something to undo"
        )
        undo.hover()

        // A tooltip is a help tag to the accessibility tree.
        let tip = app.helpTags.firstMatch
        XCTAssertTrue(tip.waitForExistence(timeout: 10), "hovering Undo shows its tooltip")
        XCTAssertTrue(tip.label.hasPrefix("Undo"), "the tooltip leads with the step: \(tip.label)")
        XCTAssertTrue(tip.label.hasSuffix("(⌘Z)"), "the tooltip ends on the key: \(tip.label)")
    }
}
