import XCTest

/// The Account screen's four `account.*` controls, read rather than only
/// written (#287).
///
/// `AccountView` has carried these identifiers for as long as it has had the
/// controls, and until this suite nothing read one of them: a rename, a moved
/// modifier or a row that stopped rendering cost nothing at build time and
/// nothing at test time. Two of the four are load-bearing — switching vault
/// closes the open one, and **Add a device…** starts a pairing that seals a
/// root to new hardware — so each is asserted on screen, enabled, and doing
/// what pressing it promises.
///
/// Two classes because the harness has two configurations. The default is the
/// single-vault session every other suite runs, in which `AccountView` hides
/// the Vaults section outright; ``MultiVaultAccountUITests`` opts into the
/// registry-backed session that puts it on screen.
@MainActor
final class AccountUITests: SunriseUITestCase {
    /// The vim-motions toggle is on the screen and flips the setting.
    ///
    /// The setting persists in the app's `UserDefaults`, which outlives this
    /// test in the simulator's container, so the test reads where it started,
    /// asserts the flip, and puts it back rather than assuming either value.
    func testTheVimModeToggleFlipsAndFlipsBack() throws {
        createVault()
        openSettings()

        let toggle = app.switches["account.vimMode"]
        reveal(toggle, named: "the vim-motions toggle")
        let before = try XCTUnwrap(toggle.value as? String, "the toggle reports a value")

        flip(toggle)
        XCTAssertTrue(
            waitUntil(timeout: 10) { (toggle.value as? String) != before },
            "pressing the vim-motions toggle changes its value from \(before)"
        )

        flip(toggle)
        XCTAssertTrue(
            waitUntil(timeout: 10) { (toggle.value as? String) == before },
            "pressing it again restores \(before), so the next run starts where this one did"
        )
    }

    /// The single-vault harness hides the Vaults section, as `AccountView`
    /// documents.
    ///
    /// The negative half of ``MultiVaultAccountUITests``: without it, a harness
    /// that quietly started building a registry would move every other suite
    /// onto a configuration none of them was written against.
    func testTheSingleVaultHarnessHasNoVaultControls() throws {
        createVault()
        openSettings()

        reveal(app.switches["account.vimMode"], named: "the vim-motions toggle")
        for identifier in ["account.vaultPicker", "account.addVault", "account.addDevice"] {
            XCTAssertFalse(
                app.descendants(matching: .any)[identifier].exists,
                "\(identifier) is absent when the session has no registry"
            )
        }
    }
}

/// The three Account controls that exist only with a vault registry.
@MainActor
final class MultiVaultAccountUITests: SunriseUITestCase {
    override func setUp() async throws {
        try await super.setUp()
        relaunch(adding: ["-sunrise-ui-test-multi-vault"])
    }

