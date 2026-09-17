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
        let revoked: Bool
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
        /// Whether this row is the device the user is holding.
        let isThisDevice: Bool
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
        }
    }

    private(set) var rows: [DeviceRow] = []
    /// Whether this vault holds the account identity key.
    private(set) var holdsIdentityKey = false
    /// The last revocation's disclosure, until the user dismisses it.
    private(set) var lastRevocation: Revocation?
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
                errorMessage = "The core answered a device list with something else."
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
                    current: row.current,
                    admittedAfterRevocation: row.admittedAfterRevocation,
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

    func dismissError() {
        errorMessage = nil
    }
}
