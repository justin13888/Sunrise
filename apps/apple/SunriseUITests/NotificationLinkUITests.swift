import XCTest

/// Where a notification's tap lands, driven through the real window.
///
/// A notification body carries a `sunrise://` link, and a tap hands it to the
/// app the way any URL is opened — so opening the URL is the tap, without a
/// notification service the test cannot post to. One per kind with a screen
/// of its own (`docs/08-features/notifications.md` §Testing): the brief, the
/// triage nudge, and a security alert, which opens Settings with the
/// per-kind switches in it.
@MainActor
final class NotificationLinkUITests: SunriseUITestCase {
    private func open(_ link: String) throws {
        let url = try XCTUnwrap(URL(string: link))
        app.open(url)
    }

    func testTheMorningBriefLinkOpensTheMorningBrief() throws {
        createVault()

        try open("sunrise://morning")

        XCTAssertTrue(
            app.descendants(matching: .any)["brief.morning"].appears(within: 10),
            "the brief's tap lands on the morning summary"
        )
    }

    func testTheTriageNudgeLinkOpensTheTriageSection() throws {
        createVault()

        try open("sunrise://triage")

        XCTAssertTrue(
            app.descendants(matching: .any)["brief.morning"].appears(within: 10),
            "the nudge's tap lands on the morning summary, whose triage section it is about"
        )
    }

    func testASecurityAlertLinkOpensSettingsWithEveryKind() throws {
        createVault()

        try open("sunrise://settings/security")

        XCTAssertTrue(
            app.descendants(matching: .any)["notifications.kind.recovery_code"].appears(within: 10),
            "the alert's tap opens Settings, which lists the recovery-code alert as always on"
        )
        XCTAssertTrue(
            app.descendants(matching: .any)["notifications.kind.morning_brief"].exists,
            "and a switch for every other kind beside it"
        )
    }
}
