import XCTest

/// The iOS twin of `SunriseUITests`: the only tests here that prove a **tap**
/// reaches the core.
///
/// `SunriseTests` runs on the simulator too and asserts a great deal, but all
/// of it against view models called directly. A model with perfect tests is
/// still unreachable if the tab that presents it was never wired, and the tab
/// shell is new code that nothing else exercises.
///
/// Unlike the macOS UI tests, these are **not** skipped in the scheme. A macOS
/// XCUITest drives another process on the developer's machine and the system
/// refuses until `DevToolsSecurity` is enabled; a simulator runner does not,
/// so there is nothing to opt into and no reason not to run them.
///
/// The launch, the scratch vault and the polling live in
/// ``SunriseUITestCase``, which the macOS suite shares.
@MainActor
final class TabShellUITests: SunriseUITestCase {
    /// Every tab opens and renders. The macOS twin asserts the same thing
    /// about the sidebar; here the failure it guards against is a `Tab` whose
    /// content was never wired, which looks identical to a working one until
    /// it is selected.
    func testEveryTabOpensAndRenders() throws {
        createVault()

        for tab in ["Today", "Calendar", "Browse", "Focus"] {
            let button = app.tabBars.buttons[tab]
            XCTAssertTrue(button.waitForExistence(timeout: 10), "\(tab) is in the tab bar")
            button.tap()
            XCTAssertTrue(
                waitUntil(timeout: 10) { self.app.state == .runningForeground },
                "\(tab) rendered without taking the app down"
            )
        }
    }

    /// The four screens that have no tab are still reachable from the toolbar
    /// menu, which is the whole claim `TabRoute` makes about them.
    ///
    /// A destination that routes nowhere is invisible rather than broken, so
    /// nothing else would catch it — and this is the exact failure that a
    /// sixth tab produced: the app's own tabs were re-homed inside a
    /// system-generated "More" list, present in the tree and unreachable in
    /// practice.
    func testTheScreensWithoutATabAreReachableFromTheMenu() throws {
        createVault()

        app.tabBars.buttons["Browse"].tap()
        for row in ["Morning", "Evening", "Routines", "Review"] {
            app.buttons["more"].tap()
            let item = app.buttons[row].firstMatch
            XCTAssertTrue(item.waitForExistence(timeout: 10), "\(row) is in the menu")
            item.tap()
            XCTAssertTrue(
                app.navigationBars[row].waitForExistence(timeout: 10),
                "\(row) pushed and titled itself"
            )
            app.navigationBars.buttons.firstMatch.tap()
            XCTAssertTrue(
                app.navigationBars["Browse"].waitForExistence(timeout: 10),
                "back returned to Browse"
            )
        }
    }

    // MARK: - Capture

    /// A tap reaches the core. The iOS twin of the Mac's capture test and the
    /// claim this suite was missing: SwiftUI → view model → UniFFI → the Rust
    /// core → SQLite → back, driven by real taps and real keystrokes.
    ///
    /// The app opens on Today, so this also pins the behaviour that makes the
    /// assertion possible at all — a bare line captured on Today gets the date
    /// Today selects on, rather than being written to the Inbox and vanishing
    /// off the screen that accepted it.
    func testCapturingFromTodayPutsTheTaskInTheList() throws {
        createVault()

        capture("Renew passport !1", landingAs: "Renew passport")
    }

    /// Capture, then leave.
    ///
    /// This is the test that could not be made to pass before: the software
    /// keyboard stays up after Add so a burst of thoughts is a burst of lines,
    /// and it covers the tab bar — so the tap that switches tabs went to the
    /// keyboard instead. `capture.done` is the way out, and this asserts that
    /// taking it actually frees the bar rather than merely existing.
    ///
    /// The Done button is a keyboard accessory and so is only drawn beside a
    /// software keyboard; a simulator with a hardware keyboard attached shows
    /// neither. The dismissal is therefore conditional and the navigation is
    /// not — the tab has to be reachable either way, which is the claim.
    func testTheTabBarIsReachableAfterCapturing() throws {
        createVault()
        capture("Renew passport !1", landingAs: "Renew passport")

        let done = app.buttons["capture.done"]
        if done.waitForExistence(timeout: 3) {
            done.tap()
        }

        let browse = app.tabBars.buttons["Browse"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { browse.isHittable },
            "the keyboard is no longer covering the tab bar"
        )
        browse.tap()
        XCTAssertTrue(
            app.navigationBars["Browse"].waitForExistence(timeout: 10),
            "the tab switched"
        )
    }

    /// The other capture seam.
    ///
    /// Calendar has no inline bar, so `openCapture` presents the sheet instead
    /// of focusing a field — a different path with a different commit
    /// (`AppSurfaces.commitCapture`, which has no local refresh and repaints
    /// through the change stream). The confirmation is set strictly after that
    /// commit returns, so it is proof the write landed rather than proof a
    /// button was tapped; the Inbox row afterwards is proof it is still there.
    func testTheCaptureSheetCommitsFromAScreenWithNoBar() throws {
        createVault()

        app.tabBars.buttons["Calendar"].tap()
        let open = app.buttons["capture"]
        XCTAssertTrue(open.waitForExistence(timeout: 10), "Calendar offers Capture")
        open.tap()

        let field = app.textFields["quick-capture.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "the sheet presented its field")
        field.tap()
        field.typeText("Book the ferry")

        let add = app.buttons["quick-capture.add"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { add.isEnabled },
            "the capture preview enables Add"
        )
        add.tap()

        // Matched as any descendant rather than as a `staticText`: the label is
        // a `Label`, and which element type SwiftUI folds that into is not a
        // promise worth resting a test on.
        let confirmation = app.descendants(matching: .any)["quick-capture.confirmation"]
        XCTAssertTrue(
            confirmation.waitForExistence(timeout: 10),
            "the sheet confirms the commit the core accepted"
        )

        // Cancel takes the sheet and its keyboard away together, which is what
        // makes the tab bar tappable again.
        app.buttons["quick-capture.cancel"].tap()

        let browse = app.tabBars.buttons["Browse"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { browse.isHittable },
            "dismissing the sheet freed the tab bar"
        )
        browse.tap()
        // The cell, not the identifier's own element. The row is a `Label`,
        // and on iOS SwiftUI splits that into an image and a static text which
        // both inherit the identifier — so the query is ambiguous, and the
        // image it resolves to first is not hittable on its own. The thing a
        // finger lands on is the row.
        let inbox = app.cells.containing(.staticText, identifier: "sidebar.inbox").firstMatch
        XCTAssertTrue(inbox.waitForExistence(timeout: 10), "the Inbox is in Browse")
        inbox.tap()

        XCTAssertTrue(
            app.staticTexts["Book the ferry"].waitForExistence(timeout: 10),
            "what the sheet captured is in the Inbox"
        )
    }
}
