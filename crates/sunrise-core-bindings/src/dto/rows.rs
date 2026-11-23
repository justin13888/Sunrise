//! List rows and status: the read models the lists and status views render.

use sunrise_core::{
    queries::{ContextRow, DeviceRow, StreamRow},
    CommandResult,
};
use sunrise_domain::{StreamColor, TaskState};
use sunrise_id::{EntityKind, EntityRef};

use super::{hex16, Constraint};

/// One row of the stream list; the Inbox comes first and is synthetic: the
/// Inbox view under the legacy Inbox id ([`crate::vocab::inbox_stream_id`]), not a
/// stream (ADR-0046 §3).
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamListRow {
    /// Stream id.
    pub id: EntityRef,
    /// Display name.
    pub name: String,
    /// Colour.
    pub color: StreamColor,
    /// Open tasks (todo / in-progress, not deleted).
    pub open_task_count: u64,
    /// Archived.
    pub archived: bool,
    /// Paused.
    pub paused: bool,
    /// The fractional index this row is sorted on; `""` for the Inbox and for
    /// a stream nothing has ever ordered.
    ///
    /// The rows arrive in this order already. It is carried anyway because a
    /// client that lets somebody *drag* a row needs the keys either side of
    /// where it landed to compute the new one — see
    /// [`crate::vocab::stream_sort_key_between`].
    pub sort_order: String,
}

impl StreamListRow {
    /// The synthetic Inbox row, counting the `open` tasks of the Inbox view.
    pub(crate) fn inbox(open: u64) -> Self {
        Self {
            id: sunrise_domain::inbox_stream_ref(),
            name: "Inbox".to_owned(),
            color: StreamColor::Slate,
            open_task_count: open,
            archived: false,
            paused: false,
            // Not a stream, and not orderable: "no position".
            sort_order: String::new(),
        }
    }
}

impl From<&StreamRow> for StreamListRow {
    fn from(s: &StreamRow) -> Self {
        let StreamRow {
            id,
            name,
            color,
            open_task_count,
            archived,
            paused,
            sort_order,
        } = s;
        Self {
            id: *id,
            name: name.clone(),
            color: color.clone(),
            open_task_count: *open_task_count,
            archived: *archived,
            paused: *paused,
            sort_order: sort_order.clone(),
        }
    }
}

/// One row of the context list.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ContextListRow {
    /// Context id.
    pub id: EntityRef,
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Archived.
    pub archived: bool,
    /// Live tasks carrying it.
    pub task_count: u64,
}

impl From<&ContextRow> for ContextListRow {
    fn from(c: &ContextRow) -> Self {
        let ContextRow {
            id,
            name,
            description,
            archived,
            task_count,
        } = c;
        Self {
            id: *id,
            name: name.clone(),
            description: description.clone(),
            archived: *archived,
            task_count: *task_count,
        }
    }
}

