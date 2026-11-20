import XCTest

/// Restoring an account from its recovery code, driven through the Mac window
/// against a seeded relay (#458).
///
/// `SunriseTests/RestoreFromCodeModelTests` covers the model against a stub
/// relay and `crates/sunrise-e2e/tests/apple_recovers_an_account.rs` covers the
/// seam against a real one; this is the screen between them. The harness
/// founds an account holding one task on the relay the runner names, and the
/// test types that account's code into onboarding's restore sheet.
///
/// The last assertion is the one that matters: a restore that opened an empty
/// vault would pass every step before it.
@MainActor
final class RestoreFromCodeUITests: SunriseUITestCase {
    func testTheRecoveryCodeRestoresTheAccountAndItsTask() throws {
        let code = try relaunchAgainstSeededRelay()
        restoreFromRecoveryCode(code)

        activate(
            app.descendants(matching: .any)["sidebar.inbox"],
            named: "the Inbox row in the sidebar",
            timeout: 20
        )
        XCTAssertTrue(
            app.staticTexts["Renew passport"].appears(within: 20),
            "the task the lost device wrote is in the restored Inbox"
        )
    }
}