    /// The picker and both buttons are on screen and enabled on a vault this
    /// device created, and **Add a device…** opens the pairing sheet.
    ///
    /// A vault created here founded its account, so it holds the signing key
    /// and can sponsor a pairing; a disabled **Add a device…** on it would be
    /// the `canSponsorPairing` snapshot gone wrong.
    func testTheVaultControlsAreOnScreenAndAddADeviceOpensPairing() throws {
        createVault()
        openSettings()

        let picker = app.buttons["account.vaultPicker"]
        let addVault = app.buttons["account.addVault"]
        let addDevice = app.buttons["account.addDevice"]
        reveal(picker, named: "the vault picker")
        for (element, name) in [
            (picker, "the vault picker"),
            (addVault, "Add a vault…"),
            (addDevice, "Add a device…")
        ] {
            XCTAssertTrue(element.waitForExistence(timeout: 10), "\(name) is on screen")
            XCTAssertTrue(element.isEnabled, "\(name) is enabled on a vault this device created")
        }

        activate(addDevice, named: "Add a device…")
        let cancel = app.buttons["pairing.cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 10), "the pairing sheet is presented")
        activate(cancel, named: "the pairing sheet's Cancel button")
        XCTAssertTrue(
            waitUntil(timeout: 10) { !cancel.exists },
            "cancelling the pairing sheet dismisses it"
        )
        XCTAssertTrue(addDevice.waitForExistence(timeout: 10), "and leaves the Account screen behind it")
    }

    /// **Add a vault…** opens a new, empty vault, and the picker switches back.
    ///
    /// The end-to-end claim the two controls make together: the new vault lands
    /// on first run because it has no key and no directory, and choosing the
    /// first vault again closes the new one and reopens the old one under the
    /// key this process created it with — not first run, which would mean the
    /// switch lost the key, and not the locked screen, which would mean it
    /// opened the wrong directory.
    func testAddingAVaultOpensItAndThePickerSwitchesBack() throws {
        createVault()
        openSettings()

        let addVault = app.buttons["account.addVault"]
        reveal(addVault, named: "Add a vault…")
        activate(addVault, named: "Add a vault…")

        let alert = app.alerts["Add a vault"]
        XCTAssertTrue(alert.waitForExistence(timeout: 10), "Add a vault… asks for a name")
        let name = alert.textFields.firstMatch
        activate(name, named: "the new vault's name field")
        name.typeText("Second")
        activate(alert.buttons["Add"], named: "the alert's Add button")

        // A new vault has no key and no directory, so it lands on first run.
        createVault()
        openSettings()

        let picker = app.buttons["account.vaultPicker"]
        reveal(picker, named: "the vault picker")
        activate(picker, named: "the vault picker")
        let first = app.buttons["My vault"]
        XCTAssertTrue(first.waitForExistence(timeout: 10), "the picker lists the first vault")
        XCTAssertTrue(app.buttons["Second"].exists, "and the one just added")
        activate(first, named: "the first vault in the picker")

        let today = app.tabBars.buttons["Today"]
        XCTAssertTrue(
            waitUntil(timeout: 30) { today.exists && !self.app.navigationBars["Settings"].exists },
            "switching closes the Settings sheet and reopens a vault behind it"
        )
        XCTAssertFalse(
            app.buttons["onboarding.create"].exists,
            "the first vault reopened under its key rather than landing on first run"
        )
    }
}

// MARK: - Reaching the screen

extension SunriseUITestCase {
    /// Open Settings from Browse's More menu, where the phone keeps it.
    func openSettings(file: StaticString = #filePath, line: UInt = #line) {
        activate(app.tabBars.buttons["Browse"], named: "the Browse tab", file: file, line: line)
        activate(app.buttons["more"], named: "Browse's More menu", file: file, line: line)
        activate(
            app.buttons["Settings"].firstMatch,
            named: "Settings in the More menu",
            file: file,
            line: line
        )
        XCTAssertTrue(
            app.navigationBars["Settings"].waitForExistence(timeout: 10),
            "the Settings sheet is presented",
            file: file,
            line: line
        )
    }

    /// Scroll the Settings form until `element` is built and hittable.
    ///
    /// The form is a lazily built list: a row below the fold does not exist in
    /// the tree until it is scrolled to, so waiting for it would wait forever.
    func reveal(
        _ element: XCUIElement,
        named name: String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        for _ in 0..<8 {
            if element.exists, element.isHittable { return }
            app.swipeUp()
        }
        XCTAssertTrue(
            element.exists && element.isHittable,
            "\(name) is reachable by scrolling the Settings form",
            file: file,
            line: line
        )
    }

    /// Press a SwiftUI `Toggle` in a form.
    ///
    /// The row is exposed as a switch whose centre is its label, and pressing
    /// the label of a form toggle does nothing on iOS; the nested switch is
    /// the control itself.
    func flip(_ toggle: XCUIElement, file: StaticString = #filePath, line: UInt = #line) {
        let control = toggle.switches.firstMatch
        activate(
            control.exists ? control : toggle,
            named: "the toggle's switch",
            file: file,
            line: line
        )
    }
}
