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
            revocationGated: false,
            revocationUnwound: []
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
    /// receiving keys, and this revocation tells the relay nothing.
    ///
    /// Driven from a synthetic `CommandOutcome` for the same reason as the
    /// test above — producing a real one needs a vault that has revoked the
    /// device running the test, which this seam cannot build.
    /// `sunrise-core`'s `revoke_device_reports_that_the_fold_discarded_its_own_op`
    /// asserts the field is set; this asserts it is carried, which is the line
    /// on this side that can be dropped.
    ///
    /// **`relayPending` is `false` because a gated revocation queues no
    /// intent of its own**, which is the whole of what this case pins.
    /// A synthetic outcome can be handed anything, so it has to be handed what
    /// the account would actually produce or the case is named for a state
    /// that cannot occur. `relayPending` is not a field of the outcome: it is
    /// read off the bridge by ``DeviceListModel/revoke(_:reason:)``, and
    /// `Core::relay_revocation_pending` answers it from a row in
    /// `relay_revocation_intents` — a queued intent that **will** be sent.
    /// The gated path inserts none, which the cited Rust test asserts in the
    /// same breath as the flag, and which is what "this revocation tells the
    /// relay nothing" above means. The second assertion below holds the
    /// fixture to it.
    ///
    /// It is the only value this state can hold. The fold that unwinds a
    /// register row leaves `relay_revocation_intents` untouched, so an intent
    /// queued by an *earlier*, effective revocation of the same device can
    /// outlive that device unwinding back to current — but an intent is owed,
    /// and `Core::relay_revocation_pending` answers `true`, only while the
    /// register calls its device revoked, and a gated revocation leaves its
    /// target current. So the old row is held rather than sent, and a gated
    /// outcome cannot sit beside a `relayPending` that is `true`
    /// ([#257](https://github.com/justin13888/Sunrise/issues/257);
    /// `sunrise-core`'s
    /// `a_relay_intent_is_owed_only_while_the_register_calls_its_device_revoked`
    /// pins it).
    @Test
    func theDisclosureSaysWhenTheAccountDiscardedTheRemoval() {
        let outcome = CommandOutcome(
            entity: "dev_00000000000000000000000000",
            state: nil,
            opId: String(repeating: "0", count: 32),
            seq: 1,
            softViolations: [],
            unrotatedStreams: [],
            revocationGated: true,
            revocationUnwound: []
        )
        let disclosure = DeviceListModel.Revocation(
            nickname: "Old laptop",
            outcome: outcome,
            relayPending: false
        )
        #expect(
            disclosure.gated,
            """
            a client that dropped this would print "Removed Old laptop" over a \
            device that is still current on every replica
            """
        )
    }

    /// **Whether a removal carries the account's authority reaches the
    /// disclosure, in each of its three shapes** (ADR-0058 §7).
    ///
    /// Synthetic for the reason the two tests above give: a real removal
    /// needs a second device, and this seam's test vault has one.
    /// `sunrise-core`'s `revocation_authority` tests assert the core sets the
    /// fields, and the seam tests assert they cross; this asserts the model
    /// reads them, which is the line on this side that can be dropped.
    @Test
    func theDisclosureSaysWhetherTheRemovalCarriesTheAccountsAuthority() {
        func outcome(signed: Bool, awaiting: [String]) -> CommandOutcome {
            CommandOutcome(
                entity: "dev_00000000000000000000000000",
                state: nil,
                opId: String(repeating: "0", count: 32),
                seq: 1,
                softViolations: [],
                unrotatedStreams: [],
                revocationGated: false,
                revocationUnwound: [],
                revokedByAuthority: signed,
                authorityAwaitsUpdate: awaiting
            )
        }
        func authority(signed: Bool, awaiting: [String]) -> DeviceListModel.Revocation.Authority {
            DeviceListModel.Revocation(
                nickname: "Old laptop",
                outcome: outcome(signed: signed, awaiting: awaiting),
                relayPending: false
            ).authority
        }

        #expect(authority(signed: true, awaiting: []) == .signed)
        #expect(
            authority(signed: false, awaiting: []) == .ordinary,
            "a removal from a device that cannot sign must not be called permanent"
        )
        #expect(
            authority(signed: false, awaiting: ["00b1", "00b2"]) == .awaitsUpdate(["00b1", "00b2"]),
            "the devices to update must reach the view whole, or the user has no remedy to act on"
        )
    }

    /// Every row carries whether the account identity removed it. A fresh
    /// vault's one device has not been removed at all, so this pins that the
    /// field reaches a row and reads `false`; its truth is asserted by
    /// `sunrise-core`'s `revocation_authority` tests and its crossing by
    /// `seam_device_rows.rs`.
    @Test
    func everyRowCarriesWhetherTheAccountIdentityRemovedIt() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        let me = try #require(model.rows.first)
        #expect(!me.revokedByAuthority)
        await vault.bridge.shutdown()
    }

    /// **The offer.** Every row carries whether this device may withdraw its
    /// removal, and a fresh vault's one device has removed nothing, so it
    /// reads `false` and the row offers no "Withdraw removal". Its truth on a
    /// removed row is asserted by `sunrise-core`'s `revocation_withdraw` tests
    /// and its crossing by `seam_device_rows.rs`; this pins that the model
    /// reads it rather than deriving it from ``DeviceListModel/DeviceRow/revoked``.
    @Test
    func everyRowCarriesWhetherThisDeviceMayWithdrawItsRemoval() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()

        let me = try #require(model.rows.first)
        #expect(!me.withdrawable)
        await vault.bridge.shutdown()
    }

    /// A withdrawal this device has nothing to withdraw for reports that
    /// nothing happened, over the real seam.
    ///
    /// The core answers `notAuthor` rather than failing, because the list's
    /// removed rows include removals other devices made; the report says
    /// "Nothing was withdrawn" and nothing after it — no keys, no relay line.
    @Test
    func withdrawingARemovalThisDeviceNeverMadeWithdrawsNothing() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()
        let stranger = DeviceListModel.DeviceRow(
            id: "dev_00000000000000000000000001",
            deviceID: String(repeating: "0", count: 31) + "1",
            nickname: "Old laptop",
            platform: "macos",
            revoked: true,
            readBounded: true,
            current: true,
            admittedAfterRevocation: false,
            revokedByAuthority: false,
            withdrawable: true,
            isThisDevice: false
        )

        await model.withdraw(stranger)

        #expect(model.errorMessage == nil)
        let done = try #require(model.lastWithdrawal)
        #expect(done.outcome == .notAuthor)
        #expect(
            done.lines(device: "Mac") == [
                L10n.Devices.withdrawNotAuthorTitle,
                L10n.Devices.withdrawNotAuthor(device: "Mac", name: "Old laptop")
            ]
        )
        #expect(!model.busy)
        model.dismissWithdrawal()
        #expect(model.lastWithdrawal == nil)
        await vault.bridge.shutdown()
    }

    /// A device cannot withdraw a removal of itself; the refusal reaches the
    /// user and no report claims a withdrawal.
    @Test
    func aRefusedWithdrawalSaysSoRatherThanReportingOne() async throws {
        let vault = try await TestVault()
        let model = DeviceListModel(bridge: vault.bridge)
        await model.refresh()
        let me = try #require(model.rows.first)

        await model.withdraw(me)

        #expect(model.lastWithdrawal == nil)
        #expect(model.errorMessage != nil)
        #expect(!model.busy)
        await vault.bridge.shutdown()
    }

    /// **Each outcome's copy** (ADR-0056 §6). Only `reinstated` says the
    /// device is back, and the relay's half is said on every emitted outcome.
    ///
    /// Synthetic, because each outcome but `notAuthor` needs a second device
    /// this seam's test vault does not have; `sunrise-core`'s
    /// `revocation_withdraw` tests assert the core reaches each one.
    @Test
    func eachWithdrawalOutcomeSaysWhatItDid() {
        func lines(
            _ kind: WithdrawalOutcomeKind,
            keys: UInt32 = 0,
            released: Bool = true,
            cancelled: Bool = true,
            relay: RelayReinstatementKind = .clear
        ) -> [String] {
            let outcome = CommandOutcome(
                entity: "dev_00000000000000000000000001",
                state: nil,
                opId: String(repeating: "0", count: 32),
                seq: 1,
                softViolations: [],
                unrotatedStreams: [],
                revocationGated: false,
                revocationUnwound: [],
                withdrawal: WithdrawalReport(
                    outcome: kind,
                    readBoundReleased: released,
                    keysResent: keys,
                    relayIntentCancelled: cancelled
                )
            )
            return DeviceListModel.Withdrawal(nickname: "Old laptop", outcome: outcome, relay: relay)
                .lines(device: "Mac")
        }
        let back = L10n.Devices.withdrawnReinstated(name: "Old laptop")

        #expect(
            lines(.reinstated, keys: 3) == [
                back,
                L10n.Devices.withdrawnKeys(count: 3, device: "Mac"),
                L10n.Devices.withdrawnRelayCancelled
            ]
        )
        #expect(
            lines(.reinstated, released: false).contains(L10n.Devices.withdrawnStillBounded(device: "Mac")),
            "back on the register while this replica sends it nothing must say so"
        )
        #expect(
            lines(.stillClaimed, cancelled: false) == [
                L10n.Devices.withdrawnStillClaimedTitle(name: "Old laptop"),
                L10n.Devices.withdrawnStillClaimed,
                L10n.Devices.withdrawnRelayTold(name: "Old laptop")
            ]
        )
        #expect(
            lines(.repairRequired) == [
                L10n.Devices.withdrawnRepairTitle(name: "Old laptop"),
                L10n.Devices.withdrawnRepair(name: "Old laptop"),
                L10n.Devices.withdrawnRelayCancelled
            ]
        )
        for kind in [WithdrawalOutcomeKind.notAuthor, .stillClaimed, .repairRequired] {
            #expect(!lines(kind).contains(back), "only a reinstated device may be called back")
        }

        // The relay's half says where the reinstatement stands (ADR-0056 §5),
        // and a cancelled telling says so whatever was read.
        #expect(
            lines(.reinstated, cancelled: false, relay: .pending).last
                == L10n.Devices.withdrawnRelayQueued(name: "Old laptop")
        )
        #expect(
            lines(.reinstated, cancelled: false, relay: .refused).last
                == L10n.Devices.withdrawnRelayRefused(name: "Old laptop", device: "Mac")
        )
        for relay in [RelayReinstatementKind.clear, .pending, .refused] {
            #expect(lines(.reinstated, relay: relay).last == L10n.Devices.withdrawnRelayCancelled)
        }
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

