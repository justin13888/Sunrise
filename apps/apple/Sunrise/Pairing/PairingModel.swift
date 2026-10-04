import Foundation

/// One device's half of a pairing, as a screen the user can walk.
///
/// # Two transports, one model
///
/// The three Noise messages and the three pairing messages cross either
/// through the relay's rendezvous or by the user's hand, and the crypto does
/// not care which — the SAS binds the transcript either way.
///
/// - **Over the relay** (``Transport/relay``, the default whenever this device
///   has a relay and a signed-in account): three phases. The device being
///   added shows a QR, the other scans it, both compare six digits, done.
///   `RelayPairing` moves every message; this model never sees one. That half
///   is in `PairingRelay.swift`.
/// - **By copy and paste** (``Transport/manual``): an eight-leg script over
///   `DevicePairing`. At each leg one device is showing text and the other is
///   pasting it, and the model's job is to make it obvious which of those this
///   device is doing right now. It is the fallback for a device with no relay,
///   no account, or a relay that does not answer, and the screen says which.
///
/// The code leg is the same in both: the existing device scans it with the
/// camera or pastes the text under it, and the scanned string goes to the seam
/// exactly as read. Swift never parses a pairing QR.
///
/// # Why eight legs and not six
///
/// Pairing used to end with one message: the device holding the vault sealed
/// everything — including `ID_S_priv`, the account's signing key — and the new
/// device opened it. Handing that key to every device is what let a *revoked*
/// one mint a fresh device id, sign itself a certificate that genuinely
/// verified, and rejoin (#105).
///
/// It does not travel now, and the cost is a round trip: the device that holds
/// the vault cannot sign a certificate for keys the joining device has not
/// minted yet. So the last leg became three — the account's public identity
/// out, the new device's freshly minted public keys back, then the signed
/// certificate and the vault key together. The user does one more copy and
/// paste; what they get is a device that cannot be impersonated by one they
/// threw away.
///
/// A device added this way can never add another. That is the property rather
/// than a gap, and `SunriseCore.canSponsorPairing()` is how a screen asks
/// before offering ``Intent/addAnotherDevice``.
///
/// # What is not here
///
/// The spec's 90-second timeout at the SAS screen. It bounds a rendezvous
/// where both devices are live on one WebSocket; here the user is walking
/// between two machines with a code in their head, and a timer that fired
/// mid-walk would abort pairings that were going fine. The abort it protects
/// against is on both screens as a button instead.
@MainActor
@Observable
final class PairingModel {
    /// Which side of the pairing this device is asking to be.
    ///
    /// Named for what the user wants rather than for the protocol role,
    /// because the two are inverted from the obvious reading: the device
    /// *being added* publishes the QR and initiates (`PairingRole.newDevice`),
    /// and the device that already holds the vault reads it and issues the
    /// certificate.
    enum Intent: Equatable {
        /// This device has no vault and wants one. It is `PairingRole.newDevice`.
        case addThisMac
        /// This device holds the vault and is authorising another. It is
        /// `PairingRole.existingDevice`.
        case addAnotherDevice
    }

    /// The legs, in the order the protocol forces: three Noise handshake
    /// messages alternating from the new device, the SAS, then offer, request
    /// and grant.
    enum Leg: Int, CaseIterable {
        case code
        case first
        case second
        case third
        case compare
        case offer
        case request
        case grant
    }

    enum Phase: Equatable {
        /// Collecting what `begin` needs; nothing has been generated.
        case idle
        case handOff(HandOff)
        case awaiting(Prompt)
        /// Both devices are showing `sas`. Nothing advances from here without
        /// an explicit answer — see `confirm(matched:)`.
        case comparing(sas: String)
        /// The seam is doing something slow, which the string names: reaching
        /// the relay, waiting for the other device, sealing a message, or
        /// writing the vault and opening it.
        case working(String)
        case done(String)
        /// The user said the digits differed. Kept apart from `failed` because
        /// it is not a thing that went wrong — it is the check working, and
        /// the only honest reading is that something was between the two
        /// devices.
        case mismatch
        case failed(String)
    }

    let intent: Intent
    /// Internal so `PairingRelay.swift` can write it; views only read it.
    var phase: Phase = .idle

    /// The seam's own answers, snapshotted.
    ///
    /// Read from `DevicePairing` after every mutation rather than computed on
    /// demand: `step()` changes inside the Rust object without anything on
    /// this class being assigned, so a computed property would never repaint.
    private(set) var role: PairingRole?
    private(set) var handshakeStep: PairingStep?

