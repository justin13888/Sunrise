import Foundation

/// What has access to this account, and the three things the core knew about
/// it that no user could see.
///
/// The device list is the surface that answers "what has access to this
/// account", so `docs/07-clients/parity-matrix.md` §Device list and revocation
/// was met by the CLI alone until now: `Query::DeviceList`,
/// `Query::IdentityStatus` and `Command::RevokeDevice` all crossed the UniFFI
/// seam and nothing in either Apple app called them.
///
/// Three signals land here together, because all three are answers to that one
/// question and splitting them across screens would make each of them look like
/// trivia ([#144](https://github.com/justin13888/Sunrise/issues/144)):
///
/// 1. **``holdsIdentityKey``** — this vault holds `ID_D_priv`. Losing it with
///    no recovery code sealed destroys the key permanently, and no recovery
///    feature added later retrieves it, because sealing a blob needs the key it
///    would carry. It was stated in `docs/03-crypto/recovery.md` and nowhere a
///    user could see.
/// 2. **``DeviceRow/admittedAfterRevocation``** — this device id turned up after
///    the account had revoked something. It was a `tracing::warn!` and reached
///    an operator reading NDJSON.
/// 3. **``Revocation/unrotatedStreams``** — the streams a revocation could not
///    rotate. The CLI has disclosed these since they existed; this is the half
///    that was deferred to this issue.
///
/// A fourth landed later and belongs to the same question:
/// ``Revocation/gated`` — the account discarded this vault's own revocation,
/// because this vault has itself been revoked. It is the strongest form of
/// "what you asked for did not happen", so the view says that instead of the
/// removal report and not beside it.
///
/// A fifth is the one nobody asks for and everybody needs:
/// ``DeviceRow/readBounded`` and ``Revocation/unwound``. The revocation
/// register is a fold, so applying one removal can discard another whose own
/// author was removed first — and the device that drops off goes back to
/// reading as an ordinary member while still receiving no keys. Both halves
/// are rendered: the row says what the device's state actually is, and the
/// disclosure says it changed and what to do about it.
///
/// Snapshots, refreshed off the change feed, because a view body cannot await
/// an actor — the same shape every other model in this app uses.
@MainActor
@Observable
final class DeviceListModel {
    /// One device, as the list renders it.
    struct DeviceRow: Identifiable, Equatable {
        /// The `dev_…` reference, which is also what a revocation takes.
        let id: String
        /// Lowercase hex, which is what a user sees and what the CLI prints.
        let deviceID: String
        let nickname: String
        let platform: String
        /// The account **currently** records this device as removed.
        ///
        /// Derived and reversible: since ADR-0041 the register is a fold over
        /// the revocation ledger, so this goes back to `false` when the ledger
        /// shows a removal's own author had been removed first. Read it beside
        /// ``readBounded``, which does not move back.
        let revoked: Bool
        /// This device receives no keys and never will, whatever ``revoked``
        /// says.
        ///
        /// `readBounded: true, revoked: false` is the unwind, and the reason
        /// this field is on screen at all: the account stopped recording the
        /// removal and calls the device current again, while it goes on
        /// receiving nothing. Without the line the row is indistinguishable
        /// from an ordinary member.
        ///
        /// A state, not an accusation, and it has a remedy: the ordinary cause
        /// is retiring two devices months apart, and the fix is to remove it
        /// again from a device the account still trusts.
        let readBounded: Bool
        /// Certified under the identity **in force**.
        ///
        /// Not the negation of ``revoked``. Rendered as "not active on this
        /// account" and never as an accusation: an honest device that has not
        /// applied a rotation yet reads exactly the same way for a moment.
        let current: Bool
        /// First seen after this vault had already recorded a revocation.
        ///
        /// Also not an accusation. Since #221 a device admitted by pairing
        /// holds no signing key and cannot certify a fresh id at all, so the
        /// ordinary cause is an honest pairing in an account that revoked
        /// something earlier. What it covers that no other field here does is
        /// a revoked *creator* certifying itself back in after a revocation
        /// that could not rotate the identity — that row reads
        /// `revoked: false, current: true` and is otherwise indistinguishable
        /// from any other member.
        let admittedAfterRevocation: Bool
        /// Removed **by the account identity**: a removal signed with the
        /// account's key names this device (ADR-0058 §7).
        ///
        /// The one removal only the device holding that identity can take
        /// back, and only while that identity is in force; no other device
        /// can. Every other removal is ordinary, and a later change on the
        /// account can take it back off the list (ADR-0041), which is what
        /// ``readBounded`` without ``revoked`` looks like afterwards. The row
        /// words the two differently so a user can tell them apart. Never
        /// `true` without ``revoked``.
        let revokedByAuthority: Bool
        /// **This** device holds a live ordinary removal of this row's device,
        /// and may withdraw it (ADR-0056 §6).
        ///
        /// The only rows that offer "Withdraw removal". A removal another
        /// device made is that device's to withdraw, and one this device
        /// signed as the account identity is not withdrawn at all. Not implied
        /// by ``revoked``: a removal of this device's that the account gates
        /// is still this device's to withdraw.
        let withdrawable: Bool
        /// Whether this row is the device the user is holding.
        let isThisDevice: Bool
    }

