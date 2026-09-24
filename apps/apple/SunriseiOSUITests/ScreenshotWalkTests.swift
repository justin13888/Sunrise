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
/// Unlike its macOS twin, this **also runs on `mise run ios-app`** and so in
/// CI, because `SunriseiOSUITests` is not skipped in the `SunriseiOS` scheme —
/// but not on a pull request. It measured 128 s in CI. The screenshots only
/// outlive the runner when the run fails, because a green run's result bundle
/// is never uploaded; what a pull request gives up is the walk's own reach —
/// it is the only iOS UI test that opens the Search tab, seeds a fixture
/// through five captures and dismisses the keyboard, so a break only it would
/// catch goes red on master after the merge rather than on the pull request.
/// CI sets `SUNRISE_SKIP_SCREENSHOT_WALK` on pull requests, which
/// the `ios-app` task turns into `-skip-testing:` for this class alone, and
/// the walk runs on every push to master, nightly and on dispatch. A
/// developer's `mise run ios-app` sets nothing and still walks.
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
            activate(app.tabBars.buttons[tab], named: "the \(tab) tab")
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
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        for (index, row) in ["Morning", "Evening", "Routines", "Review"].enumerated() {
            activate(app.buttons["more"], named: "Browse's More menu")
            activate(app.buttons[row].firstMatch, named: "\(row) in the More menu")
            _ = app.navigationBars[row].waitForExistence(timeout: 10)
            shot(String(format: "%02d-more-%@", index + 20, row.lowercased()))
            activate(app.navigationBars.buttons.firstMatch, named: "the back button on \(row)")
        }
    }

    /// Capture is a sheet on iOS where the Mac has a borderless panel. It is
    /// reached from the toolbar button every screen without an inline bar
    /// carries.
    private func walkCaptureSheet() {
        activate(app.tabBars.buttons["Calendar"], named: "the Calendar tab")
        activate(app.buttons["capture"], named: "Calendar's Capture button")
        _ = app.textFields["capture.field"].waitForExistence(timeout: 10)
        shot("30-capture-sheet")
    }
}
