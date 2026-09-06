import XCTest

/// One PNG per iOS screen, for design review.
///
/// The five tabs, then the four destinations that have no tab and are reached
/// from Browse's "More" menu — which is the half of the iOS shell most likely
/// to be wrong, because a screen you have to know about is a screen nobody
/// looks at.
///
/// Run it with `mise run apple-shots`, which extracts the attachments into
/// `out/shots/ios/`.
///
/// Unlike its macOS twin, this **also runs on every `mise run ios-app`** and so
/// in CI, because `SunriseiOSUITests` is not skipped in the `SunriseiOS`
/// scheme. That costs about a minute and buys two things: the walk cannot rot
/// unnoticed, and every CI run leaves a full set of screenshots in its result
/// bundle. If that minute ever stops being worth it, `-skip-testing:` on the
/// `ios-app` task is the lever — not deleting the suite.
@MainActor
final class ScreenshotWalkTests: SunriseUITestCase {

    /// The base class sets `continueAfterFailure = false`, which is right for
    /// the navigation suites: there, the first broken thing invalidates
    /// everything after it. A walk is the opposite. Its product is the images,
    /// and stopping at the first missing element throws away every screenshot
    /// that would have come next — including the ones showing what went wrong.
    ///
    /// So: failures are still recorded, and the walk still finishes.
    override func setUp() async throws {
        try await super.setUp()
        continueAfterFailure = true
    }
    func testWalkEveryScreen() throws {
        XCTAssertTrue(
            app.buttons["onboarding.create"].waitForExistence(timeout: 20),
            "first run is on screen"
        )
        shot("00-first-run")

        createVault()
        shot("01-today-empty")

        seedFixture()
        shot("02-today")

        // Search last: it is a `Tab(role: .search)`, which the system draws
        // apart from the others, and selecting it leaves a keyboard up that
        // would sit over the next screenshot.
        for (index, tab) in ["Today", "Calendar", "Browse", "Focus", "Search"].enumerated() {
            let button = app.tabBars.buttons[tab]
            guard button.waitForExistence(timeout: 10) else {
                XCTFail("\(tab) is in the tab bar")
                continue
            }
            button.tap()
            _ = waitUntil(timeout: 5) { self.app.state == .runningForeground }
            shot(String(format: "%02d-tab-%@", index + 10, tab.lowercased()))
        }

        walkTheMenuDestinations()
        walkCaptureSheet()
    }

    /// Morning, Evening, Routines and Review have no tab. `TabRoute` pushes
    /// them onto Browse's stack from its toolbar menu, and that is the only
    /// way to them — which is exactly why they are worth photographing.
    private func walkTheMenuDestinations() {
        app.tabBars.buttons["Browse"].tap()
        for (index, row) in ["Morning", "Evening", "Routines", "Review"].enumerated() {
            app.buttons["more"].tap()
            let item = app.buttons[row].firstMatch
            guard item.waitForExistence(timeout: 10) else {
                XCTFail("\(row) is in the More menu")
                continue
            }
            item.tap()
            _ = app.navigationBars[row].waitForExistence(timeout: 10)
            shot(String(format: "%02d-more-%@", index + 20, row.lowercased()))
            app.navigationBars.buttons.firstMatch.tap()
        }
    }

    /// Capture is a sheet on iOS where the Mac has a borderless panel. It is
    /// reached from the toolbar button every screen without an inline bar
    /// carries.
    private func walkCaptureSheet() {
        app.tabBars.buttons["Calendar"].tap()
        let button = app.buttons["capture"]
        guard button.waitForExistence(timeout: 10) else {
            XCTFail("Calendar offers a capture button")
            return
        }
        button.tap()
        _ = app.textFields["capture.field"].waitForExistence(timeout: 10)
        shot("30-capture-sheet")
    }
}