/// One paired device.
///
/// See `sunrise_core::DeviceRow` for why the four flags are orthogonal facts
/// rather than positions of one state: every pair is reachable in both
/// combinations. The fifth, `revoked_by_authority`, refines `revoked`. No item-level `allow` for `struct_excessive_bools` here —
/// `lib.rs` already allows it crate-wide, so a second one would be dead.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceListRow {
    /// Device id, lowercase hex.
    ///
    /// The form a user reads and a CLI resolves a prefix against. **Not** the
    /// form `CoreCommand::RevokeDevice` takes — see [`Self::device_ref`], which
    /// is here because a client that could only list devices in hex could not
    /// revoke one without reimplementing Crockford base32 in Swift.
    pub device_id: String,
    /// The same device as an `EntityRef` string, `dev_…`.
    ///
    /// What `CoreCommand::RevokeDevice` wants, so a device list can act on a
    /// row it is showing. Both forms are carried rather than one converted,
    /// because the two are read by different audiences: the hex is what
    /// `sunrise devices` prints and what a user retypes a prefix of, and this
    /// is an opaque token a client passes straight back.
    pub device_ref: String,
    /// Human-readable nickname.
    pub nickname: String,
    /// Platform string.
    pub platform: String,
    /// The account **currently** calls this device revoked.
    ///
    /// Derived and therefore reversible: `device_revocations` is a fold over
    /// the revocation ledger (ADR-0041), so this goes back to `false` when the
    /// ledger shows a revocation's own author had been revoked first. Pair it
    /// with `readBounded`, which does not move back.
    pub revoked: bool,
    /// This device is **read-bounded**: some revocation this vault believed
    /// named it, so it receives no key this vault mints, ever.
    ///
    /// The durable half of revocation, and worth its own line on a device list
    /// because it does not always agree with `revoked`.
    /// `readBounded: true, revoked: false` is the unwind — the account stopped
    /// believing the revocation and calls the device current again, while it
    /// goes on receiving nothing. Word it as a state and give the remedy
    /// ("removed earlier; the account no longer records that, but it still
    /// receives nothing — remove it again from a device you still trust"), not
    /// as an accusation: the ordinary cause is retiring two devices months
    /// apart.
    ///
    /// `readBounded: false, revoked: true` cannot happen.
    pub read_bounded: bool,
    /// Certified under the account identity **in force**, and so a member.
    ///
    /// Not the negation of `revoked`, and a UI that renders it as one is
    /// wrong: a device that left and certified itself back in under a fresh id
    /// is `revoked: false, current: false`, because the register names ids and
    /// the fresh one is not in it (ADR-0032, #105). Present it as "not active
    /// on this account" rather than as an accusation — an honest device that
    /// has not yet applied a rotation looks the same for a moment.
    pub current: bool,
    /// This device id was first seen **after** this vault had already recorded
    /// a revocation.
    ///
    /// `core.device.admitted_after_revocation` made durable, so it reaches the
    /// device list instead of only an operator's NDJSON
    /// ([#144](https://github.com/justin13888/Sunrise/issues/144)). Also not an
    /// accusation: the ordinary cause is pairing a new device in an account
    /// that revoked something earlier. The case it exists for is a revoked
    /// *creator* certifying a fresh id after a revocation that could not rotate
    /// the identity, which reads `revoked: false, current: true` and is
    /// otherwise indistinguishable from any other member. Word it as "joined
    /// after a device was removed".
    pub admitted_after_revocation: bool,
    /// This device is revoked **by the account identity**: an authority row
    /// names it at the register's last fold (ADR-0058 §7).
    ///
    /// The one revocation nothing can unwind. No op from any device takes it
    /// back while the identity that signed it is in force, and a rotation
    /// carries it forward. Every other revocation is ordinary: a later op can
    /// take it back off the list (ADR-0041), which is what `readBounded`
    /// without `revoked` looks like afterwards. Word the two differently, so a
    /// user can tell which kind removed a device.
    ///
    /// A fact about the register, not about the row that won it: a later
    /// ordinary revocation can win the row while this stays true.
    /// `revokedByAuthority: true, revoked: false` cannot happen. Last and
    /// defaulted, so the seam changes additively.
    #[uniffi(default = false)]
    pub revoked_by_authority: bool,
    /// **This** device holds a live revocation of this row's device that it
    /// may withdraw with `CoreCommand::WithdrawRevocation` (ADR-0056 §6).
    ///
    /// The only rows on which to offer "Withdraw revocation". A revocation
    /// another device made is that device's to withdraw. One this device
    /// signed as the account identity counts only while this device holds the
    /// identity in force, which signs the withdrawal (ADR-0058 §6). Last and
    /// defaulted, so the seam changes additively.
    #[uniffi(default = false)]
    pub withdrawable: bool,
}

impl From<&DeviceRow> for DeviceListRow {
    fn from(d: &DeviceRow) -> Self {
        let DeviceRow {
            device_id,
            nickname,
            platform,
            revoked,
            read_bounded,
            current,
            admitted_after_revocation,
            revoked_by_authority,
            withdrawable,
        } = d;
        Self {
            device_id: hex16(device_id),
            device_ref: EntityRef::new(EntityKind::Device, *device_id).to_str(),
            nickname: nickname.clone(),
            platform: platform.clone(),
            revoked: *revoked,
            read_bounded: *read_bounded,
            current: *current,
            admitted_after_revocation: *admitted_after_revocation,
            revoked_by_authority: *revoked_by_authority,
            withdrawable: *withdrawable,
        }
    }
}

/// A snapshot of sync health.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncSnapshot {
    /// Connection state.
    pub state: sunrise_sync::SyncState,
    /// Ops waiting to be sent.
    pub outbox_pending: u32,
    /// Trusted peer devices.
    pub peer_devices: u32,
    /// Last successful sync (epoch ms).
    pub last_sync_ms: Option<u64>,
}

impl From<&sunrise_core::SyncStatus> for SyncSnapshot {
    fn from(s: &sunrise_core::SyncStatus) -> Self {
        let sunrise_core::SyncStatus {
            state,
            outbox_pending,
            peer_devices,
            last_sync_ms,
            // Not carried: a field on this record would change its generated
            // initializer, and the seam changes additively only. A restore
            // reads the replay's progress through `ReplayListener` instead
            // (`recovery.rs`).
            replay_applied: _,
            replay_remaining: _,
        } = s;
        Self {
            state: *state,
            outbox_pending: *outbox_pending,
            peer_devices: *peer_devices,
            last_sync_ms: *last_sync_ms,
        }
    }
}

