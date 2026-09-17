import Foundation
import Testing

@testable import Sunrise

/// The device list, against a real vault over the real seam.
///
/// Against a real core and not a fake, for the reason `CoreBridgeTests` gives:
/// what #144 is about is three facts the core already held and no client read,
/// so a test that handed the model its own fixture rows would assert that the
/// model can render a struct — which was never the thing that was broken.
///
/// The same file compiles into `SunriseTests` and `SunriseiOSTests`, so every
/// assertion here runs on both clients.
@MainActor
struct DeviceListModelTests {
    /// The list reaches the core at all, and the row it finds is this device.
    ///
    /// The baseline every other assertion departs from, and the one that says
    /// an Apple device list exists: before this, `Query::DeviceList` crossed
    /// the seam and nothing in either app called it.
    @Test
    func theListFindsThisDevice() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.errorMessage == nil)
        #expect(model.rows.count == 1)
        let me = try #require(model.rows.first)
        #expect(me.isThisDevice)
        #expect(!me.revoked)
        #expect(me.current)
        await vault.bridge.shutdown()
    }

    /// **#144's first signal.** A vault that created its own account holds
    /// `ID_D_priv`, and the list says so.
    ///
    /// `docs/03-crypto/key-rotation.md` described this condition as "disclosed
    /// in these documents and nowhere a user can see". This is the assertion
    /// that stops being true if that goes back to being the case: flip
    /// `Keychain::holds_only_copy_of_identity_key` to `false` and it fails.
    @Test
    func theListDisclosesThatThisVaultHoldsTheIdentityKey() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        #expect(
            model.holdsIdentityKey,
            """
            a vault that founded its own account holds the only copy of \
            ID_D_priv until a recovery code is sealed, and the device list is \
            where that is said
            """
        )
        await vault.bridge.shutdown()
    }

    /// **#144's second signal.** Every row carries whether that device turned
    /// up after the account had revoked something.
    ///
    /// One device in a fresh vault cannot be in that state — the account has
    /// revoked nothing — so what this pins is that the field crosses the seam
    /// and reaches a row at all. Its *truth* is asserted four ways in
    /// `sunrise-core`'s engine tests, including the case no other column shows:
    /// a readmission under the identity in force, which reads
    /// `revoked: false, current: true`.
    @Test
    func everyRowCarriesTheReadmissionSignal() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        let me = try #require(model.rows.first)
        #expect(!me.admittedAfterRevocation)
        await vault.bridge.shutdown()
    }

    /// The row carries an id a revocation will take.
    ///
    /// Two forms, and the list needs both: `deviceID` is the hex a user reads,
    /// and `id` is the `dev_…` reference `Command::RevokeDevice` parses. Before
    /// `DeviceListRow.device_ref` crossed the seam a Swift client could list
    /// its devices and revoke none of them, because converting between the two
    /// means reimplementing Crockford base32 against a Rust encoder.
    @Test
    func aRowCanBeActedOn() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        let me = try #require(model.rows.first)
        #expect(me.id.hasPrefix("dev_"))
        #expect(me.deviceID.count == 32)
        let isHex = me.deviceID.allSatisfy(\.isHexDigit)
        #expect(isHex)
        await vault.bridge.shutdown()
    }

    /// **#144's third signal.** A revocation reports what it could not do.
    ///
    /// A vault's only device cannot revoke itself — rotating every key away
    /// from the only device holding them is not a recoverable state — so this
    /// asserts the disclosure path through its failure: the error surfaces
    /// rather than the list silently reporting a removal that did not happen.
    /// The `unrotated_streams` plumbing itself is asserted by
    /// `DeviceListModel.Revocation` being built from `outcome.unrotatedStreams`
    /// and by `sunrise-core`'s
    /// `a_revocation_that_cannot_rotate_every_stream_says_which_it_could_not`,
    /// which needs a corrupted `stream_keys` row this seam cannot make.
    @Test
    func aRefusedRevocationSaysSoRatherThanClaimingSuccess() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()
        let me = try #require(model.rows.first)

        await model.revoke(me, reason: .stolen)

        #expect(model.lastRevocation == nil, "nothing was removed, so nothing is disclosed")
        #expect(model.errorMessage != nil, "and the refusal reaches the user")
        #expect(!model.busy)
        await vault.bridge.shutdown()
    }

    /// **#144's third signal, at the line that can break.** The disclosure
    /// carries the streams the revocation could not rotate.
    ///
    /// Driven from a synthetic `CommandOutcome` rather than a real revocation,
    /// because producing a non-empty list needs a `stream_keys` row whose
    /// `stream_id` column is not 16 bytes — `sunrise-core`'s
    /// `a_revocation_that_cannot_rotate_every_stream_says_which_it_could_not`
    /// corrupts one in place, and nothing reachable from this seam can. What is
    /// left to get wrong on this side is the carrying, and this is the test
    /// that fails when it is dropped.
    @Test
    func theDisclosureCarriesTheStreamsARevocationCouldNotRotate() {
        let outcome = CommandOutcome(
            entity: "dev_00000000000000000000000000",
            state: nil,
            opId: String(repeating: "0", count: 32),
            seq: 1,
            softViolations: [],
            unrotatedStreams: ["00aabb", "00ccdd"],
            revocationGated: false
        )
        let disclosure = DeviceListModel.Revocation(
            nickname: "Old laptop",
            outcome: outcome,
            relayPending: true
        )
        #expect(
            disclosure.unrotatedStreams == ["00aabb", "00ccdd"],
            """
            a client that dropped these would be telling a user their stolen \
            laptop had been cut off from streams it can still read
            """
        )
        #expect(disclosure.relayPending)
        #expect(!disclosure.gated, "this removal happened; only its rotation was partial")
    }

    /// **The strongest form of "it did not happen" reaches the disclosure.**
    ///
    /// `revocationGated` means the account discarded this vault's own
    /// `device_revoke` op, because this vault has itself been revoked and a
    /// revoked device's revocations of third parties are stored and skipped on
    /// every replica. Nothing was cut: the target stays current, keeps
    /// receiving keys, and the relay was deliberately not told.
    ///
    /// Driven from a synthetic `CommandOutcome` for the same reason as the
    /// test above — producing a real one needs a vault that has revoked the
    /// device running the test, which this seam cannot build.
    /// `sunrise-core`'s `revoke_device_reports_that_the_fold_discarded_its_own_op`
    /// asserts the field is set; this asserts it is carried, which is the line
    /// on this side that can be dropped.
    @Test
    func theDisclosureSaysWhenTheAccountDiscardedTheRemoval() {
        let outcome = CommandOutcome(
            entity: "dev_00000000000000000000000000",
            state: nil,
            opId: String(repeating: "0", count: 32),
            seq: 1,
            softViolations: [],
            unrotatedStreams: [],
            revocationGated: true
        )
        let disclosure = DeviceListModel.Revocation(
            nickname: "Old laptop",
            outcome: outcome,
            relayPending: true
        )
        #expect(
            disclosure.gated,
            """
            a client that dropped this would print "Removed Old laptop" over a \
            device that is still current on every replica
            """
        )
    }

    /// The disclosure is dismissible and the error is too, so neither becomes
    /// a permanent fixture of the settings screen.
    @Test
    func theDisclosuresCanBeDismissed() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()
        let me = try #require(model.rows.first)
        await model.revoke(me, reason: .lost)

        #expect(model.errorMessage != nil)
        model.dismissError()
        #expect(model.errorMessage == nil)
        model.dismissRevocation()
        #expect(model.lastRevocation == nil)
        await vault.bridge.shutdown()
    }
}
