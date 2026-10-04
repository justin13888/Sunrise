import Foundation

/// What a pairing needs from the relay, and the three calls that reach it.
///
/// The seam is `RelayPairing` (`crates/sunrise-core-bindings/src/pairing_relay.rs`):
/// it moves the six messages through the relay's rendezvous with the same
/// state machine the copy-and-paste script drives, so this type adds no
/// protocol of its own. It exists so a test can stand in for the network —
/// the three closures are the only places this app touches the relay during
/// a pairing, and a model built over fakes walks exactly the path the real one
/// does.
struct RelayTransport: Sendable {
    /// The relay this device is configured for. The seam sends the bearer here
    /// and nowhere else, and refuses a scanned code that names another relay.
    let relayURL: String
    /// This account's access token. The relay binds the rendezvous to the
    /// account it names, and both devices must present one for that account.
    let bearer: String

    /// `RelayPairing.offer`: mint the code and open the rendezvous.
    var offer: @Sendable (_ relayURL: String, _ bearer: String, _ accountEmail: String) async throws
        -> any RelayPairingProtocol
    /// `RelayPairing.accept`: join the rendezvous a scanned code names. The
    /// text goes in exactly as scanned; the seam decodes it.
    var accept: @Sendable (_ qrPayload: String, _ relayURL: String, _ bearer: String) throws
        -> any RelayPairingProtocol
    /// Whether the relay answers at all, asked before either of the above.
    ///
    /// Not a nicety. The seam retries a relay it cannot reach for up to
    /// `WAIT_CAP` — five and a half minutes — because inside a pairing a
    /// dropped packet should not cost a rescan. Before one has started, the
    /// same patience is a spinner that outlasts the user, and the manual flow
    /// works with no relay at all.
    var probe: @Sendable (_ relayURL: String) async -> Bool

    /// The real relay, or `nil` when this device has no relay or no bearer to
    /// present to one — in which case the pairing runs by copy and paste.
    static func live(relayURL: String, bearer: String?) -> RelayTransport? {
        let url = relayURL.trimmed
        guard !url.isEmpty, let bearer, !bearer.isEmpty else { return nil }
        return RelayTransport(
            relayURL: url,
            bearer: bearer,
            offer: { url, bearer, email in
                try await RelayPairing.offer(relayUrl: url, bearer: bearer, accountEmail: email)
            },
            accept: { payload, url, bearer in
                try RelayPairing.accept(qrPayload: payload, relayUrl: url, bearer: bearer)
            },
            probe: { url in await RelayProbe.answers(relayURL: url) }
        )
    }
}

/// `GET /api/v1/health`, with a short timeout.
///
/// The liveness route (`docs/06-server/api.md` §Health): it consults nothing,
/// needs no bearer and is rate-limited apart from everything a pairing uses,
/// so asking it costs the pairing nothing. Five seconds, because the answer
/// decides whether the user is shown a spinner or a working fallback.
enum RelayProbe {
    static let timeout: TimeInterval = 5

    static func answers(relayURL: String) async -> Bool {
        guard let base = URL(string: relayURL.trimmed) else { return false }
        var request = URLRequest(url: base.appending(path: "api/v1/health"))
        request.timeoutInterval = timeout
        request.cachePolicy = .reloadIgnoringLocalCacheData
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = timeout
        configuration.timeoutIntervalForResource = timeout
        let session = URLSession(configuration: configuration)
        defer { session.finishTasksAndInvalidate() }
        guard let (_, response) = try? await session.data(for: request) else { return false }
        return (response as? HTTPURLResponse)?.statusCode == 200
    }
}

/// The relay half of the script: three phases where the manual flow has eight
/// legs.
///
/// Scan, compare, done. The relay carries every message, so neither device
/// shows anything but the code and the six digits; what the user does is the
/// part no transport can do for them — point a camera, and say whether the
/// digits match.
///
/// # When it is not the relay
///
/// The manual flow stays, for a device with no relay configured, no account
/// signed in, or a relay that does not answer. Each of those is decided
/// before a session is opened, and the screen says which one it was
/// (``notice``). The user can also take the manual flow by choice at any
/// point, which is the only way out when one device reaches the relay and the
/// other does not.
extension PairingModel {
    /// Which transport carries this pairing's messages.
    enum Transport: Equatable {
        /// `RelayPairing`: scan, compare, done.
        case relay
        /// `DevicePairing`, with the user as the transport: eight legs.
        case manual
    }

    /// The device that holds the vault, as `AccountView` opens it: signing
    /// through `bridge` for both transports, over the relay when `relayURL`
    /// and `bearer` are both here.
    static func sponsoring(through bridge: CoreBridge?, relayURL: String, bearer: String?) -> PairingModel {
        PairingModel(
            intent: .addAnotherDevice,
            relayURL: relayURL,
            relay: .live(relayURL: relayURL, bearer: bearer),
            signedIn: bearer != nil,
            sealOffer: { pairing in
                guard let bridge else { throw PairingUIError.noOpenVault }
                return try await bridge.sendPairingOffer(to: pairing)
            },
            sealGrant: { pairing, request in
                guard let bridge else { throw PairingUIError.noOpenVault }
                return try await bridge.sendPairingGrant(to: pairing, request: request)
            },
            sponsorRelay: { pairing in
                guard let bridge else { throw PairingUIError.noOpenVault }
                try await bridge.sponsor(pairing)
            }
        )
    }