    /// What the user is typing into the paste field. Cleared on every accepted
    /// leg, so the previous blob cannot be submitted twice.
    var pasted = ""

    /// The account this pairing is for. Hashed to four bytes before it reaches
    /// the QR — the payload names an account without naming a person.
    var accountEmail = ""

    /// Fixed at construction from what the caller could supply, and changed
    /// only toward ``Transport/manual``: by a relay that does not answer, or
    /// by the user choosing it (``useManualInstead()``).
    var transport: Transport

    /// Why this pairing is not running over the relay, when that was not the
    /// user's choice. Shown on the screen, because a fallback nobody explains
    /// reads as the app being slower than it should be.
    var notice: String?

    private var pairing: DevicePairing?
    private let relayURL: String

    /// How this device reaches the relay, or `nil` when it cannot: no relay
    /// configured, or no account signed in to present a bearer for.
    let relay: RelayTransport?

    /// Runs `RelayPairing.sponsor` against the open vault. The vault's
    /// `SunriseCore` stays behind `CoreBridge`; the cert is signed inside it.
    /// Absent on a device taking `.addThisMac`, as `sealOffer` is.
    let sponsorRelay: ((any RelayPairingProtocol) async throws -> Void)?

    /// The rendezvous in progress, once one is open.
    var relaySession: (any RelayPairingProtocol)?
    /// Which of the relay's three steps is on screen; `.working` spans two.
    var relayStep = 1
    /// Bumped by every start and every cancel, so a relay wait that returns
    /// after the user moved on cannot write over the screen they moved to.
    var attempt = 0

    /// Seals the open vault's account identity for a confirmed pairing. Absent
    /// on a device with no open vault, which is every device taking
    /// `.addThisMac`.
    private let sealOffer: ((DevicePairing) async throws -> String)?

    /// Issues the joining device's certificate and seals it with the vault.
    ///
    /// Takes the sealed request, because the certificate is signed *over the
    /// keys inside it* — which is the whole reason pairing needs a round trip.
    /// Absent for the same reason `sealOffer` is.
    private let sealGrant: ((DevicePairing, String) async throws -> String)?

    /// Hands the opened root **and** the bundle behind it to the session.
    /// Absent in `.addAnotherDevice`, where this device keeps the vault it
    /// already has.
    ///
    /// Two values rather than one because the root alone no longer opens an
    /// account: since ADR-0024 the Stream keys are random, and they travel in
    /// the bundle — along, now, with this device's own keys and the certificate
    /// the other one signed for them.
    let adopt: ((Data, Data) async -> Void)?

    /// Text a submit produced for the *next* leg to show.
    ///
    /// The three-message exchange is the only part of the script where what one
    /// device shows is computed from what it just pasted: the request is built
    /// from the offer, and the grant is signed over the request. `step(after:)`
    /// has access to neither, so the submit that produced the text parks it
    /// here and the step picks it up. Cleared as it is read, so a stale one can
    /// never be shown twice.
    private var pendingHandOff: String?

    /// `relay` is what the relay needs — its URL and this account's bearer —
    /// or `nil` to pair by copy and paste. `signedIn` only words the notice
    /// when `relay` is `nil`: a device with a relay and no account is told to
    /// sign in, one with no relay is told it has none.
    init(
        intent: Intent,
        relayURL: String = "",
        relay: RelayTransport? = nil,
        signedIn: Bool = false,
        sealOffer: ((DevicePairing) async throws -> String)? = nil,
        sealGrant: ((DevicePairing, String) async throws -> String)? = nil,
        sponsorRelay: ((any RelayPairingProtocol) async throws -> Void)? = nil,
        adopt: ((Data, Data) async -> Void)? = nil
    ) {
        self.intent = intent
        self.relayURL = relayURL
        self.relay = relay
        self.sealOffer = sealOffer
        self.sealGrant = sealGrant
        self.sponsorRelay = sponsorRelay
        self.adopt = adopt
        if relay != nil {
            transport = .relay
        } else {
            transport = .manual
            notice = relayURL.trimmed.isEmpty
                ? Self.noRelayConfigured
                : (signedIn ? nil : Self.notSignedIn)
        }
        if intent == .addAnotherDevice {
            phase = .awaiting(Self.prompt(for: .code))
        }
    }

    /// The account tag the QR carries, for the user to compare against what
    /// the other device shows. Four bytes of BLAKE3 over the normalized
    /// address — enough to catch pairing the wrong account, not enough to
    /// recover the address.
    var accountTag: String {
        accountEmail.trimmed.isEmpty ? "" : pairingAccountTag(accountEmail: accountEmail.trimmed)
    }

