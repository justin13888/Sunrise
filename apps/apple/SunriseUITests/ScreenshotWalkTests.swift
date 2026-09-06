import XCTest

/// One PNG per macOS screen, for design review.
///
/// Nine sidebar destinations, the two keyboard surfaces, and the borderless
/// capture panel — the surfaces a Mac has that a phone does not are the whole
/// reason this suite is separate from its iOS twin rather than shared.
///
/// Run it with `mise run apple-shots`, which extracts the attachments into
/// `out/shots/macos/`. It lives in the `SunriseUITests` target, which is
/// `skipped: true` in the `Sunrise` scheme — so this does not lengthen
/// `mise run macos-app` or the CI job behind it.
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
        // Before `createVault`, because first run is a screen too and it is
        // the only one a returning user never sees again.
        XCTAssertTrue(
            app.buttons["onboarding.create"].waitForExistence(timeout: 20),
            "first run is on screen"
        )
        shot("00-first-run")

        createVault()
        shot("01-today-empty")

        seedFixture()
        shot("02-today")

        // In `Destination.fixed` order, which is the order the sidebar draws
        // them, so the filenames sort into the shape of the sidebar.
        let destinations = [
            "morning", "today", "inbox", "search",
            "calendar", "focus", "routines", "review", "evening"
        ]
        for (index, name) in destinations.enumerated() {
            let row = app.descendants(matching: .any)["sidebar.\(name)"]
            guard row.waitForExistence(timeout: 10) else {
                XCTFail("\(name) is in the sidebar")
                continue
            }
            activate(row)
            // A poll rather than a sleep: every one of these is a round trip
            // through the core, and a fixed wait would be either flaky or
            // slow. There is nothing specific to wait *for* — the walk does
            // not know what each screen renders — so this waits on the
            // window still being there and lets the runloop settle.
            _ = waitUntil(timeout: 5) { self.app.windows.firstMatch.exists }
            shot(String(format: "%02d-%@", index + 10, name))
        }

        walkKeyboardSurfaces()
        walkCapturePanel()
    }

    /// The command palette and the cheat sheet. Both are macOS-only: iOS has
    /// no keyboard to summon them from, and `VaultTabs` wires them inert for
    /// that reason.
    private func walkKeyboardSurfaces() {
        app.typeKey("p", modifierFlags: [.command, .shift])
        if app.textFields["palette.field"].waitForExistence(timeout: 5) {
            shot("30-command-palette")
            app.typeKey(.escape, modifierFlags: [])
        } else {
            XCTFail("the command palette opens on ⌘⇧P")
        }

        app.typeKey("/", modifierFlags: [.command])
        _ = waitUntil(timeout: 5) { self.app.windows.firstMatch.exists }
        shot("31-cheat-sheet")
        app.typeKey(.escape, modifierFlags: [])
    }

    /// The borderless panel behind the global hotkey. Not the inline capture
    /// bar — that is already in every list screenshot — but the surface that
    /// opens over whatever app you were in.
    private func walkCapturePanel() {
        app.typeKey("n", modifierFlags: [.command, .shift])
        if app.textFields["quick-capture.field"].waitForExistence(timeout: 5) {
            shot("32-quick-capture")
            app.typeKey(.escape, modifierFlags: [])
        } else {
            XCTFail("the quick capture panel opens on ⌘⇧N")
        }
    }
}
