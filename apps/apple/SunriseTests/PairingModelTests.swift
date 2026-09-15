import Foundation
import Testing

@testable import Sunrise

/// Both halves of a pairing, driven against each other in one process.
///
/// The same shape as the seam tests in `crates/sunrise-core-bindings`, and for
/// the same reason: a pairing screen tested on its own can only prove that it
/// calls the seam in *some* order. What has to be true is that the order it
/// calls the seam in is the one the other device's screen is expecting, and
/// the only way to show that is to run both.
///
/// The user is the transport, so the tests are the user: each step takes the
/// text one model is showing and pastes it into the other.
///
/// # Why there is a real vault in here now
///
/// The last leg used to be a canned `PairingFixture`, because a pairing payload
/// was one blob the holding device sealed and the joining device opened. It is
/// three messages since #105, and the middle one is keys the *joining* device
/// mints at random — so the certificate in the last message is signed over
/// values no fixture can know in advance. A stub `sealGrant` would produce a
/// certificate the joiner correctly refuses, which would test the refusal and
/// nothing else. So the sponsor here is a real ``TestVault``, exactly as it is
/// in the app.
@MainActor
struct PairingModelTests {
    /// Somewhere for the new device's `adopt` to put what it opened.
    @MainActor
    final class Sink {
        var root: Data?
        var bundle: Data?
    }

    private static let relay = "https://relay.example"

    private func shownText(_ model: PairingModel) -> String? {
        guard case let .handOff(handOff) = model.phase else { return nil }
        return handOff.text
    }

    private func sas(_ model: PairingModel) -> String? {
        guard case let .comparing(sas) = model.phase else { return nil }
        return sas
    }

    /// A pair of models walked as far as the SAS screen, which is every test
    /// below's starting point but one.
    ///
    /// `sponsor` is `nil` for the tests that are about the handshake and never
    /// reach a message the vault has to answer.
    private func handshake(
        sink: Sink = Sink(),
        sponsor: CoreBridge?
    ) async throws -> (added: PairingModel, holder: PairingModel) {
        let added = PairingModel(
            intent: .addThisMac,
            relayURL: Self.relay,
            adopt: { root, bundle in
                sink.root = root
                sink.bundle = bundle
            }
        )
        let holder = PairingModel(
            intent: .addAnotherDevice,
            sealOffer: sponsor.map { bridge in
                { pairing in try await bridge.sendPairingOffer(to: pairing) }
            },
            sealGrant: sponsor.map { bridge in
                { pairing, request in
                    try await bridge.sendPairingGrant(to: pairing, request: request)
                }
            }
        )

        added.accountEmail = "someone@example.com"
        added.begin()

        // The QR payload, then Noise message 1, both from the device being
        // added — XX starts with the initiator.
        holder.pasted = try #require(shownText(added))
        await holder.submit()
        added.advance()

        holder.pasted = try #require(shownText(added))
        await holder.submit()
        added.advance()

        // Message 2 comes back.
        added.pasted = try #require(shownText(holder))
        await added.submit()
        holder.advance()

        // Message 3 completes the transcript on both sides.
        holder.pasted = try #require(shownText(added))
        await holder.submit()
        added.advance()

        return (added, holder)
    }

    /// The whole point: an account that started on one device ends up admitting
    /// the other, and every leg in between was something a person could do.
    ///
    /// The three legs after the SAS are the ones this change added, and the
    /// direction alternates through them: offer out, request back, grant out.
    @Test
    func theAccountCrossesWhenBothUsersConfirmTheSameDigits() async throws {
        let vault = try await TestVault()
        let sink = Sink()
        let (added, holder) = try await handshake(sink: sink, sponsor: vault.bridge)

        let theirs = try #require(sas(holder))
        let ours = try #require(sas(added))
        #expect(ours == theirs, "two devices that talked to each other agree on the SAS")
        let allDigits = ours.allSatisfy(\.isNumber)
        #expect(ours.count == 6)
        #expect(allDigits, "six decimal digits, as `sunrise-pairing`'s SAS computes them")

        await added.confirm(matched: true)
        await holder.confirm(matched: true)

        // Leg 6: the offer. It names the account and carries no key.
        added.pasted = try #require(shownText(holder))
        await added.submit()
        holder.advance()

        // Leg 7: the request, which only the device being added can produce —
        // it is the public half of keys it just minted.
        holder.pasted = try #require(shownText(added))
        await holder.submit()
        added.advance()

        // Leg 8: the certificate signed over those keys, and the vault with it.
        added.pasted = try #require(shownText(holder))
        await added.submit()
        holder.advance()

        #expect(
            sink.root == Data(repeating: 42, count: 32),
            "the root comes out of the grant, and it is the sponsor's own"
        )
        let bundle = try #require(sink.bundle)
        #expect(!bundle.isEmpty, "and the bundle behind it, which carries the Stream keys")
        if case .done = added.phase {} else { Issue.record("expected done, got \(added.phase)") }
        if case .done = holder.phase {} else { Issue.record("expected done, got \(holder.phase)") }

