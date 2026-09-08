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

        let menu = app.buttons["saved-views"]
        XCTAssertTrue(menu.waitForExistence(timeout: 10), "Today's toolbar offers Views")
        menu.tap()

        // By name rather than by identifier, for the reason
        // `SidebarAddButtonTests` gives: a control findable only by identifier
        // is silent to VoiceOver.
        XCTAssertTrue(
            app.buttons["Save this view…"].waitForExistence(timeout: 10),
            "the menu offers to save the view on screen"
        )
    }

    /// Saving one, then recalling it from the same menu.
    func testASavedViewRoundTripsThroughTheMenu() throws {
        createVault()

        app.buttons["saved-views"].tap()
        app.buttons["Save this view…"].tap()

        // `firstMatch` on the type rather than by name: the sheet has exactly
        // one field, and which of a `TextField`'s two strings — its title or
        // its prompt — SwiftUI hands the accessibility tree as the label is not
        // a promise worth resting a test on.
        let name = app.textFields.firstMatch
        XCTAssertTrue(name.waitForExistence(timeout: 10), "the sheet asks for a name")
        name.tap()
        name.typeText("mornings")
        app.buttons["Save"].tap()

        // The store is a file `sunrise` also reads, so this is a real write
        // and not a list held in the view.
        let menu = app.buttons["saved-views"]
        XCTAssertTrue(waitUntil(timeout: 10) { menu.isHittable }, "the sheet closed")
        menu.tap()
        XCTAssertTrue(
            app.descendants(matching: .any)
                .matching(NSPredicate(format: "label BEGINSWITH %@", "mornings"))
                .firstMatch
                .waitForExistence(timeout: 10),
            "the saved view is in the menu"
        )
    }

    /// Import and export are in Browse's overflow, which is where the Mac's
    /// File menu items land on a phone.
    func testTheCalendarImportAndExportAreInTheOverflowMenu() throws {
        createVault()
        app.tabBars.buttons["Browse"].tap()

        let more = app.buttons["more"]
        XCTAssertTrue(more.waitForExistence(timeout: 10), "Browse offers the More menu")
        more.tap()

        for name in ["Import calendar…", "Export calendar"] {
            XCTAssertTrue(
                app.buttons[name].waitForExistence(timeout: 10)
                    || app.otherElements[name].waitForExistence(timeout: 1),
                "\(name) is in the menu under its own name"
            )
        }
    }
}
