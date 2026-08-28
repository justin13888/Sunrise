import XCTest

/// The only tests in this project that prove a **click** reaches the core.
///
/// Everything in `SunriseTests` exercises a view model directly. That is a
/// useful claim and a narrower one: a model with perfect tests is still
/// unreachable if the button that calls it was never wired, and this epic has
/// found a correct-but-unreachable seam four separate times. These tests drive
/// the real window.
///
/// The app is launched against a scratch vault directory and an in-memory key
/// store, so nothing here touches the developer's vault or their Keychain.
@MainActor
final class AppNavigationUITests: XCTestCase {
    private let app = XCUIApplication()
    private let scratch = FileManager.default.temporaryDirectory
        .appending(path: "sunrise-uitests-\(UUID().uuidString)")

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

    /// First run, then capture, then the row exists. This is the shortest path
    /// that touches the whole stack from a click: SwiftUI → view model →
    /// UniFFI → the Rust core → SQLite → back.
    func testCapturingATaskFromTheWindowPutsItInTheList() throws {
        createVault()

        let field = app.textFields["capture.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "the capture field is on screen")
        field.click()
        field.typeText("Renew passport !1")
        // The Add button is disabled until the debounced preview lands, which
        // is itself a round trip through the core.
        let add = app.buttons["capture.add"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { add.isEnabled },
            "the capture preview enables Add"
        )
        add.click()

        XCTAssertTrue(
            app.staticTexts["Renew passport"].waitForExistence(timeout: 10),
            "the captured task appears in the list"
        )
    }

    /// Every primary view opens from the sidebar and renders something. Four
    /// of these did not exist before this stream, and a view that is built but
    /// unreachable from the sidebar is the failure this test is here for.
    func testEveryPrimaryViewOpensFromTheSidebar() throws {
        createVault()

        for name in ["inbox", "search", "focus", "routines", "review"] {
            let row = app.descendants(matching: .any)["sidebar.\(name)"]
            XCTAssertTrue(row.waitForExistence(timeout: 10), "\(name) is in the sidebar")
            row.click()
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

        app.descendants(matching: .any)["sidebar.search"].click()
        let field = app.textFields["search.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.click()
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

        let add = app.buttons["New stream"]
        XCTAssertTrue(add.waitForExistence(timeout: 10), "the Streams header offers +")
        add.click()

        let name = app.textFields.firstMatch
        XCTAssertTrue(name.waitForExistence(timeout: 10))
        name.click()
        name.typeText("Travel")
        app.buttons["Create"].click()

        XCTAssertTrue(
            app.staticTexts["Travel"].waitForExistence(timeout: 10),
            "the new stream is in the sidebar"
        )
    }

    // MARK: - Helpers

    private func createVault() {
        let create = app.buttons["onboarding.create"]
        if create.waitForExistence(timeout: 15) {
            create.click()
        }
    }

    private func capture(_ title: String) {
        let field = app.textFields["capture.field"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.click()
        field.typeText(title)
        let add = app.buttons["capture.add"]
        XCTAssertTrue(waitUntil(timeout: 10) { add.isEnabled })
        add.click()
        XCTAssertTrue(app.staticTexts[title].waitForExistence(timeout: 10))
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
