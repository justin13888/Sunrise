import XCTest

/// Screenshot capture for the two walk suites.
///
/// The walks exist so the app can be reviewed as a set of images rather than
/// by driving it — `mise run apple-shots` runs them and writes one PNG per
/// screen into `out/shots/`. A design conversation that has to start with
/// "build it and click around" starts late; this is the artifact that lets it
/// start on time.
///
/// They assert almost nothing, deliberately. `AppNavigationUITests` and
/// `TabShellUITests` are where "this screen rendered and its buttons are
/// wired" is claimed, and a walk that also asserted would abort on the first
/// failure — throwing away every image after it, including the one that would
/// have shown what went wrong.
extension SunriseUITestCase {
    /// Attach a screenshot of the app under a stable, sortable name.
    ///
    /// `app.screenshot()` rather than `XCUIScreen.main.screenshot()`: on macOS
    /// the latter captures the whole display, which means whatever else the
    /// developer had open. Scoping to the application keeps the artifact to
    /// the thing being reviewed.
    ///
    /// `.keepAlways` because the default discards attachments from *passing*
    /// tests, and a passing walk is exactly when the images are wanted.
    func shot(_ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    /// Enough of a vault that no screen has to be photographed empty.
    ///
    /// Five lines, chosen to exercise the parser rather than to look busy:
    /// two priorities, an estimate, a context, a stream, something due
    /// tomorrow so Today and Inbox differ, and one bare capture so the
    /// no-annotation path appears too.
    ///
    /// Everything here goes through the same inline capture bar a user types
    /// into, so the fixture is built by the app rather than injected behind
    /// it — which is the only way the screenshots describe what a user would
    /// actually see.
    func seedFixture() {
        capture("Renew passport #inbox ^today !1 ~1h", landingAs: "Renew passport")
        capture("Email Sara about Q3 ^today !2 ~30m", landingAs: "Email Sara about Q3")
        capture("Buy groceries @home ^today ~45m", landingAs: "Buy groceries @home")
        capture("Write the design doc ^tomorrow !1 ~2h", landingAs: "Write the design doc")
        capture("Call the dentist")
        dismissKeyboard()
    }

    /// Put the software keyboard away.
    ///
    /// A no-op on macOS. On iOS it is load-bearing, and the first run of this
    /// walk is how that was found: the capture bar keeps focus after Add — a
    /// deliberate choice, so a second thought can be typed straight after the
    /// first — and the keyboard it holds up covers the bottom of the screen,
    /// **including the tab bar**. Every tab tap after the seed silently hit
    /// the keyboard instead, and four consecutive screenshots came back
    /// identical.
    ///
    /// The app's own way out is the "Done" button beside the field. Tapping
    /// it here is what a user does, which is the only kind of step this walk
    /// should contain.
    func dismissKeyboard() {
        #if !os(macOS)
        let done = app.buttons["capture.done"]
        if done.waitForExistence(timeout: 2) {
            done.tap()
        }
        #endif
    }
}
