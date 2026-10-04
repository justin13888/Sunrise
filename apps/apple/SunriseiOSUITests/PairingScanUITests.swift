import XCTest

/// The pairing sheet's scanner, driven on the simulator (#464).
///
/// A simulator has no camera, so each test names what the scanner gets
/// instead: a real pairing code drawn as a still image
/// (`-sunrise-ui-test-scan-fixture`), or the camera refused
/// (`-sunrise-ui-test-camera-refused`). Everything after the frame is the app's
/// own — Vision reads the picture, the text goes to the pairing model, and the
/// seam accepts it — so a pass proves a scan reaches the pairing, not that a
/// button exists.
///
/// The harness has no relay and no account, so these sheets run the
/// copy-and-paste script, which is also what makes the outcome observable: a
/// code the seam accepted moves the sheet from step 1 of 8 to step 2.
@MainActor
final class PairingScanUITests: SunriseUITestCase {
    /// Open **Add a device…** on a vault this launch created.
    private func openPairing(flags: [String]) {
        relaunch(adding: ["-sunrise-ui-test-multi-vault"] + flags)
        createVault()
        openSettings()
        let addDevice = app.buttons["account.addDevice"]
        reveal(addDevice, named: "Add a device…")
        activate(addDevice, named: "Add a device…")
        XCTAssertTrue(
            app.buttons["pairing.scan"].waitForExistence(timeout: 10),
            "the pairing sheet opens at the scan"
        )
    }

    private var progress: XCUIElement { app.staticTexts["pairing.progress"] }

    /// A code put in front of the scanner is read, handed to the seam, and
    /// accepted: the sheet moves on to the next leg.
    func testAScannedCodeStartsThePairing() throws {
        openPairing(flags: ["-sunrise-ui-test-scan-fixture"])
        XCTAssertEqual(progress.label, "Step 1 of 8")

        activate(app.buttons["pairing.scan"], named: "Scan the code")
        XCTAssertTrue(
            waitUntil(timeout: 15) { self.progress.label == "Step 2 of 8" },
            "the scanned code was accepted, so the sheet asks for the next message; "
                + "it says \(progress.label)"
        )
        XCTAssertFalse(
            app.descendants(matching: .any)["pairing.scan.viewfinder"].exists,
            "and the scanner closed itself"
        )
    }

    /// A refused camera is one tap from the paste field, and the sheet has not
    /// moved: nothing was scanned, so nothing was accepted.
    func testARefusedCameraFallsBackToPasteInOneTap() throws {
        openPairing(flags: ["-sunrise-ui-test-camera-refused"])

        activate(app.buttons["pairing.scan"], named: "Scan the code")
        XCTAssertTrue(
            app.descendants(matching: .any)["pairing.scan.refused"].waitForExistence(timeout: 10),
            "the scanner says the camera is off rather than showing a black frame"
        )

        let pasteInstead = app.buttons["pairing.scan.pasteInstead"]
        activate(pasteInstead, named: "Paste the code instead")
        XCTAssertTrue(
            waitUntil(timeout: 10) { !pasteInstead.exists },
            "one tap closes the scanner"
        )
        XCTAssertTrue(
            app.descendants(matching: .any)["pairing.paste"].waitForExistence(timeout: 10),
            "and the paste field is what is left"
        )
        XCTAssertEqual(progress.label, "Step 1 of 8")
    }
}
