import Foundation
import Testing

@testable import Sunrise

/// How the relay half of `PairingModel` stops, and how the screens build it
/// (#464).
///
/// ``RelayPairingModelTests`` walks the paths that end in digits or in the
/// copy-and-paste fallback. These are the ones that end in a failure — each
/// of the seam's calls throwing — and the sheet going away mid-wait, plus the
/// two factories the views build the model with.
@MainActor
struct RelayPairingFailureTests {
    typealias FakeSession = RelayPairingModelTests.FakeSession

    nonisolated private static let url = "https://relay.example"

    /// What the relay answers for a pairing that is not there: the seam's own
    /// words for a `404`, from `pairing_relay.rs`.
    nonisolated private static let gone = BindingError.Pairing(
        message: "the pairing ended: the other device cancelled it or it expired. Start again from a new code"
    )

    private func transport(
        _ session: FakeSession,
        acceptFails: (any Error)? = nil
    ) -> RelayTransport {
        RelayTransport(
            relayURL: Self.url,
            bearer: "token",
            offer: { _, _, _ in session },
            accept: { _, _, _ in
                if let acceptFails { throw acceptFails }
                return session
            },
            probe: { _ in true }
        )
    }

    private func existingDevice(_ transport: RelayTransport) -> PairingModel {
        PairingModel(
            intent: .addAnotherDevice,
            relayURL: Self.url,
            relay: transport,
            signedIn: true,
            sponsorRelay: { session in try await (session as? FakeSession)?.sponsorAnywhere() }
        )
    }

    private func newDevice(
        _ transport: RelayTransport,
        relay: RelayPairingModelTests.Relay? = nil
    ) -> PairingModel {
        let model = PairingModel(
            intent: .addThisMac,
            relayURL: Self.url,
            relay: transport,
            signedIn: true,
            adopt: { root, bundle in relay?.adopted = (root, bundle) }
        )
        model.accountEmail = "someone@example.com"
        return model
    }

    private func waitForCancel(_ session: FakeSession) async {
        for _ in 0..<100 where !session.calls.contains("cancel") { await Task.yield() }
    }

    // MARK: - A code the relay cannot carry

    /// The usual first run: the device being added had no account, drew its
    /// code for copy and paste, and the device with the vault scanned it over
    /// the relay. That code reads exactly like a relay code, so the relay
    /// answers that no such pairing exists. The failure says what to do.
    @Test
    func aCodeWithNoPairingOnTheRelayNamesCopyAndPaste() async throws {
        let session = FakeSession(role: .existingDevice, handshakeFails: Self.gone)
        let model = existingDevice(transport(session))

        model.pasted = "SUNRISE-PAIR-MANUAL"
        await model.submit()
        #expect(model.phase == .failed(PairingModel.relayCodeFailed(Self.gone)))
        #expect(PairingModel.relayCodeFailed(Self.gone).contains("“Copy and paste instead”"))
        #expect(model.relaySession == nil)
        #expect(model.transport == .relay, "so the sheet still offers Copy and paste instead")

        model.useManualInstead()
        #expect(model.transport == .manual)
        guard case let .awaiting(prompt) = model.phase else {
            Issue.record("back at the code, by copy and paste, got \(model.phase)")
            return
        }
        #expect(prompt.leg == .code)
    }

    /// A code the seam refuses outright — one naming another relay, or one
    /// drawn with no relay at all — fails the same way, before any request.
    @Test
    func aCodeTheSeamRefusesNamesCopyAndPasteToo() async throws {
        let session = FakeSession(role: .existingDevice)
        let refusal = BindingError.Relay(message: "the code names the relay https://elsewhere.example")
        let model = existingDevice(transport(session, acceptFails: refusal))

        model.pasted = "SUNRISE-PAIR-ELSEWHERE"
        await model.submit()
        #expect(model.phase == .failed(PairingModel.relayCodeFailed(refusal)))
        #expect(model.relaySession == nil)
        #expect(session.calls.isEmpty, "no handshake was started")
    }

    /// On the device being added the code is its own, so the seam's words
    /// stand as they are.
    @Test
    func aHandshakeFailingOnTheNewDeviceIsShownAsIs() async throws {
        let session = FakeSession(role: .newDevice, handshakeFails: Self.gone)
        let model = newDevice(transport(session))

        await model.begin()
        #expect(model.phase == .failed(Self.gone.localizedDescription))
        #expect(model.relaySession == nil)
    }

    /// A rendezvous that opened but has no code to show.
    @Test
    func aSessionWithNoCodeFails() async throws {
        let session = FakeSession(role: .newDevice, payload: nil)
        let model = newDevice(transport(session))

        await model.begin()
        #expect(model.phase == .failed(PairingUIError.noPayload.localizedDescription))
        #expect(session.calls.isEmpty, "no handshake over a code nobody can scan")
    }

