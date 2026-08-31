import XCTest

/// What both UI suites need before they can assert anything.
///
/// The two suites make different claims — one about a Mac window's sidebar,
/// one about a phone's tab bar — but they reach the app the same way: a
/// scratch vault, a tap through onboarding, and a poll rather than a sleep
/// because every interesting step is a round trip through the Rust core. That
/// was duplicated verbatim in both files, which is one copy too many for
/// something that decides whether a test is red for a real reason.
///
/// The app is launched against a scratch vault directory and an in-memory key
/// store, so nothing here touches the developer's vault or their Keychain.
@MainActor
class SunriseUITestCase: XCTestCase {
    let app = XCUIApplication()

    /// Not implicitly unwrapped: a nil here means `setUp` did not run, and the
    /// honest response is to delete nothing rather than to trap.
    private var scratch: URL?

    override func setUp() async throws {
        try await super.setUp()
        continueAfterFailure = false
        let directory = FileManager.default.temporaryDirectory
            .appending(path: "sunrise-uitests-\(UUID().uuidString)")
        scratch = directory
        app.launchArguments = [
            "-sunrise-ui-test-vault", directory.path(percentEncoded: false)
        ]
        app.launch()
    }

    override func tearDown() async throws {
        app.terminate()
        if let scratch { try? FileManager.default.removeItem(at: scratch) }
        try await super.tearDown()
    }

    // MARK: - Driving the app

    /// Press it, whatever pressing means here.
    ///
    /// The one genuine difference between the two suites, and the reason this
    /// is a method rather than a rule everybody remembers: `click()` is macOS
    /// and `tap()` is touch, and getting it wrong fails at compile time in one
    /// target and nowhere else.
    func activate(_ element: XCUIElement) {
        #if os(macOS)
        element.click()
        #else
        element.tap()
        #endif
    }

    /// Get past first run.
    ///
    /// Every launch lands on `.firstRun`: the scratch directory is empty and
    /// the key store lives and dies with the process, so there is never a
    /// vault to reopen. Opening one is a real `Core::open`, which is why the
    /// timeout is generous.
    func createVault() {
        let create = app.buttons["onboarding.create"]
        if create.waitForExistence(timeout: 20) {
            activate(create)
        }
    }

    /// Capture one line through the inline bar and wait for its row.
    ///
    /// `line` is what gets typed and `title` is what the parser will make of
    /// it — they differ whenever the line carries annotations, which is most
    /// of the time worth testing.
    func capture(_ line: String, landingAs title: String? = nil) {
        let field = app.textFields["capture.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "the capture field is on screen")
        activate(field)
        field.typeText(line)
        // Add is disabled until the debounced preview lands, which is itself a
        // round trip through the core.
        let add = app.buttons["capture.add"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { add.isEnabled },
            "the capture preview enables Add"
        )
        activate(add)

        let expected = title ?? line
        XCTAssertTrue(
            app.staticTexts[expected].waitForExistence(timeout: 10),
            "the captured task appears in the list it was typed into"
        )
    }

    /// Poll rather than sleep: everything here waits on a round trip through
    /// the core, and a fixed sleep would be either flaky or slow.
    func waitUntil(timeout: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            usleep(100_000)
        }
        return condition()
    }
}
