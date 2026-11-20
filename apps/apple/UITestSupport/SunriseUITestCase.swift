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
    private(set) var scratch: URL?

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

    /// Launch again, against the same scratch directory, with `flags` added.
    ///
    /// For a suite that needs a harness configuration the default launch does
    /// not give it — the multi-vault session, say, which is opt-in so that
    /// every suite that does not name it keeps the one it was written against.
    /// Called from `setUp` after `super.setUp()`, before anything is asserted.
    func relaunch(adding flags: [String]) {
        app.terminate()
        app.launchArguments.append(contentsOf: flags)
        app.launch()
    }

    // MARK: - A seeded relay

    /// The runner's environment variable naming a running self-host relay's
    /// base URL (#458). `xcodebuild` hands the runner every variable it was
    /// given with a `TEST_RUNNER_` prefix, prefix stripped, so a run sets
    /// `TEST_RUNNER_SUNRISE_UI_TEST_RELAY`.
    static let relayVariable = "SUNRISE_UI_TEST_RELAY"

    /// Relaunch against the relay ``relayVariable`` names, wait for the
    /// harness to found an account on it, and return the account's recovery
    /// code.
    ///
    /// Skips, rather than fails, when no relay is named: the relay is a
    /// separate process the runner cannot start on iOS, and a suite run
    /// without one has nothing to restore from. The harness founds the
    /// account at launch and writes the code into the scratch directory this
    /// test owns (`RelaySeed` in the app), or writes why it could not.
    func relaunchAgainstSeededRelay(file: StaticString = #filePath, line: UInt = #line) throws -> String {
        let relay = ProcessInfo.processInfo.environment[Self.relayVariable]?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard !relay.isEmpty else {
            throw XCTSkip(
                "no relay: start a self-host sunrise-server and pass its base URL as "
                    + "TEST_RUNNER_\(Self.relayVariable)=http://127.0.0.1:PORT"
            )
        }
        let scratch = try XCTUnwrap(scratch, "setUp made a scratch directory", file: file, line: line)
        relaunch(adding: ["-sunrise-ui-test-relay", relay])

        let code = scratch.appending(path: "seeded-recovery-code")
        let failure = scratch.appending(path: "seed-failure")
        let manager = FileManager.default
        let settled = waitUntil(timeout: 60) {
            manager.fileExists(atPath: code.path(percentEncoded: false))
                || manager.fileExists(atPath: failure.path(percentEncoded: false))
        }
        if let reason = try? String(contentsOf: failure, encoding: .utf8) {
            XCTFail("the harness could not seed \(relay): \(reason)", file: file, line: line)
        }
        XCTAssertTrue(settled, "the harness seeded \(relay) within a minute", file: file, line: line)
        return try String(contentsOf: code, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Restore from `code` through onboarding's **Restore from recovery
    /// code**, and close the sheet once the account is back.
    ///
    /// The restore's sign-in is the harness's self-host bearer, so nothing
    /// leaves the app for a browser between the submit and the result.
    func restoreFromRecoveryCode(_ code: String, file: StaticString = #filePath, line: UInt = #line) {
        activate(
            app.buttons["onboarding.restore"],
            named: "onboarding's Restore from recovery code button",
            timeout: 20,
            file: file,
            line: line
        )
        // A vertical-axis `TextField` is a text view on one platform and a
        // text field on the other, so it is found by identifier alone.
        let words = app.descendants(matching: .any)["restore.words"].firstMatch
        activate(words, named: "the recovery code field", file: file, line: line)
        words.typeText(code)

        let submit = app.buttons["restore.submit"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { submit.isEnabled },
            "twenty-four valid words enable Restore",
            file: file,
            line: line
        )
        activate(submit, named: "the sheet's Restore button", file: file, line: line)

        let done = app.buttons["restore.done"]
        let error = app.descendants(matching: .any)["restore.error"].firstMatch
        if !done.appears(within: 120), error.exists {
            XCTFail(
                "the restore failed: \(error.label)",
                file: file,
                line: line
            )
        }
        activate(done, named: "the restored sheet's Done button", file: file, line: line)
        XCTAssertTrue(
            waitUntil(timeout: 10) { !done.exists },
            "Done closed the sheet",
            file: file,
            line: line
        )
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
        guard element.appears(within: timeout) else {
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
    ///
    /// The button is **required**, not waited for and skipped when it is late.
    /// Every caller expects first run, so a Create button that never appears
    /// means the app never left onboarding, and the only honest place to say so
    /// is here: returning quietly let the test fail on its next line instead,
    /// naming a tab or a list that was never the problem. ``activate(_:named:timeout:file:line:)``
    /// already fails on a missing element with a message naming its subject,
    /// and `file`/`line` are forwarded so that failure points at the test.
    func createVault(file: StaticString = #filePath, line: UInt = #line) {
        activate(
            app.buttons["onboarding.create"],
            named: "first run's Create button",
            timeout: 20,
            file: file,
            line: line
        )
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
    ///
    /// `listedHere: false` is for a line the list it was typed into does not
    /// show — `^tomorrow` typed into Today, which holds today's and overdue
    /// tasks only. It waits only for the field to clear and Add to disable,
    /// which proves the line was submitted, not that it was written:
    /// `CaptureModel.takeDraft` clears the field when Add is tapped, before
    /// the commit runs, so a failed write passes this wait too.
    func capture(
        _ text: String,
        landingAs title: String? = nil,
        listedHere: Bool = true,
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

        guard listedHere else {
            XCTAssertTrue(
                waitUntil(timeout: 10) { !add.isEnabled },
                "Add took the line, so the field cleared and Add disabled again",
                file: file,
                line: line
            )
            return
        }
        let expected = title ?? text
        XCTAssertTrue(
            app.staticTexts[expected].appears(within: 10),
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

extension XCUIElement {
    /// `waitForExistence(timeout:)`, without its one-second floor.
    ///
    /// XCTest's own wait does not look before it waits: in the result bundle
    /// of CI run 35624523650 every one of the 166 `waitForExistence` calls the
    /// iOS UI suite made took its first look between 1.00 and 1.10 s after it
    /// was asked, and 162 of them found the element on that first look —
    /// including a second wait on `onboarding.create` issued right after the
    /// first had found it. That floor alone was 172 s of a ten-minute suite.
    ///
    /// So look first, and fall back to XCTest's wait only when the element is
    /// not there yet. The fallback is XCTest's rather than a poll of our own
    /// because its "Waiting …s for … to exist" activity is what the result
    /// bundle's timeline shows, and that timeline is how a flake is read
    /// (#277); the fast path adds no activity, because it waited for nothing.
    ///
    /// The two screenshot walks keep calling `waitForExistence` directly, on
    /// purpose. There a wait is followed by a `shot`, not an assertion, and
    /// the second the floor costs is what lets a pushed screen finish arriving
    /// before it is photographed. Neither walk runs on a pull request.
    func appears(within timeout: TimeInterval) -> Bool {
        exists || waitForExistence(timeout: timeout)
    }
}
