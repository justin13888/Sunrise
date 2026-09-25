import XCTest

/// The recovery ceremony, driven rather than skipped.
///
/// Every other UI test launches without ``UITestHarness/recoveryFlag`` and so
/// never sees this sheet — which it has to, because the sheet covers the whole
/// window the moment a vault is created and every suite here begins by creating
/// one. Suppressing it for them and testing it nowhere would leave the one
/// screen that decides whether a user can ever recover their account as the
/// only screen with no coverage at all, which is how #181 happened in the first
/// place.
///
/// So this class opts back in by name. It is the only place the ceremony can
/// flake, and the only place it is asserted.
@MainActor
final class RecoveryCeremonyUITests: SunriseUITestCase {
    override func setUp() async throws {
        try await super.setUp()
        app.terminate()
        app.launchArguments.append("-sunrise-ui-test-recovery")
        app.launch()
    }

    /// Creating a vault presents the ceremony, and a device that cannot reach a
    /// relay is told so rather than shown a code for a blob nobody stored.
    ///
    /// A UI test has no relay address, no account email and no token, which is
    /// exactly the state `RecoverySetupError.notConfigured` describes. The
    /// assertion that matters is not the wording: it is that the sheet reaches
    /// a **terminal, honest** state instead of presenting twenty-four words the
    /// user would believe were backed by something.
    func testTheCeremonyAppearsAndSaysWhenItCannotStoreTheBlob() throws {
        createVault()

        let later = app.buttons["recovery.later"]
        XCTAssertTrue(
            later.appears(within: 30),
            "a vault created here cannot reach a relay, so the ceremony offers 'Not now'"
        )
        XCTAssertFalse(
            app.otherElements["recovery.words"].exists,
            "no code is shown for a blob that was never uploaded"
        )
    }

    /// Dismissing it hands the app back.
    ///
    /// The ceremony is deferrable by design — a user who cannot set recovery up
    /// this minute must still be able to use the app — and a sheet that would
    /// not go away would take every screen behind it with it.
    func testDismissingTheCeremonyLeavesTheAppUsable() throws {
        createVault()

        let later = app.buttons["recovery.later"]
        XCTAssertTrue(later.appears(within: 30), "the ceremony is on screen")
        activate(later, named: "the ceremony's 'Not now' button", timeout: 30)

        XCTAssertTrue(
            waitUntil(timeout: 10) { !later.exists },
            "the sheet is gone rather than merely dismissed behind itself"
        )
        activate(app.tabBars.buttons["Today"], named: "the Today tab, once the sheet is gone")
        XCTAssertTrue(
            waitUntil(timeout: 10) { self.app.state == .runningForeground },
            "and the app is still running"
        )
    }
}
