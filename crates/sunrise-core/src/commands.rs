//! Command type submitted via [`crate::Core::submit`].

use crate::control_op::RevokeReason;
use serde::{Deserialize, Serialize};
use sunrise_domain::{
    AttachmentDraft, BlockDraft, BlockPatch, ContextDraft, ContextPatch, Energy,
    InterruptionReason, ReviewSnapshotDraft, RoutineDraft, RoutinePatch, ScheduleConstraint,
    SessionLength, StreamDraft, StreamPatch, TaskDraft, TaskPatch, TaskState,
};
use sunrise_id::EntityRef;

/// Mutating commands. v1 covers Tasks and Streams; deeper entity types
/// expand the surface in follow-up phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Create a Task in a Stream (Inbox if `stream_id` is `None`).
    CreateTask(TaskDraft),
    /// Mutate an existing Task.
    UpdateTask {
        /// Target task.
        id: EntityRef,
        /// Patch.
        patch: TaskPatch,
    },
    /// Set Task state to `Done`.
    CompleteTask(EntityRef),
    /// Defer a Task by setting a new `scheduled_at` and bumping
    /// `deferred_count`.
    DeferTask {
        /// Target task.
        id: EntityRef,
        /// New `scheduled_at`.
        to_ms: u64,
    },
    /// Soft-delete a Task.
    DeleteTask(EntityRef),
    /// Move a Task between Streams.
    PromoteToStream {
        /// Task to move.
        id: EntityRef,
        /// New owning stream.
        stream: EntityRef,
    },
    /// Create a Stream.
    CreateStream(StreamDraft),
    /// Mutate a Stream.
    UpdateStream {
        /// Target stream.
        id: EntityRef,
        /// Patch.
        patch: StreamPatch,
    },
    /// Soft-delete a Stream.
    DeleteStream(EntityRef),
    /// Create a Context (cross-cutting tag). Rejected if a live Context
    /// already carries the same name, compared case-insensitively.
    CreateContext(ContextDraft),
    /// Mutate a Context (rename, re-describe, archive/unarchive).
    ///
    /// Archiving is *not* deletion: per
    /// `docs/02-domain/contexts-and-tags.md` an archived Context stays on the
    /// Tasks that carry it and only drops out of pickers and `@name` capture
    /// resolution.
    UpdateContext {
        /// Target context.
        id: EntityRef,
        /// Patch.
        patch: ContextPatch,
    },
    /// Soft-delete a Context, **removing it from every Task that carries it**
    /// in the same transaction (spec: "Deleting a Context removes it from all
    /// Tasks").
    DeleteContext(EntityRef),
    /// Create a Routine and materialize its near-horizon occurrences.
    CreateRoutine(RoutineDraft),
    /// Mutate a Routine. An rrule/timezone/anchor change regenerates future
    /// not-yet-started routine tasks.
    UpdateRoutine {
        /// Target routine.
        id: EntityRef,
        /// Patch.
        patch: RoutinePatch,
    },
    /// Soft-delete a Routine (tombstone). Stops generation; existing tasks
    /// remain.
    DeleteRoutine(EntityRef),
    /// Skip a single occurrence of a Routine by its occurrence key
    /// (`YYYY-MM-DDTHH:MM` in the routine's tz).
    SkipRoutineOccurrence {
        /// Target routine.
        id: EntityRef,
        /// Occurrence key to skip.
        occurrence_key: String,
    },
    /// Create a time Block (`docs/02-domain/time-blocks.md`).
    ///
    /// A Block given no title of its own and exactly one Task takes that
    /// Task's title as a **shadow copy** — the value as it is now, not a live
    /// binding. `BlockDraft::title_track_task` opts into live tracking instead.
    CreateBlock(BlockDraft),
    /// Create **or update** the Block that one external calendar item maps
    /// onto, keyed by `(source, uid)` instead of by a freshly minted id.
    ///
    /// This is the write half of `docs/09-integrations/icalendar.md` §Import:
    /// "the same UID from the same source on a subsequent import is treated as
    /// an update". The id is derived — not random — by
    /// [`sunrise_domain::imported_block_id`], so a second import of the same
    /// file lands on the Block the first one made, and two devices importing
    /// the same file converge instead of ending up with two copies of every
    /// event. See that function's module docs for why the pair lives in the id
    /// rather than in an `external_id` column.
    ///
    /// Not the same command as [`Command::CreateBlock`], and deliberately so:
    /// a create mints an identity and this one *adopts* one. Giving
    /// `CreateBlock` an optional id would let any caller overwrite an
    /// arbitrary Block through a command whose name says it makes a new one.
    ///
    /// What an update preserves, because the importer does not own it:
    ///
    /// * `created_at` — the Block was created when it was first imported.
    /// * Task bindings — a user who bound a task to an imported Block keeps
    ///   the binding; the draft's tasks are unioned in, never substituted.
    ///
    /// What it overwrites: the times and the title, which are the imported
    /// calendar's to state (`docs/09-integrations/icalendar.md`: "Imported
    /// entities are read-only").
    ///
    /// Reaches [`crate::DomainEvent`] as `Updated` in both cases. A change
    /// notification is a prompt to re-read (see [`crate::DomainEvent`]), and
    /// which of the two it was is not something a listener can act on
    /// differently.
    ImportBlock {
        /// Import source id. `("ics")` for a one-shot file import; a
        /// subscription would name itself.
        source: String,
        /// The external item's `UID`.
        uid: String,
        /// The Block to write.
        draft: BlockDraft,
    },
    /// Mutate a Block: move it, retitle it, or change title tracking.
    UpdateBlock {
        /// Target block.
        id: EntityRef,
        /// Patch.
        patch: BlockPatch,
    },
    /// Soft-delete a Block. Bound Tasks are untouched — a Block is a plan for
    /// when to do the work, never the work itself.
    DeleteBlock(EntityRef),
    /// Bind a Task to a Block.
    ///
    /// Maintains the spec's symmetry ("Bound Task's `blocks` field updates
    /// symmetrically") with **one** op rather than two: the binding lives in
    /// the `block_tasks` index and `Task.blocks` is derived from it on read,
    /// so a concurrent edit of the Task on another device cannot lose the
    /// binding to entity-level LWW.
    BindTask {
        /// Target block.
        block: EntityRef,
        /// Task to bind.
        task: EntityRef,
    },
    /// Unbind a Task from a Block. The Block survives with no Tasks bound:
    /// an empty Block is a legitimate calendar entry.
    UnbindTask {
        /// Target block.
        block: EntityRef,
        /// Task to unbind.
        task: EntityRef,
    },
    /// Record an Attachment against a Task
    /// (`docs/02-domain/attachments.md`).
    ///
    /// The bytes are uploaded first, sealed under a per-blob key the client
    /// generated: the file has to be encrypted to be uploadable at all, so the
    /// core cannot mint that key after the fact. This command writes the
    /// metadata op that makes the blob findable, and is the only thing about
    /// an attachment that ever reaches the op log.
    AttachFile(AttachmentDraft),
    /// Tombstone Attachment metadata.
    ///
    /// Step 1 of the spec's §Deletion, and all of it that belongs in the core:
    /// reclaiming the blob needs the device-cursor quorum and the 30-day grace
    /// period, which are the relay's job.
    DetachFile(EntityRef),
    /// Run materialization for every live Routine using `now_ms` as the clock.
    /// Emitted by `Core::open` and the periodic core timer.
    MaterializeRoutines {
        /// Wall clock (ms since epoch) from the injected clock.
        now_ms: u64,
    },
    /// Revoke a device and rotate every Stream key it could read.
    ///
    /// Replaces `TrustDevice`, which had no inverse: trust was a local upsert
    /// of any self-signed cert, so there was nothing to withdraw and nothing
    /// that would converge if you did. A device now becomes known by
    /// publishing an identity-signed cert as an op, and stops being known the
    /// same way.
    ///
    /// Emits a `device_revoke` op into the vault-meta stream and, in the same
    /// transaction, a fresh epoch plus `key_envelope` ops for every stream in
    /// the rotation set — the vault-meta stream and the Inbox included.
    ///
    /// # What this now does, and what it still does not
    ///
    /// The shape is unchanged and the meaning is not. It records the
    /// revocation, rotates every Stream key, **and rotates the account
    /// identity**, leaving the revoked device out of the new roster. That last
    /// part is ADR-0032 and it is what makes the first part stick: revocation
    /// names a device id, while the capability it needs to take away is
    /// `ID_S_priv`, which the departing device holds and no register touches.
    /// Before the identity rotated, the revoked device certified itself back in
    /// under a fresh id the register had never heard of
    /// ([#105](https://github.com/justin13888/Sunrise/issues/105)). Now the
    /// head has moved and it holds no share of the successor, so the fresh id
    /// is certified under an identity that is no longer the account and is a
    /// recipient of nothing.
    ///
    /// Still not cut off: **writes**. No replica refuses a revoked device's
    /// ops, because refusing at apply time does not converge — see
    /// `Engine::apply_remote` step 2. That is `#82`, behind `#80`.
    ///
    /// The user's **recovery code survives by default**: the successor is
    /// sealed to the outgoing `ID_D_pub` as well, so the existing code keeps
    /// working. The exception is the device that holds `ID_D_priv` — the
    /// account's creator. Revoking *that* device cannot carry the successor
    /// forward, because the carry share is sealed to the very key being
    /// excluded, and the user must be told their recovery code no longer opens
    /// anything (`core.identity.recovery_code_invalidated`).
    ///
    /// The cut recorded is the HLC of the op this emits, not a value the caller
    /// nominates; see [`crate::DeviceRevokePayload`] for why there is no
    /// `effective_at` to pass.
    RevokeDevice {
        /// The device to revoke.
        device_id: EntityRef,
        /// Why, for the device list to show later.
        reason: RevokeReason,
    },
    /// Replace the account identity, keeping every current device.
    ///
    /// The user-requested form of what a revocation does as a side effect
    /// (`docs/03-crypto/key-rotation.md` §Identity rotation): suspected
    /// recovery-code compromise, or suspected extraction of `ID_S_priv` itself.
    /// Every current, unrevoked device is in the new roster and receives a
    /// share, so nobody is excluded and nothing needs re-pairing.
    ///
    /// Stream keys are **not** rotated by this. An attacker holding the
    /// identity key can forge ops going forward; they cannot read content
    /// unless they also hold a Stream key. A user who wants both runs this and
    /// then rotates the streams.
    ///
    /// `keep_recovery_code` asks for the successor to be sealed to the outgoing
    /// `ID_D_pub`, so the user's existing BIP-39 code keeps working. Pass
    /// `false` when the reason for rotating is that the *recovery code* is the
    /// thing suspected — carrying the successor forward under a compromised key
    /// would hand the attacker the new identity, which is the one way this
    /// command can be worse than doing nothing.
    RotateIdentity {
        /// Seal the successor to the outgoing `ID_D_pub` so the existing
        /// recovery code still opens the account.
        keep_recovery_code: bool,
    },
    /// Mint a new epoch for one Stream and seal it to every current device.
    ///
    /// The narrow form of a revocation's rotation, for a key believed exposed
    /// with no device at fault.
    RotateStreamKey {
        /// The Stream to rotate.
        stream: EntityRef,
    },
    /// Open a focus session on a Task (ADR-0013's `start` op).
    ///
    /// Mints a fresh `fcs_` id and writes an **append-only** record; it does
    /// not touch the Task. Two devices starting a session concurrently mint
    /// different ids, so both survive and both count — that is the whole
    /// multi-device story, and it needs no OR-Set.
    ///
    /// Nothing about the running timer is persisted: the planned length is
    /// stored, the elapsed time is derived on read from the injected clock.
    StartFocus(FocusStartDraft),
    /// Close a focus session (ADR-0013's `end` op) — a *separate* record
    /// addressed to the same session id, never an edit of the start.
    ///
    /// Rejected if the session is unknown or has already ended: a session is
    /// immutable once closed.
    EndFocus {
        /// Session to close.
        session: EntityRef,
        /// Focused time to freeze. `None` freezes the derived elapsed time
        /// (`now - started_at`), which is what a session with no pauses ran
        /// for; a client that tracked pauses passes the smaller real figure.
        actual_focused_ms: Option<u64>,
        /// Whether the Task was completed in this session.
        ///
        /// `true` **auto-completes the Task** if it is still open (issue #10).
        /// This is not an inference: the focus screen's three actions are
        /// complete, defer and capture-aside, so the flag is the user saying
        /// they finished it, and the register simply never moved before.
        ///
        /// The session log still does not become a second writer of task
        /// state. The completion is derived once, on this device, and emitted
        /// as an ordinary `task.update` op that merges through the same
        /// entity-level LWW path as a hand-edit; a replica applying the
        /// `focus.end` op derives nothing. An already-`done` or `cancelled`
        /// Task is left alone.
        completed_task: bool,
    },
    /// Log one interruption against a running session
    /// (`docs/08-features/focus-mode.md` §Interruption capture).
    ///
    /// Grow-only: the `(session, at_ms, reason)` triple is the key, so
    /// re-delivery is idempotent and two devices' interruptions both survive.
    LogInterruption {
        /// Session interrupted.
        session: EntityRef,
        /// One-tap reason.
        reason: InterruptionReason,
    },
    /// Record that a weekly review was completed
    /// (`docs/08-features/reviews-and-stats.md` §Weekly review step 5).
    ///
    /// Mints a fresh `rvw_` id and writes an **append-only** record. This is
    /// the one fact about a review that is not recomputable from the op log —
    /// the lists and counts can be re-derived for any past week, but "a human
    /// sat down and reviewed week W" cannot. Two devices each finishing a
    /// review of the same week mint different ids, so both survive.
    SaveReviewSnapshot(ReviewSnapshotDraft),
}