    /// Where in the script this device is, for a progress line.
    var progress: (leg: Int, of: Int)? {
        if transport == .relay { return relayProgress }
        guard let leg = currentLeg else { return nil }
        return (leg.rawValue + 1, Leg.allCases.count)
    }

    private var currentLeg: Leg? {
        switch phase {
        case let .handOff(handOff): handOff.leg
        case let .awaiting(prompt): prompt.leg
        case .comparing: .compare
        case .working: .offer
        case .idle, .done, .mismatch, .failed: nil
        }
    }

    // MARK: - Driving it

    /// Start as the device being added: mint the throwaway keypair and publish
    /// the QR. Valid only for `.addThisMac`.
    ///
    /// Over the relay this returns only once the other device has scanned the
    /// code and the digits are on screen, or the pairing has stopped; the
    /// screen calls it from a task and shows each phase as it is entered.
    func begin() async {
        guard intent == .addThisMac, pairing == nil, relaySession == nil else { return }
        if transport == .relay, let relay {
            await beginOverRelay(relay)
        } else {
            beginManually()
        }
    }

    /// The copy-and-paste start: mint the code and show it, with nothing
    /// opened anywhere else.
    func beginManually() {
        do {
            let session = try DevicePairing.offer(
                relayUrl: relayURL,
                accountEmail: accountEmail.trimmed
            )
            pairing = session
            guard let payload = session.qrPayload() else { throw PairingUIError.noPayload }
            phase = .handOff(handOff(for: .code, text: payload))
        } catch {
            fail(with: error)
        }
        syncSeamState()
    }

    /// The user has handed the current text over; move to the next leg.
    func advance() {
        guard case let .handOff(handOff) = phase else { return }
        step(after: handOff.leg)
        syncSeamState()
    }

    /// Take what the other device is showing.
    ///
    /// The seam validates every one of these — the QR by its own decoder, the
    /// Noise messages by decrypting them, the grant by checking the certificate
    /// inside it against the offer this device has been holding — so a
    /// mistyped, truncated or simply wrong paste is refused here rather than
    /// becoming a vault whose every op its peers reject.
    func submit() async {
        guard case let .awaiting(prompt) = phase else { return }
        let text = pasted.trimmed
        guard !text.isEmpty else { return }
        if prompt.leg == .code, transport == .relay, let relay {
            await acceptOverRelay(text, relay: relay)
        } else {
            await submitManually(text, at: prompt.leg)
        }
    }

    /// One leg of the copy-and-paste script, from what was pasted into it.
    private func submitManually(_ text: String, at leg: Leg) async {
        do {
            switch leg {
            case .code:
                pairing = try DevicePairing.accept(qrPayload: text)
            case .first, .second, .third:
                try require().receiveMessage(message: text)
            case .offer:
                // The one leg that mints. `D_S_priv` is derived from the first
                // seed and every op this device ever writes is signed under it,
                // so both come from the system CSPRNG — and neither leaves the
                // Rust object: only the public halves go into the request.
                pendingHandOff = try require().requestDeviceCert(
                    sealedOffer: text,
                    nickname: Platform.deviceName,
                    platform: Platform.identifier,
                    seedS: SystemRandom.bytes(32),
                    seedD: SystemRandom.bytes(32)
                )
            case .request:
                pasted = ""
                phase = .working(finishingLabel)
                syncSeamState()
                guard let sealGrant else { throw PairingUIError.noOpenVault }
                pendingHandOff = try await sealGrant(require(), text)
            case .grant:
                let bundle = try require().openPairingGrant(sealed: text)
                pasted = ""
                phase = .working(finishingLabel)
                syncSeamState()
                await adopt?(bundle.vaultRoot, bundle.payloadBytes)
                pairing = nil
                phase = .done(doneSummary)
                syncSeamState()
                return
            case .compare:
                return
            }
        } catch {
            fail(with: error)
            syncSeamState()
            return
        }
        pasted = ""
        step(after: leg)
        syncSeamState()
    }

    /// Record this user's answer to the SAS screen.
    ///
    /// Both devices must answer `true` independently; this one only ever knows
    /// about its own user. `false` discards the ephemeral keys here and now —
    /// the local half of the spec's `pair_abort` — and cannot be walked back.
    ///
    /// On the device that holds the vault, a `true` also seals the offer, which
    /// is why this is the one confirmation that awaits: the seam refuses to
    /// send anything before the SAS was answered, so the two belong in one
    /// call.
    func confirm(matched: Bool) async {
        guard case .comparing = phase else { return }
        if let relaySession {
            await confirmOverRelay(matched: matched, session: relaySession)
            return
        }
        guard let session = pairing else { return }
        guard matched else {
            try? session.confirm(matched: false)
            pairing = nil
            phase = .mismatch
            syncSeamState()
            return
        }
        do {
            try session.confirm(matched: true)
        } catch {
            fail(with: error)
            syncSeamState()
            return
        }
        step(after: .compare)
        syncSeamState()
        guard case .working = phase, intent == .addAnotherDevice else { return }
        await sealOfferForPeer(session)
    }