/// How many ops are parked under one reason (ADR-0045 §4, "Visibility").
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ParkedReasonCount {
    /// The stored reason, e.g. `unknown_kind` (a newer device wrote a kind
    /// this build does not know) or `replay_refused`. A string, not an enum:
    /// a vault opened by an older build than the one that parked into it can
    /// hold a reason this build has no name for, and the count still counts.
    pub reason: String,
    /// How many ops are parked under it. Never zero.
    pub count: u64,
}

impl From<&sunrise_core::ParkedOpCount> for ParkedReasonCount {
    fn from(c: &sunrise_core::ParkedOpCount) -> Self {
        let sunrise_core::ParkedOpCount { reason, count } = c;
        Self {
            reason: reason.clone(),
            count: *count,
        }
    }
}

/// What a command did.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CommandOutcome {
    /// The entity the command touched.
    pub entity: EntityRef,
    /// New task state, where the command set one.
    pub state: Option<TaskState>,
    /// Op id, lowercase hex.
    pub op_id: String,
    /// Sequence number the core assigned.
    pub seq: u64,
    /// `soft` scheduling constraints the accepted time violates.
    ///
    /// A soft violation never blocks the write (a hard one is rejected
    /// outright), but it must not vanish silently either: it demotes the item
    /// in planning views, and the client is expected to say *which* window was
    /// missed. Empty for every command that schedules nothing.
    pub soft_violations: Vec<Constraint>,
    /// Streams a `RevokeDevice` could **not** rotate, as lowercase hex.
    ///
    /// Non-empty means the revocation is incomplete and the revoked device can
    /// still read whatever these rows refer to. A client must not print
    /// "revoked" over this: it is the same disclosure the relay half already
    /// gets through `relayRevocationPending` (#160), one level down. Empty for
    /// every other command and for almost every revocation.
    pub unrotated_streams: Vec<String>,
    /// The fold **discarded** this `revokeDevice`'s own op, so nothing was
    /// revoked.
    ///
    /// `true` means this device's own standing is the problem: the account has
    /// revoked it, and a revoked device's revocations of third parties are
    /// stored and skipped on every replica (ADR-0041). The target stays
    /// current everywhere, it keeps receiving new keys, and the relay's half
    /// is deliberately not queued for it. A client must not print "removed"
    /// over this — it is the same disclosure rule as `unrotatedStreams`, at
    /// the other end of the scale: that one says the removal was incomplete,
    /// this one says there was none. `false` for every other command.
    pub revocation_gated: bool,
    /// Devices the account **no longer calls revoked**, while still giving
    /// them no keys. Lowercase hex ids; empty for every other command.
    ///
    /// The register is a fold (ADR-0041), so applying a revocation can remove
    /// an older one whose author turns out to have been revoked first. Those
    /// devices go back to reading `revoked: false` on the device list while
    /// `readBounded` stays true, so they receive nothing — a state the account
    /// does not really mean and nobody asked for. A client must say so and
    /// name the remedy: revoke each of them again from a device the account
    /// still trusts. Same disclosure rule as `unrotatedStreams`.
    ///
    /// The standing set, not this command's delta: a user who missed it last
    /// time is entitled to be told again.
    pub revocation_unwound: Vec<String>,
    /// The orderings of the task's three times the accepted write leaves
    /// broken (ADR-0047 §1): `target_after_hard_due`,
    /// `planned_after_hard_due`. A warning to show, never a refusal; empty
    /// for every command that writes no task.
    #[uniffi(default = [])]
    pub ordering_warnings: Vec<String>,
    /// This `revokeDevice` was signed by the account identity, so it is an
    /// **authority revocation** (ADR-0058 §7): no op from any device can
    /// unwind it while that identity is in force.
    ///
    /// `false` for every other command, and for a revocation from a device
    /// that does not hold the account's signing key, which is every device
    /// admitted by pairing. That one is ordinary: it does everything ADR-0041
    /// gives it and a later op can still unwind it, so a client must not
    /// describe it as permanent.
    #[uniffi(default = false)]
    pub revoked_by_authority: bool,
    /// The devices whose build must be updated before this device can revoke
    /// by authority, as lowercase hex ids. Empty for every other command, and
    /// for every revocation this device either signed or could never sign.
    ///
    /// Non-empty means this device holds the account's signing key and the
    /// revocation went out ordinary only because these devices, which are not
    /// revoked, do not advertise `core.revoke_authority`. The revocation was
    /// not refused or delayed; a client names these devices so the user knows
    /// what stands between them and one nothing can unwind.
    #[uniffi(default = [])]
    pub authority_awaits_update: Vec<String>,
    /// What a `withdrawRevocation` did; `None` for every other command.
    /// Last and defaulted, so the seam changes additively.
    #[uniffi(default = None)]
    pub withdrawal: Option<WithdrawalReport>,
}

