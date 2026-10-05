import XCTest

/// Saved views and iCalendar, reachable from the phone.
///
/// `docs/07-clients/parity-matrix.md` graded both **unmet** for iOS, and both
/// for the same reason: a working, tested shared model whose only caller was
/// `macOS/VaultWindow.swift`. The models compiled into this product the whole
/// time, so nothing short of driving the shell distinguishes "it is wired" from
/// "it links". That is what this asserts — reachability, which is the exact
/// thing the audit grades.
@MainActor
final class LibraryReachUITests: SunriseUITestCase {
    /// The Views menu is on the screens a saved view can name, and it opens.
    func testTheSavedViewsMenuIsOnTheListToolbarAndOffersToSaveThisView() throws {
        createVault()

        activate(app.buttons["saved-views"], named: "Today's Views menu")

        // By name rather than by identifier, for the reason
        // `SidebarAddButtonTests` gives: a control findable only by identifier
        // is silent to VoiceOver.
        XCTAssertTrue(
            app.buttons["Save this view…"].appears(within: 10),
            "the menu offers to save the view on screen"
        )
    }

    /// Saving one, then recalling it from the same menu.
    func testASavedViewRoundTripsThroughTheMenu() throws {
        createVault()

        // Both of these were unguarded `tap()`s, and that is what made this
        // test the one CI caught: a menu item pressed before the menu's
        // presentation animation settles is pressed at nothing, and the
        // failure then surfaces four lines down on a sheet that never came.
        // `activate` waits for both halves and names which one was missing.
        activate(app.buttons["saved-views"], named: "Today's Views menu")
        activate(app.buttons["Save this view…"], named: "the menu's Save this view… item")

        // `firstMatch` on the type rather than by name: the sheet has exactly
        // one field, and which of a `TextField`'s two strings — its title or
        // its prompt — SwiftUI hands the accessibility tree as the label is not
        // a promise worth resting a test on.
        let name = app.textFields.firstMatch
        XCTAssertTrue(name.appears(within: 10), "the sheet asks for a name")
        activate(name, named: "the sheet's name field")
        name.typeText("mornings")
        activate(app.buttons["Save"], named: "the sheet's Save button")

        // The store is a file `sunrise` also reads, so this is a real write
        // and not a list held in the view.
        activate(app.buttons["saved-views"], named: "Today's Views menu, once the sheet closed")
        XCTAssertTrue(
            app.descendants(matching: .any)
                .matching(NSPredicate(format: "label BEGINSWITH %@", "mornings"))
                .firstMatch
                .appears(within: 10),
            "the saved view is in the menu"
        )
    }

    /// Import and export are in Browse's overflow, which is where the Mac's
    /// File menu items land on a phone.
    func testTheCalendarImportAndExportAreInTheOverflowMenu() throws {
        createVault()
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        activate(app.buttons["more"], named: "Browse's More menu")

        for name in ["Import calendar…", "Export calendar"] {
            XCTAssertTrue(
                app.buttons[name].appears(within: 10)
                    || app.otherElements[name].appears(within: 1),
                "\(name) is in the menu under its own name"
            )
        }
    }
}