    /// What a finished withdrawal achieved (ADR-0056 §6).
    ///
    /// Only ``WithdrawalOutcomeKind/reinstated`` means the device is back, so
    /// the copy is chosen here, where a test reads it, rather than in the view:
    /// a client that printed "back" over `stillClaimed` or `repairRequired`
    /// would tell a user a device was readmitted that the account still keeps
    /// out.
    struct Withdrawal: Equatable {
        let nickname: String
        let outcome: WithdrawalOutcomeKind
        /// This device released the device's read bound, so it is sent keys
        /// again.
        let readBoundReleased: Bool
        /// Keys sealed to the device, the ones minted while it was revoked
        /// included.
        let keysResent: Int
        /// This device's unsent telling of the relay was cancelled. `false`
        /// means the relay may already refuse the device, and nothing can tell
        /// it otherwise until the relay's reinstatement lands
        /// ([#1299](https://github.com/justin13888/Sunrise/issues/1299)).
        let relayIntentCancelled: Bool

        /// Read straight off what the command returned. A missing report is
        /// read as `notAuthor`, the one outcome that claims nothing happened.
        init(nickname: String, outcome: CommandOutcome) {
            self.nickname = nickname
            let report = outcome.withdrawal
            self.outcome = report?.outcome ?? .notAuthor
            readBoundReleased = report?.readBoundReleased ?? false
            keysResent = Int(report?.keysResent ?? 0)
            relayIntentCancelled = report?.relayIntentCancelled ?? false
        }

        /// The report, title first. `device` is the platform's word for the
        /// device the user holds.
        func lines(device: String) -> [String] {
            let name = nickname.isEmpty ? L10n.Devices.unnamed : nickname
            var out: [String]
            switch outcome {
            case .notAuthor:
                // Nothing was emitted, so nothing below applies: no keys moved
                // and the relay's telling was not touched.
                return [
                    L10n.Devices.withdrawNotAuthorTitle,
                    L10n.Devices.withdrawNotAuthor(device: device, name: name)
                ]
            case .stillClaimed:
                out = [
                    L10n.Devices.withdrawnStillClaimedTitle(name: name),
                    L10n.Devices.withdrawnStillClaimed
                ]
            case .repairRequired:
                out = [
                    L10n.Devices.withdrawnRepairTitle(name: name),
                    L10n.Devices.withdrawnRepair(name: name)
                ]
            case .reinstated:
                out = [L10n.Devices.withdrawnReinstated(name: name)]
            }
            if keysResent > 0 {
                out.append(L10n.Devices.withdrawnKeys(count: keysResent, device: device))
            }
            // The CLI's rule: back on the register, yet this replica would not
            // release the bound, because its own word does not count (§4 (c)).
            if outcome == .reinstated, !readBoundReleased {
                out.append(L10n.Devices.withdrawnStillBounded(device: device))
            }
            out.append(
                relayIntentCancelled
                    ? L10n.Devices.withdrawnRelayCancelled
                    : L10n.Devices.withdrawnRelayTold(name: name)
            )
            return out
        }
    }

