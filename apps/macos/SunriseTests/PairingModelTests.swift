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
@MainActor
struct PairingModelTests {
    /// Somewhere for the new device's `adopt` to put what it opened.
    @MainActor
    final class Sink {
        var root: Data?
    }

    private static let relay = "wss://relay.example/sync"
    private static let vaultRoot = Data(repeating: 0xAB, count: 32)

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
    private func handshake(
        sink: Sink = Sink(),
        sealRoot: ((DevicePairing) async throws -> String)? = { pairing in
            try pairing.sealVaultRoot(vaultRoot: vaultRoot)
        }
    ) async throws -> (added: PairingModel, holder: PairingModel) {
        let added = PairingModel(
            intent: .addThisMac,
            relayURL: Self.relay,
            adopt: { root in sink.root = root }
        )
        let holder = PairingModel(intent: .addAnotherDevice, sealRoot: sealRoot)

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

    /// The whole point: a vault root that started on one device ends up on the
    /// other, and every leg in between was something a person could do.
    @Test
    func theRootCrossesWhenBothUsersConfirmTheSameDigits() async throws {
        let sink = Sink()
        let (added, holder) = try await handshake(sink: sink)

        let theirs = try #require(sas(holder))
        let ours = try #require(sas(added))
        #expect(ours == theirs, "two devices that talked to each other agree on the SAS")
        let allDigits = ours.allSatisfy(\.isNumber)
        #expect(ours.count == 6)
        #expect(allDigits, "six decimal digits, as `sunrise-pairing`'s SAS computes them")

        await added.confirm(matched: true)
        await holder.confirm(matched: true)

        // The sealed root is the last thing the user carries.
        added.pasted = try #require(shownText(holder))
        await added.submit()
        holder.advance()

        #expect(sink.root == Self.vaultRoot)
        if case .done = added.phase {} else { Issue.record("expected done, got \(added.phase)") }
        if case .done = holder.phase {} else { Issue.record("expected done, got \(holder.phase)") }
    }

    /// The seam's own view of the handshake, which the screen shows so that a
    /// bug in this model is visible rather than merely wrong.
    @Test
    func eachSideReportsItsRoleAndWhereTheHandshakeHasGot() async throws {
        let (added, holder) = try await handshake()

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
        let (added, _) = try await handshake()

        await added.confirm(matched: false)
        #expect(added.phase == .mismatch)
        #expect(added.handshakeStep == nil, "the session is gone, not merely rejected")

        await added.confirm(matched: true)
        #expect(added.phase == .mismatch, "there is nothing left to confirm")
    }

    /// A rejection on one side leaves the other unable to finish, even though
    /// nobody told it. That is the property that makes six digits enough.
    @Test
    func aRejectionOnOneSideStopsTheOtherFromEverGettingTheRoot() async throws {
        let (added, holder) = try await handshake()

        await added.confirm(matched: false)
        await holder.confirm(matched: true)

        // The holder seals for a peer that threw its keys away. The ciphertext
        // exists, and the device that would have to open it no longer can.
        let sealed = try #require(shownText(holder))
        added.pasted = sealed
        await added.submit()
        #expect(added.phase == .mismatch, "a discarded pairing accepts nothing")
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
        let (added, holder) = try await handshake()

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
        let (added, holder) = try await handshake(sealRoot: nil)

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

    /// Six legs, and the screen says which one you are on — the difference
    /// between a wizard and a wall of blobs.
    @Test
    func theScreenSaysHowFarThroughTheHandshakeItIs() async throws {
        let added = PairingModel(intent: .addThisMac, relayURL: Self.relay)
        #expect(added.progress == nil, "nothing has started")

        added.accountEmail = "someone@example.com"
        added.begin()
        #expect(added.progress?.leg == 1)
        #expect(added.progress?.of == 6)

        let (walked, _) = try await handshake()
        #expect(walked.progress?.leg == 5, "the SAS is the fifth leg")
    }
}
