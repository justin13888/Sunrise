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
/// The app is launched against a scratch vault directory and an in-memory key
/// store, so nothing here touches a real vault or Keychain item.
@MainActor
final class TabShellUITests: XCTestCase {
    private let app = XCUIApplication()
    private let scratch = FileManager.default.temporaryDirectory
        .appending(path: "sunrise-ios-uitests-\(UUID().uuidString)")

    override func setUp() async throws {
        continueAfterFailure = false
        app.launchArguments = [
            "-sunrise-ui-test-vault", scratch.path(percentEncoded: false)
        ]
        app.launch()
    }

    override func tearDown() async throws {
        app.terminate()
        try? FileManager.default.removeItem(at: scratch)
    }

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

    // MARK: - Helpers

    private func createVault() {
        let create = app.buttons["onboarding.create"]
        if create.waitForExistence(timeout: 20) {
            create.tap()
        }
    }

    /// Poll rather than sleep: everything here waits on a round trip through
    /// the core, and a fixed sleep would be either flaky or slow.
    private func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            usleep(100_000)
        }
        return condition()
    }
}
