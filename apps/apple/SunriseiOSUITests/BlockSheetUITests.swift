import XCTest

/// The two calendar sheets present with a commit control a phone can see.
///
/// Issue #174 inferred the opposite from framework behaviour — `.toolbar` items
/// outside a navigation container have nowhere to render on iOS, and a sheet
/// does not supply one — and asked for the inference to be confirmed before
/// anything was changed. This suite is that confirmation, and then the
/// regression test for the fix: run against the pre-fix tree both tests fail on
/// the commit control, which is a modal a user can open and cannot complete.
///
/// It runs here and not on the Mac for two reasons. The defect is iOS-only: a
/// macOS sheet draws a toolbar whether or not anything asked it to, which is
/// why #173's screenshot pass over the Mac could not have caught this. And
/// `SunriseUITests` is `skipped: true` in the `Sunrise` scheme, so `mise run
/// macos-app` executes no UI tests at all — a Mac-side test would be a test
/// nothing runs.
///
/// **Reaching the draft sheet is most of the work**, which is also why #173
/// could not screenshot it: the only thing that opens it is a drag on the day
/// column, and there is no button, menu item, keyboard shortcut or deep link
/// that does. Hence the press-then-drag in `openDraftSheet`, which is the only
/// coordinate-driven interaction in either UI suite.
final class BlockSheetUITests: SunriseUITestCase {
    /// `BlockDraftSheetView` — Cancel and Add block.
    func testTheBlockDraftSheetShowsItsCommitControls() {
        createVault()
        openDraftSheet()

        // By label rather than by identifier, deliberately and unlike the rest
        // of this suite. The identifiers are part of the fix; the labels are
        // what the pre-fix tree already had, so asserting on them is what makes
        // this a confirmation of #174's inference rather than a test of the
        // change beside it. The `named:` argument carries the diagnosis into
        // the failure message.
        XCTAssertTrue(
            app.buttons["Add block"].waitForExistence(timeout: 5),
            "the draft sheet must offer a way to commit the block it is asking "
                + "about — without one it is a modal a phone can open and not finish"
        )
        XCTAssertTrue(
            app.buttons["Cancel"].exists,
            "and a way to abandon it that is not a guessed-at swipe"
        )

        activate(app.buttons["Add block"], named: "the draft sheet's Add block")
        XCTAssertTrue(
            app.descendants(matching: .any)["calendar-block"].waitForExistence(timeout: 5),
            "pressing it must actually create the block, so this suite fails on a "
                + "control that is visible and inert as well as on one that is absent"
        )
    }

    /// `BlockEditorView` — Cancel, Delete and Save.
    ///
    /// Reached through the draft sheet, because a block has to exist before it
    /// can be edited and the draft sheet is the only thing that makes one.
    func testTheBlockEditorSheetShowsItsCommitControls() {
        createVault()
        openDraftSheet()
        activate(app.buttons["Add block"], named: "the draft sheet's Add block")
        XCTAssertTrue(
            app.descendants(matching: .any)["calendar-block"].waitForExistence(timeout: 5),
            "the block reached the grid"
        )

        // Tapped by coordinate, in the middle of the drag that drew it, and not
        // by `.tap()` on the `calendar-block` element. SwiftUI folds the day
        // column into one accessibility element on iOS, so that element's frame
        // is the whole 1056-point column and its centre is 500 points below the
        // block — a tap there hits empty grid and opens nothing.
        Self.blockCentre(in: app).tap()

        XCTAssertTrue(
            app.buttons["Save"].waitForExistence(timeout: 5),
            "the editor must offer a way to keep an edit"
        )
        XCTAssertTrue(app.buttons["Cancel"].exists, "and a way to drop one")
        XCTAssertTrue(app.buttons["Delete"].exists, "and the delete the Mac has")
    }

    /// Both sheets name themselves, so the bar that carries the buttons is not
    /// an unlabelled strip of controls.
    func testTheSheetsAreTitled() {
        createVault()
        openDraftSheet()
        XCTAssertTrue(
            app.navigationBars["New block"].waitForExistence(timeout: 5),
            "the draft sheet's bar must say what it is for"
        )
    }

    /// Drag down the day column to draw a block, and wait for the sheet.
    ///
    /// `press(forDuration:thenDragTo:)` rather than a plain drag: the column
    /// lives inside a vertical `ScrollView`, and a fast vertical swipe is
    /// claimed by the scroll pan before `DragGesture(minimumDistance: 4)` ever
    /// sees it. The press is what makes the content gesture win.
    private func openDraftSheet() {
        activate(app.tabBars.buttons["Calendar"], named: "the Calendar tab")
        // The span picker, which carries `calendar-toolbar` down onto its own
        // controls rather than exposing a container of that name.
        XCTAssertTrue(
            app.segmentedControls["calendar-toolbar"].waitForExistence(timeout: 10),
            "the calendar is on screen before anything is dragged on it"
        )

        // Coordinates, and deliberately not an identifier on the day column.
        // Naming that container makes SwiftUI fold the whole grid into a
        // single accessibility element — measured, not guessed: with the
        // identifier on it the hierarchy showed one `StaticText` labelled with
        // the block's title and no `calendar-block` inside it, which would
        // have cost VoiceOver every individual block to buy this suite one
        // convenient handle. These two offsets land in the middle of the
        // visible grid, clear of the 52-point hour gutter on the left, the
        // toolbar above and the tab bar below.
        let start = app.coordinate(withNormalizedOffset: CGVector(dx: dragX, dy: dragTop))
        let end = app.coordinate(withNormalizedOffset: CGVector(dx: dragX, dy: dragBottom))
        start.press(forDuration: 0.6, thenDragTo: end)

        XCTAssertTrue(
            app.textFields["block-title"].waitForExistence(timeout: 10),
            "the drag must open the draft sheet; if this fails the rest of the "
                + "suite is asserting about a sheet that never appeared"
        )
    }

    private static let dragX: CGFloat = 0.6
    private static let dragTop: CGFloat = 0.45
    private static let dragBottom: CGFloat = 0.60

    private var dragX: CGFloat { Self.dragX }
    private var dragTop: CGFloat { Self.dragTop }
    private var dragBottom: CGFloat { Self.dragBottom }

    /// The middle of the block the drag in `openDraftSheet` draws.
    private static func blockCentre(in app: XCUIApplication) -> XCUICoordinate {
        app.coordinate(
            withNormalizedOffset: CGVector(dx: dragX, dy: (dragTop + dragBottom) / 2)
        )
    }
}
