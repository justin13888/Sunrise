import Foundation

/// The recovery-code ceremony: publish this vault, show the twenty-four words
/// once, and make the user prove they wrote them down.
///
/// Until this existed, **a vault created by the Apple apps had no recovery blob
/// at all** (#181). That is not a missing feature; it is a data-loss defect
/// with a delay on it. `Keychain::create` keeps `ID_D_priv` on the device that
/// created the account and nowhere else, and the recovery blob is the only
/// other place that key is allowed to live — so a Mac or iPhone that founded an
/// account and was then lost took the key with it, and every `Recipient::
/// Identity` copy in the op log became permanently unopenable.
/// `docs/03-crypto/key-rotation.md` §Revocation has said so for some time.
///
/// # Why this is a ceremony and not a screen
///
/// `docs/03-crypto/pairing-and-onboarding.md` §Onboarding UX requirements: *the
/// recovery code is shown exactly once and the user passes a paste-back
/// confirmation.* Both halves are load-bearing, and the second is the one an
/// app can get wrong cheaply. A code shown once and not written down is worse
/// than no code, because it is believed. So the ceremony ends on
/// ``Phase/done`` only after the user has typed the words back, and a mismatch
/// goes to ``Phase/mismatch``, whose only way forward is to see the code again
/// — `recovery.md` §Test-recovery affordance is explicit that a user who fails
/// this gate is not marooned on a code they wrote down wrong.
///
/// # What the CLI does, and where this differs
///
/// `sunrise bootstrap` prints the code to stdout, says what losing it costs,
/// and exits. It cannot do more: there is no one to ask. This performs the same
/// ceremony — draw, seal, upload, then show, once, never to a file and never to
/// a log — and adds the confirmation a UI is able to ask for. The words are
/// held in this object for the length of the ceremony and dropped at
/// ``finish()``; nothing writes them anywhere.
@MainActor
@Observable
final class RecoveryCodeModel {
    enum Phase: Equatable {
        /// Nothing has happened yet.
        case idle
        /// Sealing and uploading.
        case working
        /// The words are on screen. This is the only time they ever are.
        case show
        /// Asking for them back.
        case verify
        /// What was typed is not what was shown.
        case mismatch
        /// Confirmed. The words are gone from memory.
        case done
        /// This device holds no `ID_D_priv`, so it cannot seal a blob.
        ///
        /// Not an error, and it must not be shown as one: a device admitted by
        /// pairing holds only `ID_D_pub`, and the device that created the
        /// account is the one that can produce a code — which it already did.
        /// A paired device that sealed one anyway would hand its user a code
        /// decrypting to a key that opens nothing.
        case notThisDevice
        /// The seal or the upload failed.
        case failed(String)
    }

    private(set) var phase: Phase = .idle

    /// The twenty-four words, while the ceremony is running.
    ///
    /// Empty before ``start()`` and after ``finish()``. Deliberately not
    /// `@ObservationIgnored`: the view has to redraw when they arrive.
    private(set) var words: [String] = []

    /// What the user typed back, so ``mismatch`` can say how close it was
    /// without saying what was right.
    private(set) var attempted = false

    /// Publish the vault and return the code, or `nil` if this device cannot
    /// seal one.
    ///
    /// A closure rather than a `CoreBridge`, so the ceremony is testable with
    /// no vault, no relay and no network — the rule `PairingModel` already
    /// follows for the same reason.
    private let publish: () async throws -> String?

    init(publish: @escaping () async throws -> String?) {
        self.publish = publish
    }