    /// What a finished revocation actually achieved, in the two halves that
    /// are different guarantees.
    struct Revocation: Equatable {
        let nickname: String
        /// Streams the rotation could not reach. Non-empty means the revoked
        /// device still holds whatever key it was last given for them.
        let unrotatedStreams: [String]
        /// `true` while the relay has not been told. A queued intent, sent on
        /// the next sync — which is the half a user pressing the button
        /// believes they are getting (#160).
        let relayPending: Bool
        /// `true` when **nothing was removed**: this vault has itself been
        /// revoked, so the account discards its revocations of other devices.
        ///
        /// Not a failure and not an error — the command succeeded, the keys
        /// rotated, and the op is kept and re-judged whenever another
        /// revocation lands. It is a claim the view must not make: the target
        /// is still current on every replica, it still receives new keys, and
        /// this revocation queues no relay intent of its own. Same disclosure
        /// rule as ``unrotatedStreams`` at the other end of the scale.
        let gated: Bool
        /// Devices the account has stopped recording a removal of, hex ids.
        ///
        /// Not about the device the user just acted on. Applying any removal
        /// re-folds the whole register, which can discard an older removal
        /// whose own author the ledger removes — and those devices go back to
        /// reading as ordinary members while still receiving nothing. The
        /// standing set rather than this command's delta, so a user who missed
        /// it last time is told again.
        let unwound: [String]
        /// Which kind of removal this was (ADR-0058 §7).
        let authority: Authority

        /// Whether a removal carries the account identity's signature, and if
        /// it could have but did not, what stood in the way.
        enum Authority: Equatable {
            /// Signed by the account identity: only the device holding that
            /// identity can take it back.
            case signed
            /// Made by a device that does not hold the account's signing key,
            /// which is every device added by pairing. A later change on the
            /// account can take it back off the list.
            case ordinary
            /// This device holds the key, but these devices (hex ids) run a
            /// build that cannot read a signed removal, so it went out
            /// ordinary. Not refused or delayed: the device being removed is
            /// often the one the user lost.
            case awaitsUpdate([String])

            init(outcome: CommandOutcome) {
                if outcome.revokedByAuthority {
                    self = .signed
                } else if outcome.authorityAwaitsUpdate.isEmpty {
                    self = .ordinary
                } else {
                    self = .awaitsUpdate(outcome.authorityAwaitsUpdate)
                }
            }
        }

        /// Read straight off what the command returned.
        ///
        /// A named initialiser rather than three assignments inside
        /// ``DeviceListModel/revoke(_:reason:)`` so that the one line #144
        /// exists to add — carrying `unrotatedStreams` out of the outcome —
        /// is reachable from a test. Drop it and
        /// `theDisclosureCarriesTheStreamsARevocationCouldNotRotate` goes red;
        /// leave it inline and nothing fails until somebody revokes a device
        /// against a vault with a damaged `stream_keys` row.
        init(nickname: String, outcome: CommandOutcome, relayPending: Bool) {
            self.nickname = nickname
            unrotatedStreams = outcome.unrotatedStreams
            self.relayPending = relayPending
            gated = outcome.revocationGated
            unwound = outcome.revocationUnwound
            authority = Authority(outcome: outcome)
        }
    }

