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
            activate(app.tabBars.buttons[tab], named: "the \(tab) tab")
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

        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")
        for row in ["Morning", "Evening", "Routines", "Review"] {
            activate(app.buttons["more"], named: "Browse's More menu")
            activate(app.buttons[row].firstMatch, named: "\(row) in the More menu")
            XCTAssertTrue(
                app.navigationBars[row].appears(within: 10),
                "\(row) pushed and titled itself"
            )
            activate(app.navigationBars.buttons.firstMatch, named: "the back button on \(row)")
            XCTAssertTrue(
                app.navigationBars["Browse"].appears(within: 10),
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
    /// software keyboard, which a simulator with a hardware keyboard attached
    /// does not raise. That is the one condition under which this test has
    /// nothing to say, so it says so and skips rather than passing: with no
    /// keyboard there is nothing covering the tab bar, and asserting that an
    /// uncovered tab bar is reachable would be green whatever the app did —
    /// including with the Done button deleted outright.
    func testTheTabBarIsReachableAfterCapturing() throws {
        createVault()
        capture("Renew passport !1", landingAs: "Renew passport")

        try XCTSkipUnless(
            app.keyboards.element.appears(within: 3),
            "no software keyboard on this simulator, so nothing covers the tab bar"
        )

        // Unconditional from here. Past the skip the keyboard is up, so the
        // way out has to exist and has to work.
        let done = app.buttons["capture.done"]
        XCTAssertTrue(done.appears(within: 5), "the keyboard offers Done")
        activate(done, named: "the keyboard's Done button", timeout: 5)

        // The claim of this test, and the reason the message is spelled out
        // here rather than left to `activate`'s default: a tab bar that is
        // present but not hittable is the keyboard still covering it, which is
        // the exact defect this test exists for.
        let browse = app.tabBars.buttons["Browse"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { browse.isHittable },
            "the keyboard is no longer covering the tab bar"
        )
        activate(browse, named: "the Browse tab")
        XCTAssertTrue(
            app.navigationBars["Browse"].appears(within: 10),
            "the tab switched"
        )
    }

    /// The other capture seam.
    ///
    /// Calendar has no inline bar, so `openCapture` presents the sheet instead
    /// of focusing a field — a different path with a different commit
    /// (`AppSurfaces.commitCapture`, which has no local refresh and repaints
    /// through the change stream). The Inbox row at the end is what proves the
    /// write landed — read back out of the core, not a label the sheet drew —
    /// and neither of the sheet's own two labels is asserted on at all, for
    /// the reasons set out where the Add press used to sample them.
    func testTheCaptureSheetCommitsFromAScreenWithNoBar() throws {
        createVault()

        activate(app.tabBars.buttons["Calendar"], named: "the Calendar tab")
        activate(app.buttons["capture"], named: "Calendar's Capture button")

        let field = app.textFields["quick-capture.field"]
        activate(field, named: "the capture sheet's field")
        field.typeText("Book the ferry")

        let add = app.buttons["quick-capture.add"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { add.isEnabled },
            "the capture preview enables Add"
        )
        activate(add, named: "the capture sheet's Add button")

        // Not a wait on `quick-capture.confirmation`. `submit` sets that label
        // and clears it two seconds later (`QuickCaptureView.swift:146-149`),
        // so the evidence this line used to wait for deletes itself: a
        // ten-second `waitForExistence` is protection against an element
        // arriving late and none at all against one already gone. CI run
        // 35446535264 was red on this line on a Rust-only diff, which is a
        // slow runner and not a broken commit.
        //
        // That leaves `quick-capture.confirmation` asserted by no test in this
        // repository, and that is deliberate rather than an oversight. The
        // label exists for two seconds by design — `QuickCaptureView.swift:59-61`
        // contrasts it with the failure label below it — and any assertion on a
        // self-deleting element is the flake this change exists to remove: one
        // that requires the label re-creates the two-second window, and one
        // tolerant enough to pass without it constrains nothing.
        //
        // `quick-capture.failure`, the durable half of the same state, is not
        // sampled here either, and no line of this test witnesses a refusal.
        // A sample taken at this point cannot observe one: the label is set by
        // `submit`'s catch arm (`QuickCaptureView.swift:153`), which runs
        // inside the detached `_Concurrency.Task` opened at `:142` and only
        // once `try await commit(draft)` at `:144` has resumed. `activate`
        // ends at `element.tap()`, and the quiescence XCUITest waits out after
        // a tap is the app's main run loop — not a Task suspended across
        // SwiftUI, a view model, UniFFI, Rust and SQLite. `exists` is an
        // instantaneous query with no wait of its own, so an assertion here is
        // sampled before `failure` is ever assigned, and passes under a
        // refusal exactly as it passes under an acceptance.
        //
        // The only witness this test has is the Inbox row at the end of it
        // (`TabShellUITests.swift:209-212`). It reads the captured line back
        // out of the core through the change stream after a tab switch, so it
        // constrains the write rather than a label the sheet drew — and it has
        // no window to miss, because the row is never taken away again, where
        // both of the sheet's labels are states the view drops. The negative
        // case — a refusal actually observed — needs a seam that can fail a
        // commit on demand, and is issue #295.

        // Cancel takes the sheet and its keyboard away together, which is what
        // makes the tab bar tappable again.
        activate(app.buttons["quick-capture.cancel"], named: "the capture sheet's Cancel button")

        // Spelled out for the same reason as above: "present but not hittable"
        // is the sheet still over it, which is what this line asserts against.
        let browse = app.tabBars.buttons["Browse"]
        XCTAssertTrue(
            waitUntil(timeout: 10) { browse.isHittable },
            "dismissing the sheet freed the tab bar"
        )
        activate(browse, named: "the Browse tab")
        // The cell, not the identifier's own element. The row is a `Label`,
        // and on iOS SwiftUI splits that into an image and a static text which
        // both inherit the identifier — so the query is ambiguous, and the
        // image it resolves to first is not hittable on its own. The thing a
        // finger lands on is the row.
        let inbox = app.cells.containing(.staticText, identifier: "sidebar.inbox").firstMatch
        activate(inbox, named: "the Inbox row in Browse")

        XCTAssertTrue(
            app.staticTexts["Book the ferry"].appears(within: 10),
            "what the sheet captured is in the Inbox"
        )
    }
}

/// The sidebar's add controls, by **name** rather than by identifier.
///
/// Deliberately not a test of the identifier. The two are separate
/// accessibility attributes, and a control findable only by identifier is
/// findable by this suite and silent to VoiceOver — which is the half of the
/// original defect that a test asserting on identifiers would have missed.
///
/// It runs here rather than in the macOS suite because `BrowseSidebar` is
/// shared: iOS renders it as the Browse tab, and a simulator runner needs none
/// of the machine grants a macOS XCUITest does. The claim is about the view,
/// so the cheaper platform to make it on is the right one.
@MainActor
final class SidebarAddButtonTests: SunriseUITestCase {
    func testTheAddControlsCarryNamesAndNotOnlyIdentifiers() throws {
        createVault()
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab")

        // On iOS the two actions are a toolbar menu rather than a bottom bar:
        // the space under an iPhone list belongs to the tab bar. Open it, then
        // assert on the items inside.
        activate(app.buttons["sidebar.add"], named: "Browse's Add menu")

        for name in ["New stream", "New context"] {
            XCTAssertTrue(
                app.buttons[name].appears(within: 10),
                "\(name) is in the accessibility tree under its own name"
            )
        }
    }
}
