import Foundation

/// One device's half of a pairing, as a screen the user can walk.
///
/// # Why the user carries the bytes
///
/// `crates/sunrise-core-bindings/src/pairing.rs` says it plainly: the relay's
/// pairing rendezvous does not exist yet, so the transport for the three Noise
/// messages and the sealed root **is the user**. Every one of them crosses as
/// base64url text, and the crypto does not care how it travelled — the SAS
/// binds the transcript either way. So this model is a six-leg script: at each
/// leg one device is showing text and the other is pasting it, and the model's
/// job is to make it obvious which of those this Mac is doing right now.
///
/// When the rendezvous lands, the same seam calls drive it in the same order.
/// Only who moves the bytes changes.
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
    /// Which side of the pairing this Mac is asking to be.
    ///
    /// Named for what the user wants rather than for the protocol role,
    /// because the two are inverted from the obvious reading: the device
    /// *being added* publishes the QR and initiates (`PairingRole.newDevice`),
    /// and the device that already holds the vault reads it and sends the root.
    enum Intent: Equatable {
        /// This Mac has no vault and wants one. It is `PairingRole.newDevice`.
        case addThisMac
        /// This Mac holds the vault and is authorising another device. It is
        /// `PairingRole.existingDevice`.
        case addAnotherDevice
    }

    /// The legs, in the order Noise XX forces: three handshake messages
    /// alternating from the new device, then the SAS, then the root.
    enum Leg: Int, CaseIterable {
        case code
        case first
        case second
        case third
        case compare
        case root
    }

    /// Text this Mac has produced that the other one needs.
    struct HandOff: Equatable {
        let leg: Leg
        let title: String
        let instruction: String
        let text: String
        /// Only the QR payload is drawn as a code. The Noise messages are
        /// bigger than a screen-readable symbol and are copied, not scanned.
        let drawsCode: Bool
    }

    /// Text the other Mac is showing that this one needs.
    struct Prompt: Equatable {
        let leg: Leg
        let title: String
        let instruction: String
    }

    enum Phase: Equatable {
        /// Collecting what `begin` needs; nothing has been generated.
        case idle
        case handOff(HandOff)
        case awaiting(Prompt)
        /// Both devices are showing `sas`. Nothing advances from here without
        /// an explicit answer — see `confirm(matched:)`.
        case comparing(sas: String)
        /// The seam is doing the one slow thing on this screen: sealing the
        /// root, or writing it and opening the vault behind it.
        case working
        case done(String)
        /// The user said the digits differed. Kept apart from `failed` because
        /// it is not a thing that went wrong — it is the check working, and
        /// the only honest reading is that something was between the two
        /// devices.
        case mismatch
        case failed(String)
    }

    let intent: Intent
    private(set) var phase: Phase = .idle

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

    private var pairing: DevicePairing?
    private let relayURL: String
    /// Seals the open vault's pairing payload for a confirmed pairing. Absent
    /// on a Mac with no open vault, which is every Mac taking `.addThisMac`.
    private let sealPayload: ((DevicePairing) async throws -> String)?
    /// Hands the opened root **and** the bundle behind it to the session.
    /// Absent in `.addAnotherDevice`, where this Mac keeps the vault it
    /// already has.
    ///
    /// Two values rather than one because the root alone no longer opens an
    /// account: since ADR-0024 the Stream keys are random, and they travel in
    /// the bundle.
    private let adopt: ((Data, Data) async -> Void)?

    init(
        intent: Intent,
        relayURL: String = "",
        sealPayload: ((DevicePairing) async throws -> String)? = nil,
        adopt: ((Data, Data) async -> Void)? = nil
    ) {
        self.intent = intent
        self.relayURL = relayURL
        self.sealPayload = sealPayload
        self.adopt = adopt
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

    /// Where in the six-leg script this Mac is, for a progress line.
    var progress: (leg: Int, of: Int)? {
        guard let leg = currentLeg else { return nil }
        return (leg.rawValue + 1, Leg.allCases.count)
    }

    private var currentLeg: Leg? {
        switch phase {
        case let .handOff(handOff): handOff.leg
        case let .awaiting(prompt): prompt.leg
        case .comparing: .compare
        case .working: .root
        case .idle, .done, .mismatch, .failed: nil
        }
    }

    // MARK: - Driving it

    /// Start as the device being added: mint the throwaway keypair and publish
    /// the QR. Valid only for `.addThisMac`.
    func begin() {
        guard intent == .addThisMac, pairing == nil else { return }
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
    /// Noise messages by decrypting them — so a mistyped or truncated paste is
    /// refused here rather than becoming a handshake that fails later for no
    /// visible reason.
    func submit() async {
        guard case let .awaiting(prompt) = phase else { return }
        let text = pasted.trimmed
        guard !text.isEmpty else { return }
        do {
            switch prompt.leg {
            case .code:
                pairing = try DevicePairing.accept(qrPayload: text)
            case .first, .second, .third:
                try require().receiveMessage(message: text)
            case .root:
                let bundle = try require().openPairingPayload(sealed: text)
                pasted = ""
                phase = .working
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
        step(after: prompt.leg)
        syncSeamState()
    }

    /// Record this user's answer to the SAS screen.
    ///
    /// Both devices must answer `true` independently; this one only ever knows
    /// about its own user. `false` discards the ephemeral keys here and now —
    /// the local half of the spec's `pair_abort` — and cannot be walked back.
    ///
    /// On the device that holds the vault, a `true` also seals the root, which
    /// is why this is the one confirmation that awaits: the seam refuses to
    /// seal before the SAS was answered, so the two belong in one call.
    func confirm(matched: Bool) async {
        guard case .comparing = phase, let session = pairing else { return }
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
        await sealForPeer(session)
    }

    /// End it, at any point, leaving nothing half-open.
    ///
    /// `confirm(matched: false)` is the discard: it takes the session out of
    /// the Rust object and drops the ephemeral keys with it. Its error is the
    /// expected outcome, not a problem — which is why it is swallowed here and
    /// nowhere else.
    func cancel() {
        try? pairing?.confirm(matched: false)
        pairing = nil
        pasted = ""
        phase = intent == .addAnotherDevice ? .awaiting(Self.prompt(for: .code)) : .idle
        syncSeamState()
    }
}

/// The six-leg script, and the words for each leg.
///
/// An extension rather than more of the class above: what the model *is* —
/// the phase, the seam handle, the answers a user can give — is a different
/// thing from the order the legs run in and what each one says on screen.
extension PairingModel {
    // MARK: - The script

    /// What this Mac does at the leg after `leg`, given its role.
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
    ///     root     | paste       | show
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
            case .root:
                // The device that seals passes through `.working` while it
                // does; the one being added waits to be handed the ciphertext.
                phase = shows(.root) ? .working : .awaiting(Self.prompt(for: .root))
            }
        } catch {
            fail(with: error)
        }
    }

    /// Whether this Mac is the one showing text at `leg`.
    private func shows(_ leg: Leg) -> Bool {
        switch leg {
        case .code, .first, .third: intent == .addThisMac
        case .second, .root: intent == .addAnotherDevice
        case .compare: false
        }
    }

    private func sealForPeer(_ session: DevicePairing) async {
        guard let sealPayload else {
            fail(with: PairingUIError.noOpenVault)
            return
        }
        do {
            phase = .handOff(handOff(for: .root, text: try await sealPayload(session)))
        } catch {
            fail(with: error)
        }
        syncSeamState()
    }

    private func require() throws -> DevicePairing {
        guard let pairing else { throw PairingUIError.noSession }
        return pairing
    }

    private func fail(with error: any Error) {
        phase = .failed(error.localizedDescription)
    }

    private func syncSeamState() {
        role = pairing?.role()
        handshakeStep = pairing?.step()
    }

    private var doneSummary: String {
        intent == .addThisMac
            ? "This Mac is paired. Your vault is open here."
            : "The other device has your vault key. It can open your vault now."
    }

    // MARK: - Words

    private func handOff(for leg: Leg, text: String) -> HandOff {
        switch leg {
        case .code:
            HandOff(
                leg: leg,
                title: "Show this to the Mac that has your vault",
                instruction: """
                    Scan the code, or copy the text below and paste it into \
                    Settings › Vaults › Add a device on the Mac that already \
                    has your vault.
                    """,
                text: text,
                drawsCode: true
            )
        case .first, .second, .third:
            HandOff(
                leg: leg,
                title: "Copy this to the other Mac",
                instruction: """
                    Paste it into the field the other Mac is showing, then come \
                    back here and continue.
                    """,
                text: text,
                drawsCode: false
            )
        case .compare:
            HandOff(leg: leg, title: "", instruction: "", text: text, drawsCode: false)
        case .root:
            HandOff(
                leg: leg,
                title: "Copy this last block to the other Mac",
                instruction: """
                    This is your vault key, sealed so that only the device whose \
                    digits you just confirmed can open it. Anything it passes \
                    through on the way — a message, a clipboard, a relay — sees \
                    nothing usable.
                    """,
                text: text,
                drawsCode: false
            )
        }
    }

    private static func prompt(for leg: Leg) -> Prompt {
        switch leg {
        case .code:
            Prompt(
                leg: leg,
                title: "Paste the code from the Mac you are adding",
                instruction: """
                    That Mac is showing a QR code with the same text underneath \
                    it. Paste the text here.
                    """
            )
        case .first, .second, .third:
            Prompt(
                leg: leg,
                title: "Paste what the other Mac is showing",
                instruction: "Copy the block from the other Mac's screen and paste it here."
            )
        case .compare:
            Prompt(leg: leg, title: "", instruction: "")
        case .root:
            Prompt(
                leg: leg,
                title: "Paste the sealed key",
                instruction: """
                    The other Mac is showing one last block, now that you have \
                    both confirmed the digits. It is the only thing in this \
                    whole exchange that carries your vault key.
                    """
            )
        }
    }
}

/// The failures that are this screen's, not the seam's.
enum PairingUIError: Error, Equatable, LocalizedError {
    case noSession
    case noPayload
    case noOpenVault

    var errorDescription: String? {
        switch self {
        case .noSession:
            "This pairing is over. Start it again from the beginning."
        case .noPayload:
            "Sunrise could not produce a pairing code for this device."
        case .noOpenVault:
            "There is no open vault on this device to share."
        }
    }
}
