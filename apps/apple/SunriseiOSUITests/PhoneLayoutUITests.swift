import XCTest

/// What an iPhone's own chrome does to a list that was written for a window.
///
/// [#54](https://github.com/justin13888/Sunrise/issues/54) records two things
/// the screenshot walk photographed and could not distinguish between: content
/// drawn *under* iOS 26's floating tab bar, which is what a translucent bar is
/// for, and content a user cannot scroll out from under it, which is a defect.
/// A still image shows the same thing either way. This drives it.
@MainActor
final class PhoneLayoutUITests: SunriseUITestCase {
    /// The bottom of Browse's list can be brought out from under the tab bar.
    ///
    /// **Contexts** is the header [#54](https://github.com/justin13888/Sunrise/issues/54)
    /// names, and it is the last thing in the sidebar. At rest it sits under
    /// the floating tab bar, which is what a translucent bar is *for* — the
    /// claim worth testing is that a finger can bring it out, not that it
    /// starts clear.
    ///
    /// No fixture: the nine fixed destinations plus the Streams and Contexts
    /// sections already run past the bottom of an iPhone, and a captured
    /// `@home` does not become a context — the shared parser leaves it in the
    /// title, which is what `seedFixture`'s own `landingAs:` records.
    func testTheLastSidebarSectionScrollsClearOfTheFloatingTabBar() throws {
        createVault()

        app.tabBars.buttons["Browse"].tap()
        XCTAssertTrue(
            app.navigationBars["Browse"].waitForExistence(timeout: 10),
            "Browse is on screen"
        )

        let header = app.staticTexts["Contexts"]
        XCTAssertTrue(header.waitForExistence(timeout: 10), "the Contexts header is in the sidebar")

        for _ in 0..<3 where !header.isHittable {
            app.swipeUp()
        }
        XCTAssertTrue(
            waitUntil(timeout: 5) { header.isHittable },
            "the last section scrolls out from under the tab bar rather than staying beneath it"
        )
    }

    /// The calendar toolbar fits the width it has.
    ///
    /// The stepper's three controls and the date between them are the half
    /// that ran off the trailing edge on a phone; **Today** is the one with a
    /// name a query can find, and it being hittable is the claim.
    func testTheCalendarToolbarFitsAnIPhone() throws {
        createVault()

        app.tabBars.buttons["Calendar"].tap()

        // The chevrons rather than the **Today** between them, which shares a
        // name with the tab bar's first tab and so matches two elements — a
        // query that is ambiguous rather than wrong, and one whose `isHittable`
        // says nothing about this toolbar.
        for name in ["Previous", "Next"] {
            let control = app.buttons[name]
            XCTAssertTrue(control.waitForExistence(timeout: 10), "\(name) is on screen")
            XCTAssertTrue(
                control.isHittable,
                "\(name) is inside the window rather than off its edge"
            )
        }

        // The other end of the toolbar, so a layout that fitted by dropping
        // half of itself would not pass.
        let span = app.segmentedControls.buttons["Week"]
        XCTAssertTrue(span.waitForExistence(timeout: 5), "the span picker is on screen")
        XCTAssertTrue(span.isHittable, "and it is reachable too")
    }
}
