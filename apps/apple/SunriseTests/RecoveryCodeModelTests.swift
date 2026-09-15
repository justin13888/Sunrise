import Foundation
import Testing

@testable import Sunrise

/// The ceremony, with no vault, no relay and no network.
///
/// What is under test is the part that decides whether a user ends up holding
/// a code they can actually use: that the words are shown once, that the
/// paste-back is real rather than decorative, that a mismatch leads back to the
/// code rather than into a dead end, and that the words are gone from memory
/// once it is over.
@MainActor
struct RecoveryCodeModelTests {
    /// Twenty-four distinct words, which is what the real encoder produces and
    /// what makes an off-by-one in the row grouping visible.
    private static let code = (0 ..< 24).map { "word\($0)" }.joined(separator: " ")

    @Test
    func theWordsAreShownOnceAndThenTheCeremonyAsksForThemBack() async {
        let model = RecoveryCodeModel(publish: { Self.code })
        await model.start()

        #expect(model.phase == .show)
        #expect(model.words.count == 24)
        #expect(model.rows.count == 6, "six lines of four, as the CLI prints them")
        #expect(model.rows.allSatisfy { $0.count == 4 })

        model.beginVerification()
        #expect(model.phase == .verify)
    }

    /// The gate that makes the whole thing worth doing. A code shown once and
    /// not written down is worse than no code, because it is believed.
    @Test
    func aCorrectPasteBackFinishesAndForgetsTheWords() async {
        let model = RecoveryCodeModel(publish: { Self.code })
        await model.start()
        model.beginVerification()

        model.confirm(Self.code)

        #expect(model.phase == .done)
        #expect(model.words.isEmpty, "the ceremony must not keep the code after it ends")
    }

    /// A user retyping twenty-four words off paper will not reproduce the
    /// spacing and has no reason to reproduce the case. The BIP-39 wordlist is
    /// lower-case ASCII, so normalising loses nothing and refusing would fail
    /// somebody who got it right.
    @Test
    func caseAndSpacingAreNotPartOfTheCode() async {
        let model = RecoveryCodeModel(publish: { Self.code })
        await model.start()
        model.beginVerification()

        model.confirm("  WORD0   Word1\nword2 " + (3 ..< 24).map { "word\($0)" }
            .joined(separator: "  ") + "\n")

        #expect(model.phase == .done)
    }

    /// A wrong code must not pass, and must not be a dead end either:
    /// `recovery.md` §Test-recovery affordance is explicit that a user who
    /// fails this gate is forced back to "show me the code again" rather than
    /// being marooned on a code they wrote down wrong.
    @Test
    func aMismatchLeadsBackToTheCode() async {
        let model = RecoveryCodeModel(publish: { Self.code })
        await model.start()
        model.beginVerification()

        model.confirm("word0 word1 word2")
        #expect(model.phase == .mismatch)
        #expect(!model.words.isEmpty, "the code is still needed — it has to be showable again")

        model.showAgain()
        #expect(model.phase == .show)
        #expect(model.rows.count == 6)

        model.beginVerification()
        model.confirm(Self.code)
        #expect(model.phase == .done)
    }

    /// A single missing word is a mismatch. Asserted separately from the
    /// obviously-wrong case above because it is the failure a user actually
    /// makes, and a comparison that only checked a prefix would pass it.
    @Test
    func aTruncatedCodeIsAMismatch() async {
        let model = RecoveryCodeModel(publish: { Self.code })
        await model.start()
        model.beginVerification()

        model.confirm((0 ..< 23).map { "word\($0)" }.joined(separator: " "))
        #expect(model.phase == .mismatch)
    }

    /// A device admitted by pairing holds no `ID_D_priv`, so it cannot seal a
    /// blob — and must not be shown an error for it. The device that created
    /// the account is the one that can, and it already did.
    @Test
    func aPairedDeviceIsToldItIsNotItsJobRatherThanShownAFailure() async {
        let model = RecoveryCodeModel(publish: { nil })
        await model.start()

        #expect(model.phase == .notThisDevice)
        #expect(model.words.isEmpty)
    }

    /// A failed upload must not show a code. The blob is what makes a code
    /// mean anything; showing the words for a blob that was never stored is
    /// the one outcome worse than showing nothing, because the user would then
    /// believe they were covered.
    @Test
    func aFailedUploadShowsNoCode() async {
        struct Boom: LocalizedError {
            var errorDescription: String? { "the relay said no" }
        }
        let model = RecoveryCodeModel(publish: { throw Boom() })
        await model.start()

        #expect(model.phase == .failed("the relay said no"))
        #expect(model.words.isEmpty)
    }

    /// And a failure is retryable, because the state it leaves the user in —
    /// one device holding the only copy of the account key — is the one #181
    /// exists to get out of.
    @Test
    func aFailureCanBeRetried() async {
        actor Attempts {
            private var count = 0
            func next() -> Int {
                count += 1
                return count
            }
        }
        struct Boom: Error {}
        let attempts = Attempts()
        let model = RecoveryCodeModel(publish: {
            if await attempts.next() == 1 { throw Boom() }
            return Self.code
        })

        await model.start()
        guard case .failed = model.phase else {
            Issue.record("expected the first attempt to fail, got \(model.phase)")
            return
        }

        await model.start()
        #expect(model.phase == .show)
    }

    /// The text the Recovery setup screen states verbatim, per
    /// `docs/03-crypto/recovery.md` §What the user is told. Asserted because
    /// it is the whole product promise and its whole cost in one paragraph,
    /// and a paraphrase would be a different promise.
    @Test
    func theWarningSaysWhatTheDocumentSaysItSays() {
        let warning = RecoveryCodeModel.warning
        #expect(warning.contains("end-to-end encrypted"))
        #expect(warning.contains("We cannot reset your account if you lose this code"))
        #expect(warning.contains("we can't recover it for you"))
        #expect(warning.contains("years from now"))
    }
}