/// Whether a withdrawal reinstated its device, and if not, why not
/// (ADR-0056 §6). A client must not say "reinstated" over the first three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum WithdrawalOutcomeKind {
    /// This device holds no live revocation of the device: another device
    /// made it, or this device made it as the account identity. Nothing was
    /// emitted. That device withdraws it, or the device is paired again.
    NotAuthor,
    /// Another device's revocation is still live, so the device stays
    /// revoked until that device withdraws too.
    StillClaimed,
    /// The revocation rotated the account identity away from the device, so
    /// it must be paired again.
    RepairRequired,
    /// The device is a current member again.
    Reinstated,
}

impl From<sunrise_core::WithdrawalOutcome> for WithdrawalOutcomeKind {
    fn from(o: sunrise_core::WithdrawalOutcome) -> Self {
        match o {
            sunrise_core::WithdrawalOutcome::NotAuthor => Self::NotAuthor,
            sunrise_core::WithdrawalOutcome::StillClaimed => Self::StillClaimed,
            sunrise_core::WithdrawalOutcome::RepairRequired => Self::RepairRequired,
            sunrise_core::WithdrawalOutcome::Reinstated => Self::Reinstated,
        }
    }
}

/// Where the relay's half of a withdrawn revocation stands for one device
/// (ADR-0056 §5). See `sunrise_core::RelayReinstatement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RelayReinstatementKind {
    /// Nothing is owed to the relay: no reinstatement was queued, the relay
    /// accepted it, or the device is revoked again.
    Clear,
    /// Queued for the next session; until then a relay that was told of the
    /// revocation still refuses the device.
    Pending,
    /// The relay refused it the last time it was asked: it recorded another
    /// device as the revoker. The device must be paired again, unless that
    /// revoker withdraws too; each session asks the relay again, and this
    /// becomes `Clear` once it accepts the device.
    Refused,
}

impl From<sunrise_core::RelayReinstatement> for RelayReinstatementKind {
    fn from(r: sunrise_core::RelayReinstatement) -> Self {
        match r {
            sunrise_core::RelayReinstatement::Clear => Self::Clear,
            sunrise_core::RelayReinstatement::Pending => Self::Pending,
            sunrise_core::RelayReinstatement::Refused => Self::Refused,
        }
    }
}

/// What `withdrawRevocation` did. See `sunrise_core::Withdrawal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct WithdrawalReport {
    /// Whether the device is back, and if not, why not.
    pub outcome: WithdrawalOutcomeKind,
    /// Whether this device released the device's read bound, so it is sent
    /// keys again.
    pub read_bound_released: bool,
    /// How many keys this device sent it, including the keys minted while it
    /// was revoked. A client says so: the device can read what was written
    /// while it was out.
    pub keys_resent: u32,
    /// Whether this device's unsent telling of the relay was cancelled.
    /// `false` means the relay may already refuse the device, and nothing yet
    /// tells it otherwise: say the device may need pairing again.
    pub relay_intent_cancelled: bool,
}

impl From<&sunrise_core::Withdrawal> for WithdrawalReport {
    fn from(w: &sunrise_core::Withdrawal) -> Self {
        let sunrise_core::Withdrawal {
            outcome,
            read_bound_released,
            keys_resent,
            relay_intent_cancelled,
        } = w;
        Self {
            outcome: (*outcome).into(),
            read_bound_released: *read_bound_released,
            keys_resent: *keys_resent,
            relay_intent_cancelled: *relay_intent_cancelled,
        }
    }
}

impl From<&CommandResult> for CommandOutcome {
    fn from(r: &CommandResult) -> Self {
        let CommandResult {
            entity,
            state,
            op_id,
            seq,
            soft_violations,
            ordering_warnings,
            unrotated_streams,
            revocation_gated,
            revocation_unwound,
            revoked_by_authority,
            authority_awaits_update,
            withdrawal,
            // What the core publishes on its change stream, which the seam
            // relays from there rather than from the result.
            events: _,
        } = r;
        Self {
            entity: *entity,
            state: state.clone(),
            op_id: hex16(op_id),
            seq: *seq,
            soft_violations: soft_violations.iter().map(Constraint::from).collect(),
            unrotated_streams: unrotated_streams.clone(),
            revocation_gated: *revocation_gated,
            revocation_unwound: revocation_unwound.clone(),
            ordering_warnings: ordering_warnings
                .iter()
                .map(|w| w.as_str().to_owned())
                .collect(),
            revoked_by_authority: *revoked_by_authority,
            authority_awaits_update: authority_awaits_update.clone(),
            withdrawal: withdrawal.as_ref().map(WithdrawalReport::from),
        }
    }
}