    private(set) var rows: [DeviceRow] = []
    /// Whether this vault holds the account identity key.
    private(set) var holdsIdentityKey = false
    /// The last revocation's disclosure, until the user dismisses it.
    private(set) var lastRevocation: Revocation?
    /// The last withdrawal's report, until the user dismisses it.
    private(set) var lastWithdrawal: Withdrawal?
    private(set) var errorMessage: String?
    private(set) var busy = false

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    /// Re-read everything on screen.
    func refresh() async {
        do {
            let result = try await bridge.query(.deviceList)
            guard case let .devices(devices) = result else {
                errorMessage = L10n.Identity.deviceListUnexpected
                return
            }
            let me = await bridge.deviceId()
            rows = devices.map { row in
                DeviceRow(
                    id: row.deviceRef,
                    deviceID: row.deviceId,
                    nickname: row.nickname,
                    platform: row.platform,
                    revoked: row.revoked,
                    readBounded: row.readBounded,
                    current: row.current,
                    admittedAfterRevocation: row.admittedAfterRevocation,
                    revokedByAuthority: row.revokedByAuthority,
                    withdrawable: row.withdrawable,
                    isThisDevice: row.deviceId == me
                )
            }
            holdsIdentityKey = await bridge.holdsIdentityKey()
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Follow the change feed. A device certificate applying is a write to the
    /// vault, so the ordinary batch is enough; nothing here reads `touched`,
    /// for the reason every other model gives.
    func follow() async {
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    /// Revoke `row`, and keep what the revocation could **not** do.
    ///
    /// The disclosure is the point of this method. `Command::RevokeDevice`
    /// rotates every Stream key it can and returns the ids of the rows it
    /// could not — a `stream_id` column that is not 16 bytes names no stream to
    /// mint an epoch for — and a client that printed "revoked" over a non-empty
    /// list would be telling a user their stolen laptop had been cut off from
    /// streams it can still read. Failing the whole revocation instead would be
    /// worse: the device that is gone is the entire scenario.
    func revoke(_ row: DeviceRow, reason: DeviceRevokeReason) async {
        busy = true
        defer { busy = false }
        do {
            let outcome = try await bridge.submit(
                .revokeDevice(deviceId: row.id, reason: reason)
            )
            let pending = (try? await bridge.relayRevocationPending(row.id)) ?? true
            lastRevocation = Revocation(
                nickname: row.nickname,
                outcome: outcome,
                relayPending: pending
            )
            errorMessage = nil
            await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func dismissRevocation() {
        lastRevocation = nil
    }

    /// Withdraw this device's own removal of `row` (ADR-0056), and keep what
    /// the withdrawal did.
    ///
    /// Not recorded for undo: its inverse is a new removal, which the user
    /// makes deliberately. A refusal is shown and nothing is reported as
    /// withdrawn; the feature gate's refusal (§7) has no override, so it says
    /// what to update rather than offering to go ahead.
    func withdraw(_ row: DeviceRow) async {
        busy = true
        defer { busy = false }
        do {
            let outcome = try await bridge.submit(.withdrawRevocation(deviceId: row.id))
            lastWithdrawal = Withdrawal(nickname: row.nickname, outcome: outcome)
            errorMessage = nil
            await refresh()
        } catch {
            lastWithdrawal = nil
            errorMessage = Self.withdrawalRefusal(error, rows: rows)
        }
    }

    /// The features whose gate can refuse a withdrawal: `core.revoke_withdraw`
    /// for a device's own removal, `core.revoke_withdraw_authority` for one
    /// the account identity made, which the identity key's holder may
    /// withdraw.
    nonisolated static let withdrawalFeatures: Set<String> = [
        "core.revoke_withdraw", "core.revoke_withdraw_authority"
    ]

    /// What to show for a withdrawal the core refused.
    ///
    /// The feature gate's refusal (ADR-0056 §7) crosses the seam as
    /// `BindingError.FeatureUnsupportedByDevices`, and `featureRefusal` reads
    /// the feature and the devices to update out of its message. The user is
    /// told how many, then each on its own line by the name `rows` gives it
    /// beside its id, or by its id alone where the list does not hold it, as
    /// `sunrise device withdraw` lists them. Anything else falls back to the core's own words: never to a
    /// claim that the removal was withdrawn.
    nonisolated static func withdrawalRefusal(_ error: Error, rows: [DeviceRow]) -> String {
        guard case let .FeatureUnsupportedByDevices(message)? = error as? BindingError,
              let refusal = featureRefusal(message: message),
              withdrawalFeatures.contains(refusal.feature)
        else { return error.localizedDescription }
        // The id as the row shows it, so the line can be matched to a row.
        let names = refusal.devices.map { id in
            let shown = String(id.prefix(16))
            return rows.first { $0.deviceID == id }.map { "\($0.nickname) (\(shown))" } ?? shown
        }
        return ([L10n.Devices.withdrawAwaitsUpdate(count: refusal.devices.count)] + names)
            .joined(separator: "\n")
    }

    func dismissWithdrawal() {
        lastWithdrawal = nil
    }

    func dismissError() {
        errorMessage = nil
    }
}