        await vault.bridge.shutdown()
    }

    /// The seam's own view of the handshake, which the screen shows so that a
    /// bug in this model is visible rather than merely wrong.
    @Test
    func eachSideReportsItsRoleAndWhereTheHandshakeHasGot() async throws {
        let (added, holder) = try await handshake(sponsor: nil)

        #expect(added.role == .newDevice, "the device being added is the one that offers")
        #expect(holder.role == .existingDevice)
        #expect(added.handshakeStep == .awaitingConfirmation)
        #expect(holder.handshakeStep == .awaitingConfirmation)

        await added.confirm(matched: true)
        #expect(added.handshakeStep == .confirmed)
    }

    /// A mismatch is the check working. It must not read as a hiccup, and it
    /// must take the keys with it: the seam discards the session, so there is
    /// nothing left to confirm a second time.
    @Test
    func sayingTheDigitsDifferEndsItAndCannotBeWalkedBack() async throws {
        let (added, _) = try await handshake(sponsor: nil)

        await added.confirm(matched: false)
        #expect(added.phase == .mismatch)
        #expect(added.handshakeStep == nil, "the session is gone, not merely rejected")

        await added.confirm(matched: true)
        #expect(added.phase == .mismatch, "there is nothing left to confirm")
    }

    /// A rejection on one side leaves the other unable to finish, even though
    /// nobody told it. That is the property that makes six digits enough.
    ///
    /// It bites one leg earlier than it used to. The offer carries nothing
    /// worth having, so a MITM that got this far would learn nothing from it —
    /// and the device that rejected cannot even accept the offer, let alone
    /// reach the grant that carries the vault.
    @Test
    func aRejectionOnOneSideStopsTheOtherFromEverGettingTheVault() async throws {
        let vault = try await TestVault()
        let (added, holder) = try await handshake(sponsor: vault.bridge)

        await added.confirm(matched: false)
        await holder.confirm(matched: true)

        // The holder seals for a peer that threw its keys away. The ciphertext
        // exists, and the device that would have to open it no longer can.
        added.pasted = try #require(shownText(holder))
        await added.submit()
        #expect(added.phase == .mismatch, "a discarded pairing accepts nothing")

        await vault.bridge.shutdown()
    }

    /// A mistyped paste is refused where it was typed, not three legs later.
    @Test
    func aPasteThatIsNotAPairingCodeIsRefusedImmediately() async {
        let holder = PairingModel(intent: .addAnotherDevice)
        holder.pasted = "definitely not a pairing payload"
        await holder.submit()

        if case .failed = holder.phase {} else {
            Issue.record("expected a failure, got \(holder.phase)")
        }
        #expect(holder.role == nil, "nothing was constructed")
    }

    /// A Noise message that has been truncated in transit fails to decrypt,
    /// which is the same thing a tampered one looks like.
    @Test
    func aCorruptedHandshakeMessageStopsThePairing() async throws {
        let added = PairingModel(intent: .addThisMac, relayURL: Self.relay)
        let holder = PairingModel(intent: .addAnotherDevice)
        added.accountEmail = "someone@example.com"
        added.begin()

        holder.pasted = try #require(shownText(added))
        await holder.submit()
        added.advance()

        holder.pasted = String(try #require(shownText(added)).dropLast(8))
        await holder.submit()
        if case .failed = holder.phase {} else {
            Issue.record("expected a failure, got \(holder.phase)")
        }
    }

    /// Cancelling has to leave the app somewhere it can start again from.
    /// A half-finished handshake that could not be abandoned would be a modal
    /// with no way out.
    @Test
    func cancellingMidHandshakeDiscardsTheKeysAndReturnsToTheStart() async throws {
        let (added, holder) = try await handshake(sponsor: nil)

        added.cancel()
        #expect(added.phase == .idle)
        #expect(added.handshakeStep == nil)
        #expect(added.pasted.isEmpty)

        holder.cancel()
        #expect(holder.handshakeStep == nil)
        guard case let .awaiting(prompt) = holder.phase else {
            Issue.record("the device with the vault goes back to its paste field")
            return
        }
        #expect(prompt.leg == .code)
    }

    /// The screen that offers to share a vault is reachable from a window that
    /// has one open, but the bridge behind it can go away. Saying so beats
    /// producing a sealed blob that carries nothing.
    @Test
    func sharingWithNoOpenVaultFailsRatherThanSealingNothing() async throws {
        let (added, holder) = try await handshake(sponsor: nil)

        await added.confirm(matched: true)
        await holder.confirm(matched: true)

        #expect(holder.phase == .failed(PairingUIError.noOpenVault.localizedDescription))
    }

    /// The QR names an account without naming a person.
    @Test
    func theAccountTagIsTheSeamsHashAndNotTheAddress() {
        let model = PairingModel(intent: .addThisMac)
        #expect(model.accountTag.isEmpty, "nothing to hash yet")

        model.accountEmail = "  Someone@Example.com  "
        #expect(model.accountTag == pairingAccountTag(accountEmail: "Someone@Example.com"))
        #expect(model.accountTag.count == 8)
        #expect(!model.accountTag.contains("@"))
    }

    /// Eight legs, and the screen says which one you are on — the difference
    /// between a wizard and a wall of blobs.
    @Test
    func theScreenSaysHowFarThroughTheHandshakeItIs() async throws {
        let added = PairingModel(intent: .addThisMac, relayURL: Self.relay)
        #expect(added.progress == nil, "nothing has started")

        added.accountEmail = "someone@example.com"
        added.begin()
        #expect(added.progress?.leg == 1)
        #expect(added.progress?.of == 8, "six until the signing key stopped travelling")

        let (walked, _) = try await handshake(sponsor: nil)
        #expect(walked.progress?.leg == 5, "the SAS is still the fifth leg")
    }

    /// A device added by pairing holds the account's public identity and no
    /// signing key, so it cannot certify a third device. The screen asks before
    /// it offers, rather than after eight legs of copying.
    @Test
    func aVaultThatCanSponsorSaysSoAndOneCreatedByPairingWouldNot() async throws {
        let vault = try await TestVault()
        let canSponsor = await vault.bridge.canSponsorPairing()
        #expect(
            canSponsor,
            "a vault opened without a pairing bundle created its own account, so it holds ID_S_priv"
        )
        await vault.bridge.shutdown()
    }
}