    /// Seal, upload, and show. Idempotent against a double tap.
    func start() async {
        guard phase == .idle || isRetryable(phase) else { return }
        phase = .working
        do {
            guard let code = try await publish() else {
                phase = .notThisDevice
                return
            }
            let parts = Self.normalize(code)
            guard !parts.isEmpty else {
                phase = .failed("The recovery code came back empty.")
                return
            }
            words = parts
            phase = .show
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// Move from reading the code to typing it back.
    func beginVerification() {
        guard phase == .show || phase == .mismatch else { return }
        phase = .verify
    }

    /// Show the code again, which is the only way out of a mismatch.
    func showAgain() {
        guard phase == .verify || phase == .mismatch else { return }
        phase = .show
    }

    /// Check what the user typed against what was shown.
    ///
    /// Compared word by word after normalising case and whitespace, because a
    /// user retyping twenty-four words off paper will not reproduce the
    /// spacing and has no reason to reproduce the case. The BIP-39 wordlist is
    /// lower-case ASCII, so this loses nothing.
    func confirm(_ typed: String) {
        guard phase == .verify else { return }
        attempted = true
        guard Self.normalize(typed) == words else {
            phase = .mismatch
            return
        }
        finish()
    }

    /// Drop the words and close the ceremony.
    private func finish() {
        words = []
        phase = .done
    }

    /// Whether this phase is one the user can try again from.
    private func isRetryable(_ phase: Phase) -> Bool {
        if case .failed = phase { return true }
        return false
    }

    /// Words, lower-cased, with runs of whitespace collapsed.
    static func normalize(_ code: String) -> [String] {
        code.lowercased()
            .split(whereSeparator: \.isWhitespace)
            .map(String.init)
    }

    /// The code as it is read off a screen: rows of four.
    ///
    /// The same shape `sunrise-cli`'s `print_recovery_code` uses, and for the
    /// reason it gives: twenty-four words on one wrapped line is what a
    /// transcription error looks like before it happens.
    var rows: [[String]] {
        stride(from: 0, to: words.count, by: 4).map { start in
            Array(words[start ..< min(start + 4, words.count)])
        }
    }

    /// What the Recovery setup screen states, verbatim from
    /// `docs/03-crypto/recovery.md` §What the user is told.
    ///
    /// Verbatim because the document says "states verbatim". This sentence is
    /// the whole product promise and its whole cost in one paragraph, and an
    /// app that paraphrased it would be making a different promise.
    static let warning = """
        Sunrise is end-to-end encrypted. We cannot reset your account if you \
        lose this code. This is by design — it means we can't read your data, \
        but it also means we can't recover it for you. Save this code \
        somewhere you'll find it years from now.
        """
}

extension SessionModel {
    /// Why a recovery blob could not be sealed and uploaded yet.
    ///
    /// Each case is something the user can fix, and each says what: none of
    /// them means the vault is broken, and all of them mean the account key
    /// still has no second copy.
    enum RecoverySetupError: LocalizedError, Equatable {
        case vaultClosed
        case notConfigured
        case signedOut

        var errorDescription: String? {
            switch self {
            case .vaultClosed:
                "The vault closed before recovery could be set up."
            case .notConfigured:
                """
                Sunrise needs a relay address and your account email before it \
                can store your recovery blob. Add them in Settings, then set \
                recovery up again.
                """
            case .signedOut:
                """
                Sign in first. Your recovery blob is stored on the relay under \
                your account, and the relay will not take it from a device it \
                cannot identify.
                """
            }
        }
    }
}

/// The other end of the ceremony: spend a recovery code on a device that has
/// nothing, and come back with the account (#349).
///
/// `docs/03-crypto/recovery.md` §Recovery flow, as a user walks it. They type
/// the twenty-four words, and each word is checked against the BIP-39 list as
/// it is typed, by the same Rust the restore spends it with. A typo is marked
/// before any network call. The browser sign-in comes next, because the relay
/// releases the blob only to a fresh one. The restore follows, and it reports
/// progress until the history has replayed. The aftercare comes last, as
/// §Recovery flow step 7 asks.
///
/// Closures rather than a `CoreBridge` and a `SunriseLogin`, so the whole
/// sequence is testable against a stub relay with no network, as
/// ``RecoveryCodeModel`` and `PairingModel` are.
@MainActor
@Observable
final class RestoreFromCodeModel {
    enum Phase: Equatable {
        /// Typing the words.
        case entering
        /// The browser sign-in the relay requires before it releases the blob.
        case signingIn
        /// The restore is running.
        case restoring(Progress)
        /// The account is back and its history is read. The aftercare is on
        /// screen.
        case restored
        /// The vault is restored and opened, and its history had not finished
        /// replaying. It resumes when the vault syncs.
        case incomplete(String)
        /// Nothing was written. ``Failure`` says what to do next.
        case failed(Failure)
    }

    enum Progress: Equatable {
        case fetching
        case identityOpened
        case deviceRegistered
        case replaying(applied: UInt64)
    }

    enum Failure: Equatable {
        /// Mistyped, or another account's code. Back to the words.
        case wrongCode(String)
        /// The relay wants a fresh sign-in, or the sign-in did not complete.
        case signInRequired(String)
        /// Anything else: no relay, no blob, a relay that did not answer.
        case other(String)

        var message: String {
            switch self {
            case let .wrongCode(text), let .signInRequired(text), let .other(text): text
            }
        }
    }

    /// The restore itself: the code, a bearer carrying a fresh sign-in, and
    /// where to report each step. Throws the seam's `BindingError`.
    typealias Restore = (
        _ code: String,
        _ bearer: String,
        _ onStep: @escaping @Sendable (RecoveryStep) -> Void
    ) async throws -> Void

    private(set) var phase: Phase = .entering

    /// What is in the 24-word field. Each edit re-checks it.
    var text = "" {
        didSet { revalidate() }
    }

    /// How many words the field holds.
    private(set) var wordCount = 0
    /// The 1-based positions of words that are not on the list. Positions,
    /// never the words: what is on screen next to a recovery code ends up in
    /// screenshots.
    private(set) var unknownWords: [Int] = []
    /// Set when twenty-four listed words still fail the checksum.
    private(set) var checksumProblem: String?
    /// How many Stream keys the aftercare rotated, once it has.
    private(set) var rotatedStreams: Int?
    /// Why the aftercare's rotation failed, if it did.
    private(set) var rotationProblem: String?

    private let isWord: (String) -> Bool
    private let checkCode: (String) throws -> Void
    private let signIn: () async throws -> String
    private let restore: Restore
    private let rotateStreamKeys: () async throws -> Int

    init(
        signIn: @escaping () async throws -> String,
        restore: @escaping Restore,
        rotateStreamKeys: @escaping () async throws -> Int = { 0 },
        isWord: @escaping (String) -> Bool = { isRecoveryWord(word: $0) },
        checkCode: @escaping (String) throws -> Void = { try checkRecoveryCode(code: $0) }
    ) {
        self.signIn = signIn
        self.restore = restore
        self.rotateStreamKeys = rotateStreamKeys
        self.isWord = isWord
        self.checkCode = checkCode
    }

    /// Whether the words are worth a sign-in: twenty-four of them, all
    /// listed, with a checksum that holds.
    var canRestore: Bool {
        guard wordCount == 24, unknownWords.isEmpty, checksumProblem == nil else { return false }
        switch phase {
        case .entering, .failed: return true
        default: return false
        }
    }

    /// Sign in, restore, and report. Idempotent against a double tap.
    func restoreAccount() async {
        guard canRestore else { return }
        let code = RecoveryCodeModel.normalize(text).joined(separator: " ")
        phase = .signingIn
        let bearer: String
        do {
            bearer = try await signIn()
        } catch {
            phase = .failed(.signInRequired(error.localizedDescription))
            return
        }
        phase = .restoring(.fetching)
        do {
            try await restore(code, bearer) { [weak self] step in
                _Concurrency.Task { @MainActor in self?.advance(step) }
            }
            text = ""
            phase = .restored
        } catch {
            phase = Self.outcome(of: error)
            // A restore that wrote the vault has spent the words; they are
            // not kept on screen for a retry that has nothing left to do.
            if case .incomplete = phase { text = "" }
        }
    }

    /// Rotate every Stream key: the second half of §Recovery flow step 7.
    func rotateKeys() async {
        rotationProblem = nil
        do {
            rotatedStreams = try await rotateStreamKeys()
        } catch {
            rotationProblem = error.localizedDescription
        }
    }

    /// What a failed restore means for the next step, read off the seam's
    /// error variant rather than its wording.
    static func outcome(of error: any Error) -> Phase {
        guard let binding = error as? BindingError else {
            return .failed(.other(error.localizedDescription))
        }
        switch binding {
        case let .RecoveryCode(message): return .failed(.wrongCode(message))
        case let .StepUpRequired(message): return .failed(.signInRequired(message))
        case let .RecoveryIncomplete(message): return .incomplete(message)
        default: return .failed(.other(binding.localizedDescription))
        }
    }

    /// Fold one step into the progress, but only while the restore is still
    /// running: a step delivered after it returned must not pull the screen
    /// back from its outcome.
    private func advance(_ step: RecoveryStep) {
        guard case .restoring = phase else { return }
        switch step {
        case .blobFetched: phase = .restoring(.fetching)
        case .identityOpened: phase = .restoring(.identityOpened)
        case .deviceRegistered: phase = .restoring(.deviceRegistered)
        case let .replaying(applied): phase = .restoring(.replaying(applied: applied))
        case .caughtUp: break
        }
    }

    private func revalidate() {
        let words = RecoveryCodeModel.normalize(text)
        wordCount = words.count
        unknownWords = words.indices.filter { !isWord(words[$0]) }.map { $0 + 1 }
        checksumProblem = nil
        if wordCount == 24, unknownWords.isEmpty {
            do {
                try checkCode(words.joined(separator: " "))
            } catch {
                checksumProblem = error.localizedDescription
            }
        }
        // Editing the words is the answer to a wrong code.
        if case .failed(.wrongCode) = phase { phase = .entering }
    }
}

/// What a restore sends besides the vault: the relay, a bearer carrying a
/// fresh sign-in, the words, and what the device list will call this device.
struct RecoveryRequest: Sendable {
    let relayURL: String
    let bearer: String
    let code: String
    let nickname: String
}

/// Hands the seam's recovery steps to a Swift closure. Called on a Rust
/// worker thread, never the main one.
final class RecoveryStepForwarder: RecoveryListener {
    private let forward: @Sendable (RecoveryStep) -> Void

    init(onStep: @escaping @Sendable (RecoveryStep) -> Void) {
        forward = onStep
    }

    func onStep(step: RecoveryStep) {
        forward(step)
    }
}
