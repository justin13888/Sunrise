import Foundation
import Testing

@testable import Sunrise

/// The relay half of `PairingModel`: scan, compare, done (#464).
///
/// The seam's own end-to-end proof is `crates/sunrise-e2e/tests/pairing_over_the_relay.rs`,
/// which pairs two `RelayPairing`s over a live relay. What is left to prove
/// here is this screen's half: that it calls the seam in that order, hands it
/// the scanned text untouched, and falls back to copy and paste — saying why —
/// whenever the relay cannot carry the pairing. So the relay is a fake, and
/// every call it receives is recorded.
@MainActor
struct RelayPairingModelTests {
    /// A `RelayPairing` that answers at once and writes down what it was asked.
    final class FakeSession: RelayPairingProtocol, @unchecked Sendable {
        private let lock = NSLock()
        private var log: [String] = []
        private let payload: String?
        private let pairingRole: PairingRole
        private let digits: String
        /// What `handshake` throws, when it is to fail.
        private let handshakeFails: (any Error)?
        /// What `join` and `sponsor` throw, when the last messages are to fail.
        private let finishFails: (any Error)?

        init(
            role: PairingRole,
            payload: String? = "SUNRISE-PAIR-FAKE",
            digits: String = "123456",
            handshakeFails: (any Error)? = nil,
            finishFails: (any Error)? = nil
        ) {
            pairingRole = role
            self.payload = payload
            self.digits = digits
            self.handshakeFails = handshakeFails
            self.finishFails = finishFails
        }

        var calls: [String] {
            lock.lock()
            defer { lock.unlock() }
            return log
        }

        private func record(_ call: String) {
            lock.lock()
            defer { lock.unlock() }
            log.append(call)
        }

        func cancel() async { record("cancel") }
        func handshake() async throws -> String {
            record("handshake")
            if let handshakeFails { throw handshakeFails }
            return digits
        }
        func join(nickname: String, platform: String, seedS: Data, seedD: Data) async throws -> PairedBundle {
            record("join")
            if let finishFails { throw finishFails }
            #expect(seedS.count == 32 && seedD.count == 32, "both seeds are 32 bytes from the CSPRNG")
            #expect(seedS != seedD, "and they are two draws, not one")
            return PairedBundle(vaultRoot: Data(repeating: 7, count: 32), payloadBytes: Data([1, 2, 3]))
        }
        func qrPayload() -> String? { payload }
        func reject() async { record("reject") }
        func role() -> PairingRole { pairingRole }
        func sponsor(core: SunriseCore) async throws { try await sponsorAnywhere() }
        /// `sponsor`, for a model whose closure has no core to hand it.
        func sponsorAnywhere() async throws {
            record("sponsor")
            if let finishFails { throw finishFails }
        }
    }

    /// What the transport was asked to do, across the model's whole life.
    @MainActor
    final class Relay {
        var offered = 0
        var accepted: [String] = []
        var sponsored = 0
        var adopted: (root: Data, bundle: Data)?
    }

    nonisolated private static let url = "https://relay.example"

    private func transport(
        _ relay: Relay,
        answers: Bool = true,
        session: FakeSession,
        offerFails: (any Error)? = nil
    ) -> RelayTransport {
        RelayTransport(
            relayURL: Self.url,
            bearer: "token",
            offer: { url, bearer, _ in
                await MainActor.run { relay.offered += 1 }
                #expect(url == Self.url && bearer == "token", "the seam gets this device's relay and bearer")
                if let offerFails { throw offerFails }
                return session
            },
            accept: { text, url, bearer in
                MainActor.assumeIsolated { relay.accepted.append(text) }
                #expect(url == Self.url && bearer == "token")
                return session
            },
            probe: { _ in answers }
        )
    }

    private func newDevice(_ relay: Relay, transport: RelayTransport) -> PairingModel {
        let model = PairingModel(
            intent: .addThisMac,
            relayURL: Self.url,
            relay: transport,
            signedIn: true,
            adopt: { root, bundle in relay.adopted = (root, bundle) }
        )
        model.accountEmail = "someone@example.com"
        return model
    }

    private func existingDevice(_ relay: Relay, transport: RelayTransport) -> PairingModel {
        PairingModel(
            intent: .addAnotherDevice,
            relayURL: Self.url,
            relay: transport,
            signedIn: true,
            sponsorRelay: { _ in relay.sponsored += 1 }
        )
    }

    /// The whole of the new device's side: code, digits, vault.
    @Test
    func theNewDeviceShowsTheCodeThenTheDigitsThenOpensTheVault() async throws {
        let relay = Relay()
        let session = FakeSession(role: .newDevice)
        let model = newDevice(relay, transport: transport(relay, session: session))
        #expect(model.transport == .relay)
        #expect(model.notice == nil, "nothing to explain when the relay carries it")

        await model.begin()
        #expect(relay.offered == 1)
        #expect(model.phase == .comparing(sas: "123456"), "the handshake ran and the digits are up")
        #expect(model.progress?.leg == 2 && model.progress?.of == 3, "scan, compare, done")
        #expect(model.role == .newDevice)

        await model.confirm(matched: true)
        #expect(session.calls == ["handshake", "join"])
        #expect(relay.adopted?.root == Data(repeating: 7, count: 32), "the bundle the seam opened is adopted")
        #expect(relay.adopted?.bundle == Data([1, 2, 3]))
        if case .done = model.phase {} else { Issue.record("expected done, got \(model.phase)") }
    }

