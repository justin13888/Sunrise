import XCTest

/// The only tests in this project that prove a **click** reaches the core.
///
/// Everything in `SunriseTests` exercises a view model directly. That is a
/// useful claim and a narrower one: a model with perfect tests is still
/// unreachable if the button that calls it was never wired, and this epic has
/// found a correct-but-unreachable seam four separate times. These tests drive
/// the real window.
///
/// The launch, the scratch vault and the polling live in
/// ``SunriseUITestCase``, which the iOS suite shares.
@MainActor
final class AppNavigationUITests: SunriseUITestCase {
    /// First run, then capture, then the row exists. This is the shortest path
    /// that touches the whole stack from a click: SwiftUI → view model →
    /// UniFFI → the Rust core → SQLite → back.
    ///
    /// The window opens on Today, and until `TaskListModel.create` began
    /// giving a bare capture the date Today selects on, this assertion could
    /// not have held: the row was written to the Inbox and was not on screen.
    /// Nothing noticed, because this target is skipped in the scheme.
    func testCapturingATaskFromTheWindowPutsItInTheList() throws {
        createVault()

        capture("Renew passport !1", landingAs: "Renew passport")
    }

    /// Every primary view opens from the sidebar and renders something. Four
    /// of these did not exist before this stream, and a view that is built but
    /// unreachable from the sidebar is the failure this test is here for.
    func testEveryPrimaryViewOpensFromTheSidebar() throws {
        createVault()

        for name in ["inbox", "search", "focus", "routines", "review"] {
            let row = app.descendants(matching: .any)["sidebar.\(name)"]
            XCTAssertTrue(row.waitForExistence(timeout: 10), "\(name) is in the sidebar")
            activate(row)
            XCTAssertTrue(
                waitUntil(timeout: 10) { self.app.windows.firstMatch.exists },
                "\(name) rendered without taking the window down"
            )
        }
    }

    /// Search narrows what is on screen, driven by real keystrokes.
    func testSearchNarrowsFromTypedKeystrokes() throws {
        createVault()
        capture("Renew passport")
        capture("Book the ferry")

        activate(app.descendants(matching: .any)["sidebar.search"])
        let field = app.textFields["search.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        activate(field)
        field.typeText("ferry")

        XCTAssertTrue(
            app.staticTexts["Book the ferry"].waitForExistence(timeout: 10),
            "the match shows"
        )
        XCTAssertTrue(
            waitUntil(timeout: 10) { !self.app.staticTexts["Renew passport"].exists },
            "the non-match is filtered out"
        )
    }

    /// A stream created from the sidebar's own `+` reaches the vault and comes
    /// back as a row.
    func testCreatingAStreamFromTheSidebar() throws {
        createVault()

        // By identifier, not by display string. The label is user-facing text
        // that i18n will translate; the identifier is the contract. This
        // assertion was red for a different reason though, and it is worth
        // recording: the button was merged out of the accessibility tree by
        // the section header it lives in, so it existed on screen and nowhere
        // else. See `BrowseSidebar.header(_:)`, and
        // `addButton(_:systemImage:identifier:action:)` for where the
        // control went.
        let add = app.buttons["sidebar.stream.new"]
        XCTAssertTrue(add.waitForExistence(timeout: 10), "the Streams header offers +")
        activate(add)

        // The sheet's own field, not `textFields.firstMatch`. The list behind
        // this sheet is Today, which has a capture bar — so the first text
        // field was as likely to be `capture.field`, and typing there left the
        // Create button disabled, wrote nothing, and still satisfied a
        // `staticTexts["Travel"]` assertion off the capture preview. Green
        // test, empty vault.
        let name = app.textFields["stream.name"]
        XCTAssertTrue(name.waitForExistence(timeout: 10))
        activate(name)
        name.typeText("Travel")
        activate(app.buttons["Create"])

        XCTAssertTrue(
            app.descendants(matching: .any)["sidebar.stream.travel"]
                .firstMatch.waitForExistence(timeout: 10),
            "the new stream is a row in the sidebar"
        )
    }
}