    /// End it, at any point, leaving nothing half-open.
    ///
    /// `confirm(matched: false)` is the discard: it takes the session out of
    /// the Rust object and drops the ephemeral keys — and, since the round
    /// trip, this device's minted `D_S`/`D_D` with them. Its error is the
    /// expected outcome, not a problem, which is why it is swallowed here and
    /// nowhere else.
    ///
    /// Over the relay, the seam's own `cancel` ends any wait in progress and
    /// tells the other device, which then stops at its next poll rather than
    /// at the session's five-minute expiry.
    func cancel() {
        attempt += 1
        if let relaySession {
            Task { await relaySession.cancel() }
        }
        relaySession = nil
        relayStep = 1
        try? pairing?.confirm(matched: false)
        pairing = nil
        pasted = ""
        pendingHandOff = nil
        phase = intent == .addAnotherDevice ? .awaiting(Self.prompt(for: .code)) : .idle
        syncSeamState()
    }
}

/// The script, and the words for each leg.
///
/// An extension rather than more of the class above: what the model *is* —
/// the phase, the seam handle, the answers a user can give — is a different
/// thing from the order the legs run in and what each one says on screen.
extension PairingModel {
    // MARK: - The script

    /// What this device does at the leg after `leg`, given its role.
    ///
    /// The table this encodes:
    ///
    ///     leg      | being added | already has the vault
    ///     ---------|-------------|----------------------
    ///     code     | show        | paste
    ///     first    | show        | paste
    ///     second   | paste       | show
    ///     third    | show        | paste
    ///     compare  | both        | both
    ///     offer    | paste       | show
    ///     request  | show        | paste
    ///     grant    | paste       | show
    ///
    /// The last three alternate where the old script simply ended, and the
    /// alternation is the point: a vault cannot certify keys it has not been
    /// shown.
    private func step(after leg: Leg) {
        guard let next = Leg(rawValue: leg.rawValue + 1) else {
            phase = .done(doneSummary)
            return
        }
        do {
            switch next {
            case .code:
                return
            case .first, .second, .third:
                phase = shows(next)
                    ? .handOff(handOff(for: next, text: try require().nextMessage()))
                    : .awaiting(Self.prompt(for: next))
            case .compare:
                phase = .comparing(sas: try require().sas())
            case .offer:
                // The device that seals passes through `.working` while it
                // does; the one being added waits to be handed the ciphertext.
                phase = shows(.offer) ? .working(finishingLabel) : .awaiting(Self.prompt(for: .offer))
            case .request, .grant:
                // Both were computed by the submit that just ran — a request is
                // built from the offer, a grant is signed over the request — so
                // the text is waiting rather than produced here.
                if shows(next) {
                    guard let text = pendingHandOff else { throw PairingUIError.noSession }
                    pendingHandOff = nil
                    phase = .handOff(handOff(for: next, text: text))
                } else {
                    phase = .awaiting(Self.prompt(for: next))
                }
            }
        } catch {
            fail(with: error)
        }
    }

    /// Whether this device is the one showing text at `leg`.
    private func shows(_ leg: Leg) -> Bool {
        switch leg {
        case .code, .first, .third, .request: intent == .addThisMac
        case .second, .offer, .grant: intent == .addAnotherDevice
        case .compare: false
        }
    }

    private func sealOfferForPeer(_ session: DevicePairing) async {
        guard let sealOffer else {
            fail(with: PairingUIError.noOpenVault)
            syncSeamState()
            return
        }
        do {
            phase = .handOff(handOff(for: .offer, text: try await sealOffer(session)))
        } catch {
            fail(with: error)
        }
        syncSeamState()
    }

    private func require() throws -> DevicePairing {
        guard let pairing else { throw PairingUIError.noSession }
        return pairing
    }

    func fail(with error: any Error) {
        phase = .failed(error.localizedDescription)
    }

    func syncSeamState() {
        role = pairing?.role() ?? relaySession?.role()
        // `RelayPairing` keeps its `DevicePairing` inside, so over the relay
        // there is no step to show — and nothing to disagree with, since the
        // seam runs the order itself.
        handshakeStep = pairing?.step()
    }
}