/// Draft for [`Command::StartFocus`]. The core fills the session id, the
/// owning Stream, the start time, the planned length and the chunk marker —
/// all of which are derived, not caller-supplied, so two clients starting the
/// same kind of session record the same shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusStartDraft {
    /// Task to focus on.
    pub task_id: EntityRef,
    /// Work or break.
    pub kind: sunrise_domain::FocusKind,
    /// How long this session should run.
    pub length: SessionLength,
    /// The session's declared **energy budget** — what the user has in the
    /// tank. Defaults to the Task's own `energy` facet when `None`.
    pub energy: Option<Energy>,
}

/// Result of a command, returned synchronously to the caller after the
/// in-process apply step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// Stable id of the entity touched.
    pub entity: EntityRef,
    /// New state if applicable (e.g., for Task transitions).
    pub state: Option<TaskState>,
    /// Op id assigned by the core.
    pub op_id: [u8; 16],
    /// Sequence number assigned by the core.
    pub seq: u64,
    /// `soft` scheduling constraints the accepted `scheduled_at` violates.
    ///
    /// A `soft` violation never blocks the write (a `hard` one is rejected
    /// outright with [`sunrise_domain::ValidationError::HardScheduleConstraint`]),
    /// but it must not vanish silently either: per
    /// `docs/02-domain/scheduling-constraints.md` it demotes the item in
    /// planning views and the UI surfaces *which* window was missed. Empty for
    /// every command that does not schedule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub soft_violations: Vec<ScheduleConstraint>,
    /// Streams a `RevokeDevice` could **not** rotate, as lowercase hex.
    ///
    /// Empty for every command but `RevokeDevice`, and for almost every one of
    /// those. A non-empty value means the revocation is **incomplete**: the
    /// named rows carry a `stream_id` that is not 16 bytes, so there was no
    /// stream to mint a new epoch for, and the revoked device still holds the
    /// last key it was given for whatever those rows refer to.
    ///
    /// Reported rather than raised for the same reason the relay half of a
    /// revocation is queued rather than awaited (issue #160,
    /// [`Core::relay_revocation_pending`](crate::Core::relay_revocation_pending)):
    /// the device being revoked is often the one that is gone, so failing the
    /// whole operation is the worse answer — but a caller that printed
    /// "revoked" over this would be telling a user their stolen laptop was cut
    /// off when it was not. Same rule as `soft_violations` above: it does not
    /// block the write and it must not vanish.
    ///
    /// Hex rather than ids because these values are exactly the ones that are
    /// not ids; see `Keychain::rotation_set`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrotated_streams: Vec<String>,
    /// The fold **discarded** this `RevokeDevice`'s own op, so the account
    /// records no revocation of the target.
    ///
    /// `false` for every command but `RevokeDevice`, and for almost every one
    /// of those. `true` means this device's standing is the problem rather
    /// than the target's: `device_revocations` is a fold over every
    /// `device_revoke` op, and a row whose sender the ledger revokes anywhere
    /// is stored and skipped (ADR-0041). The op is kept and the judgement is
    /// re-taken whenever another revocation lands, so this is "not believed
    /// yet" and not "thrown away".
    ///
    /// What the caller must not do is print "revoked". Nothing was cut: the
    /// target stays current on every replica, it keeps receiving new epochs,
    /// and the relay's half is deliberately not queued for it — the two
    /// halves of a revocation are different guarantees and a user is entitled
    /// to know which they have (issue #160). Same rule as
    /// `unrotated_streams` above, at the other end of the scale: that one says
    /// the revocation was incomplete, this one says there was none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revocation_gated: bool,
    /// Devices the account **no longer calls revoked**, while still giving
    /// them no keys. Lowercase hex ids, empty for every command but
    /// `RevokeDevice`.
    ///
    /// `device_revocations` is a fold (ADR-0041), so applying a
    /// `device_revoke` can *remove* a row: a revocation stops being believed
    /// once the ledger shows its own author had been revoked first. The device
    /// then reads `current` on every device list again — and it is the only
    /// outcome of that design a user could be surprised by, which is why
    /// `core.device.revocation_unwound` is logged. This is the same fact where
    /// a user can see it, on the rule `docs/10-cross-cutting/log-events.md`
    /// states for `revoke_incomplete`: a signal that changes what the account
    /// believes comes back on the result and is not left in an operator's
    /// NDJSON.
    ///
    /// **It is the standing set, not this command's delta**, and deliberately:
    /// the hazard is not that one op unwound something, it is that the account
    /// is in a state it does not mean, and a user who was not looking the
    /// first time is entitled to be told again. Each id here is read-bounded
    /// (`DeviceRow::read_bounded`) and not revoked, so it receives nothing and
    /// looks like an ordinary member. The remedy is this family's usual one:
    /// revoke it again from a device the account still trusts.
    ///
    /// Same disclosure rule as `unrotated_streams` and `revocation_gated`: a
    /// caller must not print a bare "revoked" over a non-empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revocation_unwound: Vec<String>,
}