    // MARK: - The last messages

    /// "Match", then the sponsor's last three messages fail: the session is
    /// dropped, so the other device stops waiting, and the failure is shown.
    @Test
    func aSponsorThatFailsCancelsTheSession() async throws {
        let failure = BindingError.Relay(message: "the relay went away")
        let session = FakeSession(role: .existingDevice, finishFails: failure)
        let model = existingDevice(transport(session))
        model.pasted = "SUNRISE-PAIR-SCANNED"
        await model.submit()

        await model.confirm(matched: true)
        #expect(model.phase == .failed(failure.localizedDescription))
        #expect(session.calls == ["handshake", "sponsor", "cancel"])
        #expect(model.relaySession == nil)
    }

    /// The same on the device being added, and no vault is adopted.
    @Test
    func aJoinThatFailsCancelsTheSessionAndAdoptsNothing() async throws {
        let failure = BindingError.Pairing(message: "the grant did not open")
        let session = FakeSession(role: .newDevice, finishFails: failure)
        let relay = RelayPairingModelTests.Relay()
        let model = newDevice(transport(session), relay: relay)
        await model.begin()

        await model.confirm(matched: true)
        #expect(model.phase == .failed(failure.localizedDescription))
        #expect(session.calls == ["handshake", "join", "cancel"])
        #expect(relay.adopted == nil)
    }

    // MARK: - The sheet going away

    /// A swipe-down mid-wait ends the rendezvous, as Cancel does.
    @Test
    func dismissingTheSheetMidPairingCancelsTheRendezvous() async throws {
        let session = FakeSession(role: .newDevice)
        let model = newDevice(transport(session))
        await model.begin()

        model.dismissed()
        await waitForCancel(session)
        #expect(session.calls.contains("cancel"))
        #expect(model.relaySession == nil)
        #expect(model.phase == .idle)
    }

    /// After "done" there is nothing left to cancel, and nothing is.
    @Test
    func dismissingAFinishedPairingLeavesItDone() async throws {
        let session = FakeSession(role: .newDevice)
        let model = newDevice(transport(session))
        await model.begin()
        await model.confirm(matched: true)

        model.dismissed()
        for _ in 0..<10 { await Task.yield() }
        #expect(!session.calls.contains("cancel"))
        if case .done = model.phase {} else { Issue.record("expected done, got \(model.phase)") }
    }

    // MARK: - The factories

    /// Nothing on first run has read the account, so `joining` takes the
    /// first look itself: a sign-in still in the Keychain reaches the relay.
    @Test
    func joiningReadsTheAccountBeforeAskingForItsBearer() {
        let account = AccountModel(
            store: StubCredentialStore(value: credentials(accessToken: "access-old")),
            makeDriver: { _, _ in StubLoginDriver() },
            openURL: { _ in }
        )
        #expect(account.accessToken == nil, "unread until something looks")

        let model = PairingModel.joining(relayURL: Self.url, account: account, adopt: { _, _ in })
        #expect(model.transport == .relay)
        #expect(model.relay?.bearer == "access-old")
        #expect(model.notice == nil)
    }

    /// No stored sign-in: copy and paste, and the screen says why.
    @Test
    func joiningWithNoSignInIsCopyAndPaste() {
        let account = AccountModel(
            store: StubCredentialStore(),
            makeDriver: { _, _ in StubLoginDriver() },
            openURL: { _ in }
        )
        let model = PairingModel.joining(relayURL: Self.url, account: account, adopt: { _, _ in })
        #expect(model.transport == .manual)
        #expect(model.notice == PairingModel.notSignedIn)
    }

    /// `sponsoring` signs through the bridge it was given: the open vault's
    /// core is what the seam is handed.
    @Test
    func sponsoringRunsTheSeamThroughTheBridge() async throws {
        let vault = try await TestVault()
        let model = PairingModel.sponsoring(through: vault.bridge, relayURL: Self.url, bearer: "token")
        #expect(model.transport == .relay)
        #expect(model.relay?.bearer == "token")

        let session = FakeSession(role: .existingDevice)
        let sponsor = try #require(model.sponsorRelay)
        try await sponsor(session)
        #expect(session.calls == ["sponsor"])
    }

    /// With no open vault there is nothing to sign with, and it says so.
    @Test
    func sponsoringWithNoVaultRefuses() async throws {
        let model = PairingModel.sponsoring(through: nil, relayURL: Self.url, bearer: nil)
        #expect(model.transport == .manual)
        let sponsor = try #require(model.sponsorRelay)
        await #expect(throws: PairingUIError.noOpenVault) {
            try await sponsor(FakeSession(role: .existingDevice))
        }
    }
}
