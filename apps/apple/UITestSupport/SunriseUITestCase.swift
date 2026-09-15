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

    /// Press it, whatever pressing means here — and not before it can be
    /// pressed.
    ///
    /// The one genuine difference between the two suites, and the reason this
    /// is a method rather than a rule everybody remembers: `click()` is macOS
    /// and `tap()` is touch, and getting it wrong fails at compile time in one
    /// target and nowhere else.
    ///
    /// The **waits** are here for a reason that cost a CI run. A press aimed at
    /// an element that exists but is not yet hittable — a menu mid-presentation,
    /// a sheet still sliding up, a control under the software keyboard — lands
    /// nowhere, and the test then fails several lines later on whatever the
    /// press was supposed to have produced. That is the classic XCUITest flake,
    /// and it is what took `LibraryReachUITests` red on a pull request that
    /// touched no Swift at all. Every suite here had call sites guarded by hand,
    /// and some had none; making the guard part of the only press helper is what
    /// stops the next one being written unguarded.
    ///
    /// Existence and hittability are **separate** assertions on purpose: "it was
    /// never built" and "it was built and something is over it" are different
    /// defects, and a single combined message would name neither.
    ///
    /// `named` is the subject of both messages, and `file`/`line` are forwarded
    /// the way ``capture(_:landingAs:file:line:)`` forwards them, so a failure
    /// points at the test that asked for the press rather than at this file.
    func activate(
        _ element: XCUIElement,
        named name: String? = nil,
        timeout: TimeInterval = 10,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let subject = name ?? "the element"
        guard element.waitForExistence(timeout: timeout) else {
            XCTFail("\(subject) is on screen to be pressed", file: file, line: line)
            return
        }
        // A `guard` rather than an `XCTAssertTrue`, because the walk suites set
        // `continueAfterFailure = true`: there, a failed assertion returns here
        // rather than unwinding, and pressing a thing that is not hittable
        // raises a second, less informative failure on top of the first.
        guard waitUntil(timeout: timeout, { element.isHittable }) else {
            XCTFail(
                "\(subject) is hittable to be pressed — it exists, so something "
                    + "is over it or its presentation never settled",
                file: file,
                line: line
            )
            return
        }
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
            activate(create, named: "first run's Create button", timeout: 20)
        }
    }

    /// Capture one line through the inline bar and wait for its row.
    ///
    /// `line` is what gets typed and `title` is what the parser will make of
    /// it — they differ whenever the line carries annotations, which is most
    /// of the time worth testing.
    ///
    /// `file` and `line` are forwarded so a failure points at the test that
    /// asked for the capture rather than at this file, which four call sites
    /// across two products now share.
    func capture(
        _ text: String,
        landingAs title: String? = nil,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        let field = app.textFields["capture.field"]
        activate(field, named: "the capture field", file: file, line: line)
        field.typeText(text)
        // Add is disabled until the debounced preview lands, which is itself a
        // round trip through the core.
        let add = app.buttons["capture.add"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { add.isEnabled },
            "the capture preview enables Add",
            file: file,
            line: line
        )
        activate(add, named: "the capture bar's Add button", file: file, line: line)

        let expected = title ?? text
        XCTAssertTrue(
            app.staticTexts[expected].waitForExistence(timeout: 10),
            "the captured task appears in the list it was typed into",
            file: file,
            line: line
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