impl CommandResult {
    /// Result for a command that schedules nothing (the common case).
    #[must_use]
    pub const fn new(
        entity: EntityRef,
        state: Option<TaskState>,
        op_id: [u8; 16],
        seq: u64,
    ) -> Self {
        Self {
            entity,
            state,
            op_id,
            seq,
            soft_violations: Vec::new(),
            unrotated_streams: Vec::new(),
            revocation_gated: false,
            revocation_unwound: Vec::new(),
        }
    }

    /// Attach the streams a revocation could not rotate.
    #[must_use]
    pub fn with_unrotated_streams(mut self, v: Vec<String>) -> Self {
        self.unrotated_streams = v;
        self
    }

    /// Record that the fold discarded this revocation's own op.
    #[must_use]
    pub const fn with_revocation_gated(mut self, v: bool) -> Self {
        self.revocation_gated = v;
        self
    }

    /// Attach the devices the account no longer calls revoked but still gives
    /// no keys.
    #[must_use]
    pub fn with_revocation_unwound(mut self, v: Vec<String>) -> Self {
        self.revocation_unwound = v;
        self
    }

    /// Attach the `soft` constraint violations observed while scheduling.
    #[must_use]
    pub fn with_soft_violations(mut self, v: Vec<ScheduleConstraint>) -> Self {
        self.soft_violations = v;
        self
    }
}