    /// The device being added, as `OnboardingView` and `LockedView` open it.
    ///
    /// `bearer` is the process's account, which outlives any vault: a device
    /// that signed in before it had one pairs over the relay, and one that has
    /// not pairs by copy and paste and is told why.
    static func joining(
        relayURL: String,
        bearer: String?,
        adopt: @escaping (Data, Data) async -> Void
    ) -> PairingModel {
        PairingModel(
            intent: .addThisMac,
            relayURL: relayURL,
            relay: .live(relayURL: relayURL, bearer: bearer),
            signedIn: bearer != nil,
            adopt: adopt
        )
    }

    /// The relay's progress line: code, digits, done.
    static let relaySteps = 3

    /// Start as the device being added, over the relay.
    func beginOverRelay(_ relay: RelayTransport) async {
        attempt += 1
        let mine = attempt
        relayStep = 1
        phase = .working(PairingModel.reachingRelay)
        guard await relay.probe(relay.relayURL) else {
            guard mine == attempt else { return }
            fallBackToManual(because: PairingModel.relayDidNotAnswer(relay.relayURL))
            beginManually()
            return
        }
        guard mine == attempt else { return }
        let session: any RelayPairingProtocol
        do {
            session = try await relay.offer(relay.relayURL, relay.bearer, accountEmail.trimmed)
        } catch {
            guard mine == attempt else { return }
            // Every failure to open a rendezvous — a relay that went away after
            // the probe, one that refuses new pairings for now, a URL the seam
            // will not send a bearer to — leaves the manual flow untouched.
            fallBackToManual(because: PairingModel.relayRefused(error))
            beginManually()
            return
        }
        guard mine == attempt else {
            await session.cancel()
            return
        }
        relaySession = session
        guard let payload = session.qrPayload() else {
            fail(with: PairingUIError.noPayload)
            syncSeamState()
            return
        }
        phase = .handOff(relayCodeHandOff(payload))
        syncSeamState()
        await runHandshake(session, attempt: mine)
    }

    /// Join the rendezvous a scanned (or pasted) code names, on the device
    /// that holds the vault.
    func acceptOverRelay(_ text: String, relay: RelayTransport) async {
        attempt += 1
        let mine = attempt
        pasted = ""
        relayStep = 1
        phase = .working(PairingModel.reachingRelay)
        guard await relay.probe(relay.relayURL) else {
            guard mine == attempt else { return }
            // Not "carry on manually with this code": the device that showed
            // it is waiting on the relay, not on a pasted Noise message. Both
            // have to start again by copy and paste, and the screen says so.
            fallBackToManual(because: PairingModel.relayDidNotAnswerHere(relay.relayURL))
            phase = .awaiting(Self.prompt(for: .code))
            return
        }
        guard mine == attempt else { return }
        let session: any RelayPairingProtocol
        do {
            session = try relay.accept(text, relay.relayURL, relay.bearer)
        } catch {
            fail(with: error)
            syncSeamState()
            return
        }
        relaySession = session
        phase = .working(PairingModel.connecting)
        syncSeamState()
        await runHandshake(session, attempt: mine)
    }

    /// The user's answer to the digits, over the relay.
    ///
    /// "Match" runs the whole of the last three messages: the seam sends the
    /// offer, the request and the grant itself, so there is nothing in between
    /// for this screen to show but that it is working.
    func confirmOverRelay(matched: Bool, session: any RelayPairingProtocol) async {
        let mine = attempt
        guard matched else {
            relaySession = nil
            phase = .mismatch
            syncSeamState()
            await session.reject()
            return
        }
        relayStep = 3
        phase = .working(finishingLabel)
        do {
            if intent == .addAnotherDevice {
                guard let sponsorRelay else { throw PairingUIError.noOpenVault }
                try await sponsorRelay(session)
            } else {
                let bundle = try await session.join(
                    nickname: Platform.deviceName,
                    platform: Platform.identifier,
                    seedS: SystemRandom.bytes(32),
                    seedD: SystemRandom.bytes(32)
                )
                guard mine == attempt else { return }
                await adopt?(bundle.vaultRoot, bundle.payloadBytes)
            }
        } catch {
            guard mine == attempt else { return }
            relaySession = nil
            await session.cancel()
            fail(with: error)
            syncSeamState()
            return
        }
        guard mine == attempt else { return }
        relaySession = nil
        phase = .done(doneSummary)
        syncSeamState()
    }

    /// Leave the relay for the copy-and-paste script, by the user's choice.
    ///
    /// The way out when one device reaches the relay and the other does not:
    /// both take this, and both are then walking the same eight legs.
    func useManualInstead() {
        cancel()
        transport = .manual
        notice = nil
    }

    // MARK: - Internals

    private func runHandshake(_ session: any RelayPairingProtocol, attempt mine: Int) async {
        do {
            let sas = try await session.handshake()
            guard mine == attempt else { return }
            relayStep = 2
            phase = .comparing(sas: sas)
        } catch {
            guard mine == attempt else { return }
            relaySession = nil
            fail(with: error)
        }
        syncSeamState()
    }

    private func fallBackToManual(because reason: String) {
        transport = .manual
        notice = reason
        relaySession = nil
    }

    /// Where the relay's three steps are, for the progress line.
    ///
    /// Read by ``progress`` whenever ``transport`` is the relay.
    var relayProgress: (leg: Int, of: Int)? {
        switch phase {
        case .handOff, .awaiting, .working, .comparing: (relayStep, Self.relaySteps)
        case .idle, .done, .mismatch, .failed: nil
        }
    }
}

extension PairingModel.Phase {
    /// Done, mismatched or failed: the pairing has an outcome and nothing is
    /// running. A notice about how it was running no longer applies.
    var isOutcome: Bool {
        switch self {
        case .done, .mismatch, .failed: true
        case .idle, .handOff, .awaiting, .comparing, .working: false
        }
    }
}