extension DeviceListModelTests {
    /// **The relay's state is read only after an uncancelled telling**
    /// (ADR-0056 §5), and what the read answers reaches the report: a
    /// `pending` read prints the queued line. A cancelled telling never asks,
    /// and a failed read falls back to `clear`.
    ///
    /// Synthetic: an uncancelled telling exists only once a sync delivered
    /// the revocation to a relay, and `sunrise-e2e`'s `relay_reinstatement`
    /// asserts the core's answer along that path.
    @Test
    func theRelaysStateIsReadOnlyAfterAnUncancelledTelling() async {
        struct ReadFailed: Error {}
        func outcome(cancelled: Bool) -> CommandOutcome {
            CommandOutcome(
                entity: "dev_00000000000000000000000001",
                state: nil,
                opId: String(repeating: "0", count: 32),
                seq: 1,
                softViolations: [],
                unrotatedStreams: [],
                revocationGated: false,
                revocationUnwound: [],
                withdrawal: WithdrawalReport(
                    outcome: .reinstated,
                    readBoundReleased: true,
                    keysResent: 0,
                    relayIntentCancelled: cancelled
                )
            )
        }
        let told = outcome(cancelled: false)

        let relay = await DeviceListModel.relayAfterWithdrawal(told) { .pending }
        #expect(relay == .pending)
        #expect(
            DeviceListModel.Withdrawal(nickname: "Old laptop", outcome: told, relay: relay)
                .lines(device: "Mac").last == L10n.Devices.withdrawnRelayQueued(name: "Old laptop"),
            "an uncancelled telling with a queued reinstatement says it is queued"
        )
        #expect(await DeviceListModel.relayAfterWithdrawal(told) { .refused } == .refused)
        #expect(
            await DeviceListModel.relayAfterWithdrawal(told) { throw ReadFailed() } == .clear,
            "a failed read must not claim a queued request or a refusal"
        )

        var asked = false
        let cancelled = await DeviceListModel.relayAfterWithdrawal(outcome(cancelled: true)) {
            asked = true
            return .pending
        }
        #expect(cancelled == .clear)
        #expect(!asked, "a cancelled telling leaves nothing to read")
    }
}