    /// The scanned string goes to `RelayPairing.accept` exactly as read: the
    /// seam decodes it, and a client that trimmed or reparsed it could only
    /// disagree with the encoder.
    @Test
    func theExistingDeviceHandsTheScannedTextToTheSeamAndSponsors() async throws {
        let relay = Relay()
        let session = FakeSession(role: .existingDevice, payload: nil)
        let model = existingDevice(relay, transport: transport(relay, session: session))
        guard case let .awaiting(prompt) = model.phase else {
            Issue.record("the device with the vault starts at the scan")
            return
        }
        #expect(prompt.leg == .code)
        #expect(model.progress?.leg == 1 && model.progress?.of == 3)

        model.pasted = "SUNRISE-PAIR-SCANNED"
        await model.submit()
        #expect(relay.accepted == ["SUNRISE-PAIR-SCANNED"])
        #expect(model.phase == .comparing(sas: "123456"))

        await model.confirm(matched: true)
        #expect(relay.sponsored == 1, "the open vault signs the cert, through the bridge")
        if case .done = model.phase {} else { Issue.record("expected done, got \(model.phase)") }
    }

    /// "They're different" tells the other device through the relay, and is
    /// final here.
    @Test
    func sayingTheDigitsDifferRejectsThroughTheRelay() async throws {
        let relay = Relay()
        let session = FakeSession(role: .newDevice)
        let model = newDevice(relay, transport: transport(relay, session: session))
        await model.begin()

        await model.confirm(matched: false)
        #expect(model.phase == .mismatch)
        #expect(session.calls == ["handshake", "reject"])
        #expect(relay.adopted == nil, "no vault, and no join was attempted")

        await model.confirm(matched: true)
        #expect(model.phase == .mismatch, "there is nothing left to confirm")
    }

    /// A relay that does not answer costs the user nothing but a sentence: the
    /// manual flow starts at once, and the screen says why.
    @Test
    func aRelayThatDoesNotAnswerFallsBackToCopyAndPasteAndSaysSo() async throws {
        let relay = Relay()
        let session = FakeSession(role: .newDevice)
        let model = newDevice(relay, transport: transport(relay, answers: false, session: session))

        await model.begin()
        #expect(relay.offered == 0, "no session was opened against a relay that did not answer")
        #expect(model.transport == .manual)
        #expect(model.notice == PairingModel.relayDidNotAnswer(Self.url))
        guard case let .handOff(handOff) = model.phase else {
            Issue.record("the manual code is on screen, got \(model.phase)")
            return
        }
        #expect(handOff.leg == .code && handOff.drawsCode)
        #expect(model.progress?.of == 8, "and it is the eight-leg script from here")
        #expect(model.handshakeStep == .handshaking, "over a DevicePairing of its own")
    }

    /// A relay that answers its health check and then refuses the session — over
    /// its pair-attempt limit, say — falls back the same way.
    @Test
    func aRelayThatRefusesTheSessionFallsBackToo() async throws {
        let relay = Relay()
        let session = FakeSession(role: .newDevice)
        let refusal = BindingError.Relay(message: "the relay refused a new pairing")
        let model = newDevice(relay, transport: transport(relay, session: session, offerFails: refusal))

        await model.begin()
        #expect(relay.offered == 1)
        #expect(model.transport == .manual)
        #expect(model.notice == PairingModel.relayRefused(refusal))
        if case .handOff = model.phase {} else {
            Issue.record("expected the manual code, got \(model.phase)")
        }
    }

    /// The device with the vault cannot carry on with a relay code by hand —
    /// the other device is waiting on the relay — so it goes back to the scan,
    /// in manual mode, and says what both have to do.
    @Test
    func theExistingDeviceThatCannotReachTheRelayAsksForAFreshCode() async throws {
        let relay = Relay()
        let session = FakeSession(role: .existingDevice)
        let model = existingDevice(relay, transport: transport(relay, answers: false, session: session))

        model.pasted = "SUNRISE-PAIR-SCANNED"
        await model.submit()
        #expect(relay.accepted.isEmpty)
        #expect(model.transport == .manual)
        #expect(model.notice == PairingModel.relayDidNotAnswerHere(Self.url))
        guard case let .awaiting(prompt) = model.phase else {
            Issue.record("back at the scan, got \(model.phase)")
            return
        }
        #expect(prompt.leg == .code)
    }

    /// No relay, or no account to present to one: copy and paste from the
    /// start, and the screen names which.
    @Test
    func withoutARelayOrAnAccountItIsCopyAndPasteAndSaysWhich() {
        let noRelay = PairingModel(intent: .addAnotherDevice, relayURL: "")
        #expect(noRelay.transport == .manual)
        #expect(noRelay.notice == PairingModel.noRelayConfigured)

        let signedOut = PairingModel(intent: .addThisMac, relayURL: Self.url, signedIn: false)
        #expect(signedOut.transport == .manual)
        #expect(signedOut.notice == PairingModel.notSignedIn)

        #expect(RelayTransport.live(relayURL: Self.url, bearer: nil) == nil)
        #expect(RelayTransport.live(relayURL: "  ", bearer: "token") == nil)
        #expect(RelayTransport.live(relayURL: Self.url, bearer: "token") != nil)
    }

    /// The user's own way off the relay drops the rendezvous and starts the
    /// eight legs, with no notice — it was their choice.
    @Test
    func choosingCopyAndPasteCancelsTheRendezvous() async throws {
        let relay = Relay()
        let session = FakeSession(role: .newDevice)
        let model = newDevice(relay, transport: transport(relay, session: session))
        await model.begin()

        model.useManualInstead()
        #expect(model.transport == .manual)
        #expect(model.notice == nil)
        #expect(model.phase == .idle, "back at the account form, to mint a manual code")
        #expect(model.progress == nil)
        // The seam's cancel runs on a task of its own; give it the turn.
        for _ in 0..<100 where !session.calls.contains("cancel") { await Task.yield() }
        #expect(
            session.calls.contains("cancel"),
            "the relay session is dropped, so the other side stops waiting"
        )
    }
}
