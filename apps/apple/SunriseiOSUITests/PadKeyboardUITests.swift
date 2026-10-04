import UIKit
import XCTest

/// An iPad with a hardware keyboard gets the desktop keyboard set —
/// `docs/08-features/keyboard.md` Rule 3.
///
/// The keys are typed through `typeKey`, which the simulator delivers as
/// hardware-keyboard events: the same path a Magic Keyboard takes, and the one
/// a `UIKeyCommand` answers. On an iPhone simulator these skip, because a
/// phone leaves the palette and the cheat sheet inert by design; the
/// `ios-app` task runs an iPhone, so these run on an iPad destination only.
@MainActor
final class PadKeyboardUITests: SunriseUITestCase {
    override func setUp() async throws {
        try XCTSkipUnless(
            UIDevice.current.userInterfaceIdiom == .pad,
            "the palette and the cheat sheet are an iPad's; a phone leaves them inert"
        )
        try await super.setUp()
    }

    /// ⇧⌘P is a key command on the scene, so it works with nothing focused.
    func testShiftCommandPOpensThePalette() throws {
        createVault()

        app.typeKey("p", modifierFlags: [.command, .shift])

        XCTAssertTrue(
            app.textFields["palette.field"].waitForExistence(timeout: 10),
            "⇧⌘P opened the command palette"
        )
    }

    /// `?` is a list key, so it needs the rows to hold the keyboard — which
    /// ⌘1 gives them, as it does on the Mac. A row has to exist first: an
    /// empty list draws its empty state, not a list that can take focus.
    func testQuestionMarkInAListOpensTheCheatSheet() throws {
        createVault()
        capture("Renew passport")

        app.typeKey("1", modifierFlags: .command)
        app.typeKey("/", modifierFlags: .shift)

        XCTAssertTrue(
            app.descendants(matching: .any)["cheatsheet"].waitForExistence(timeout: 10),
            "? in the list opened the cheat sheet"
        )
    }

    /// ⌘K from Search itself — the empty state's own advice — starts a fresh
    /// query in the field, rather than leaving the old one standing because
    /// the tab was already on screen.
    func testCommandKOnSearchClearsTheQueryAndFocusesTheField() throws {
        createVault()

        app.typeKey("k", modifierFlags: .command)
        let field = app.textFields["search.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "⌘K showed Search")
        field.tap()
        field.typeText("ferry")
        app.typeKey(.return, modifierFlags: []) // the field hands the keyboard to the results

        app.typeKey("k", modifierFlags: .command)

        let cleared = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "value != %@", "ferry"),
            object: field
        )
        wait(for: [cleared], timeout: 10)
        XCTAssertEqual(
            field.value(forKey: "hasKeyboardFocus") as? Bool,
            true,
            "⌘K put the keyboard in the field"
        )
    }

    /// ⌘/ is the menu's way to the same sheet, from anywhere.
    func testCommandSlashOpensTheCheatSheet() throws {
        createVault()

        app.typeKey("/", modifierFlags: .command)

        XCTAssertTrue(
            app.descendants(matching: .any)["cheatsheet"].waitForExistence(timeout: 10),
            "⌘/ opened the cheat sheet"
        )
    }
}
