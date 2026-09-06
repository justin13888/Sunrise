//! Command/query engine pipeline.
//!
//! Translates a [`Command`] into:
//! 1. a new op-id (ULID; high 48 bits = clock, low 80 bits = RNG),
//! 2. a CBOR-encoded inner-op blob (the canonical op record),
//! 3. an op log row (envelope = inner-op for now; the encrypted
//!    `OpEnvelope` wrapping happens once `sunrise-sync` actually emits
//!    on the wire — locally we store the plaintext inner-op as the
//!    envelope blob since SQLCipher already encrypts at rest),
//! 4. a materialized state mutation (insert/update of `tasks`/`streams`
//!    rows), and
//! 5. a side-effect-free `CommandResult` returned to the caller.
//!
//! All five happen inside one `BEGIN IMMEDIATE` transaction.
//!
//! Read queries (Today, Inbox, StreamTasks, EntityById) materialise
//! results from the same tables; they do not re-read the op log.
//!
//! The engine is parameterized over a [`crate::config::Clock`] and
//! [`crate::config::Rng`], so tests inject fakes and the engine remains
//! deterministic.

// Stylistic clippy lints relaxed while the engine surface is in active
// expansion (Phase 17 retighten). The substantive ones (correctness,
// integer truncation, etc.) remain enforced.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::single_match_else,
    clippy::field_reassign_with_default,
    clippy::bool_assert_comparison,
    clippy::needless_collect,
    clippy::manual_let_else,
    clippy::option_map_unit_fn,
    clippy::option_if_let_else,
    clippy::unnecessary_wraps,
    clippy::missing_const_for_fn,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::unused_self,
    clippy::unnecessary_cast
)]

use crate::commands::{Command, CommandResult, FocusStartDraft};
use crate::config::{Clock, HlcClock, Rng};
use crate::control_op::{DeviceRevokePayload, KeyEnvelopePayload, Recipient, RevokeReason};
use crate::events::DomainEvent;
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp, InnerOpError, OpEffect};
use crate::keychain::{EnvelopeRecipient, KeySource, Keychain};
use crate::queries::{
    ActionableTask, BlockRow, ContextRow, DeviceRow, FocusPlanRow, FocusSessionRow, Query,
    QueryResult, StreamRow,
};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, open_envelope_unverified, stream_key_id, verify_envelope, DeviceCert,
    StreamKey,
};
use sunrise_domain::sort_order;
use sunrise_domain::time::SunriseTime;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    activity_for_entities, activity_table, break_after, build_daily_review, build_end_of_day_plan,
    build_morning_summary, build_weekly_review, effective_state, focus_table, fold_activity,
    fold_focus_stats, fold_trends, imported_block_id, inbox_stream_ref,
    materialization_horizon_days, occurrence_key_at, occurrence_task_id, plan_reminders,
    plan_session, rank_focus_plan, routine_drift, streaks_table, trends_table, unblock_cascade,
    violations_by_severity, ActivityEvent, Attachment, AttachmentDraft, Block, BlockDraft,
    BlockPatch, Chunk, Context, ContextDraft, ContextPatch, DependencyGraph, Energy, ExportDataset,
    ExportFormat, FocusEnd, FocusKind, FocusSession, FocusStart, Interruption, InterruptionReason,
    NoteBody, OpPayload, OpRecord, PlanCandidate, ReminderCandidate, ReminderKind,
    ReminderSettings, ReviewSnapshot, ReviewSnapshotDraft, ReviewStream, ReviewWindow, Routine,
    RoutineCatchupPolicy, RoutineDraft, RoutineDrift, RoutinePatch, ScheduleConstraint,
    SessionLength, SessionRecord, StreakOutcome, StreakRow, Stream, StreamColor, StreamDraft,
    StreamPatch, StreamReviewCadence, Task, TaskDraft, TaskPatch, TaskState, TaskTemplate, Trends,
    ValidationError, WeekGrid, Weekday, WeeklyReview, WeeklyReviewInput, DEFAULT_DRIFT_THRESHOLD,
    DRIFT_WINDOW_WEEKS, INBOX_STREAM_BYTES, POMODORO_MS, TREND_WEEKS,
};
use sunrise_id::{EntityKind, EntityRef, Ulid};
use sunrise_storage::{Db, OpLog, Outbox};
use thiserror::Error;

/// Vault-meta op-log stream id: 16 zero bytes.
///
/// Routing: Stream lifecycle ops (create/update/delete), all routine ops, review
/// snapshots and the control op families are logged under this meta stream,
/// while task ops are logged under their owning Stream's id.
///
/// It is an ordinary member of the rotation set. Revoking a device mints a new
/// epoch here as it does for every other stream, so a revoked device stops
/// seeing Streams created after it was cut off — not only their tasks. Leaving
/// vault-meta on a fixed epoch would have left the metadata readable forever,
/// which is exactly the claim ADR-0025's credential story rests on.
///
/// The Inbox no longer shares this id: see
/// [`sunrise_domain::INBOX_STREAM_BYTES`].
pub(crate) const META_STREAM: [u8; 16] = [0u8; 16];

/// Upper bound on the trend window a caller may ask for.
///
/// The trend fold re-reads and decrypts op history, so an unbounded `weeks`
/// would turn one keypress into a full-log scan. Two years is far past the
/// point where a weekly chart is still readable.
const MAX_TREND_WEEKS: u32 = 104;

/// How many actionable tasks the Focus Planner scans before ranking.
///
/// The planner's ranking is energy-first, so it cannot be truncated in SQL
/// without risking dropping the best-matching pick — the scan has to be wide
/// enough that ranking sees every plausible candidate, and bounded so a huge
/// vault cannot turn one keypress into a full-table sort.
const FOCUS_PLAN_SCAN_CAP: u32 = 512;

/// How far above this replica's live epoch an absorbed Stream key may sit.
///
/// `epoch` is a plain field of the signed envelope, so a member chooses it
/// freely, and `current_epoch_tx` reads the live epoch as `MAX(epoch)`. Without
/// a bound a single `key_envelope` at `u32::MAX` does two things at once:
/// [`Keychain::mint_epoch`](crate::keychain::Keychain::mint_epoch) saturates,
/// so rotation can never advance past it again, and every op this device seals
/// from then on is sealed under a key only the sender holds — the victim goes
/// dark to its own account, permanently, from one op.
///
/// The window can be this tight because a legitimate epoch is *walked*, never
/// leapt. Each rotation raises one stream's epoch by exactly one and emits its
/// `key_envelope` sealed under the meta epoch current at the time; a replica
/// catching up absorbs the meta key for epoch n, drains the ops that key
/// releases, and only then sees the envelopes for n+1. So the gap this has to
/// tolerate is what can arrive out of order within one drain, not the number of
/// rotations an account has ever performed. Sixty-four is far past that and far
/// short of anything that would strand `mint_epoch`.
///
/// The residual is that a key genuinely more than 64 epochs ahead is refused
/// and not retried, so ops sealed under it stay unreadable on this replica. It
/// is logged (`core.key.epoch_refused`) rather than swallowed, because the only
/// ways to reach it are a hostile sender and a bug.
const MAX_EPOCH_LEAP: u32 = 64;

/// How many ops may sit in `deferred_ops` for one `(stream_id, epoch)`.
///
/// A deferred op is one that arrived before the key that opens it, which is a
/// real and ordinary race — but a *transient* one: the very next absorb of that
/// key releases the whole bucket. So the honest population of one bucket is
/// whatever a peer wrote in the gap between minting a key and its
/// `key_envelope` reaching here, which is a burst, not a backlog. 256 covers a
/// peer that wrote a full working session into the gap.
const DEFERRED_PER_EPOCH_CAP: i64 = 256;

/// How many ops may sit in `deferred_ops` across the whole vault.
///
/// A revocation rotates *every* stream at once, so several buckets can fill
/// legitimately at the same moment; the total is sized for that rather than for
/// one stream. 4096 is sixteen full buckets.
///
/// Both caps exist because `epoch` is attacker-chosen (see [`MAX_EPOCH_LEAP`])
/// and `defer_op` is reached *before* anything about the payload is checked: a
/// member could otherwise park bytes of its choosing on every peer in the
/// account, at any `(stream, epoch)` it liked, with no ceiling. Overflow evicts
/// the **oldest** rows, not the newest: a row that has waited longest is the
/// one whose key is least likely to still be in flight, and evicting newest
/// would let a flood freeze a bucket against every honest op behind it.
const DEFERRED_TOTAL_CAP: i64 = 4096;

/// How long a parked op is kept before a drain sweeps it away.
///
/// Nothing re-delivers the key for an op this old: the relay's op ring has long
/// since evicted the `key_envelope` that would have released it, and no replica
/// re-emits one on request. Keeping it is keeping ciphertext this device will
/// never read. Thirty days is generous against every offline window a person
/// actually has and still bounds a slow drip that never reaches either cap.
const DEFERRED_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// How far ahead of the op that declares it a revocation's cut may sit.
///
/// `effective_at_ms` is a plain field of the signed envelope, and since the cut
/// became a real time comparison it is also the whole of a revocation's
/// meaning. Unbounded, `u64::MAX` is a permanent immunity: the first
/// revocation to land sets the cut, [`Engine::apply_control_op`] takes the
/// earliest of the cuts it has seen, and no realistic HLC ever reaches it, so
/// every later genuine revocation converges onto a value that refuses nothing.
///
/// Seven days rather than something tighter because a forward-dated cut is a
/// documented feature, not an oddity: `docs/03-crypto/key-rotation.md` §Device
/// key rotation emits a `device_revoke` for the *old* device key at
/// `now + 24h`, which is the overlap window that keeps a rotating device
/// working while its new cert propagates. Seven days covers that with room and
/// still refuses anything whose purpose could only be immunity.
const REVOKE_CUT_AHEAD_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Engine error. Maps to `CoreError::Engine` at the public API.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Underlying SQLite/storage failure.
    #[error("storage: {0}")]
    Storage(#[from] sunrise_storage::DbError),
    /// SQL error during a transaction.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Domain validation rejected the input.
    #[error("validation: {0}")]
    Validation(#[from] sunrise_domain::ValidationError),
    /// Op-log error.
    #[error("op-log: {0}")]
    OpLog(#[from] sunrise_storage::OpLogError),
    /// CBOR encode/decode failure for an inner-op blob.
    #[error("cbor: {0}")]
    Cbor(String),
    /// Inner-op CBOR codec failure.
    #[error("inner-op: {0}")]
    InnerOp(#[from] InnerOpError),
    /// Target entity not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Invalid state transition or policy violation.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A remote op envelope was malformed, failed verification, or failed to
    /// decrypt (bad magic / signature / AEAD / non-canonical inner CBOR).
    ///
    /// Maps to [`sunrise_error::ErrorCode::SyncOpInvalid`] at the public API.
    #[error("remote op invalid: {0}")]
    RemoteOpInvalid(String),
    /// A remote op envelope names a `device_id` that is not present in the
    /// local `devices` table.
    ///
    /// Since ADR-0024 a device becomes known by publishing its identity-signed
    /// cert as a `DeviceCertPublish` op, not by a local `TrustDevice` command.
    ///
    /// Maps to [`sunrise_error::ErrorCode::SyncOpInvalid`] at the public API:
    /// the op decoded but failed a semantic (trust) check.
    #[error("remote op from unknown device")]
    UnknownDevice,
    /// A remote op was signed by a device this vault has revoked, with an HLC
    /// at or after the revocation's `effective_at_ms`.
    ///
    /// Distinct from [`Self::UnknownDevice`] on purpose: "I have never heard of
    /// you" and "I cut you off on Tuesday" are different facts, and the second
    /// one is permanent. Maps to
    /// [`sunrise_error::ErrorCode::AuthDeviceRevoked`].
    #[error("remote op from revoked device")]
    DeviceRevoked,
    /// Keychain failure while resolving or absorbing a Stream key.
    #[error("keychain: {0}")]
    Keychain(String),
}

/// One command-application pipeline. Stateless; holds references to the
/// injected clock + rng so tests can produce deterministic op-ids.
#[derive(Clone)]
pub struct Engine {
    clock: Arc<dyn Clock>,
    hlc: Arc<dyn HlcClock>,
    rng: Arc<dyn Rng>,
    keychain: Arc<Keychain>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("device_id", &hex_short(&self.keychain.device_id()))
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Construct. The keychain supplies the device id + signing key used to
    /// seal every op envelope.
    #[must_use]
    pub fn new(
        clock: Arc<dyn Clock>,
        hlc: Arc<dyn HlcClock>,
        rng: Arc<dyn Rng>,
        keychain: Arc<Keychain>,
    ) -> Self {
        Self {
            clock,
            hlc,
            rng,
            keychain,
        }
    }

    /// Construct with the causal clock derived from `clock`.
    ///
    /// The shape a caller wants when it has no reason to substitute the HLC
    /// independently of the wall clock: skew `clock` and the HLC skews with it.
    #[must_use]
    pub fn from_clock(clock: Arc<dyn Clock>, rng: Arc<dyn Rng>, keychain: Arc<Keychain>) -> Self {
        let hlc = Arc::new(crate::config::MonotonicHlc::new(Arc::clone(&clock)));
        Self::new(clock, hlc, rng, keychain)
    }

    /// Mint the LWW stamp for an op this device is about to emit.
    ///
    /// Called ONCE per emitted op: [`HlcClock::send`] is strictly increasing,
    /// so calling it twice for one op would stamp two of its rows differently
    /// and make the op non-atomic under merge.
    ///
    /// The converse matters just as much where **one command emits two ops**
    /// (a completion plus its streak advance; a routine skip plus its task
    /// tombstone). Each row must be stamped with the stamp of the op that
    /// carries it, because that is the stamp every remote replica will write
    /// when the op merges — `materialize_remote` reads it off the envelope.
    /// Stamping a row with its sibling's stamp leaves the origin holding
    /// different `lww_*` columns from everyone else, and the next concurrent
    /// edit then resolves differently there than elsewhere.
    fn lww_stamp(&self, seq: u64) -> LwwStamp {
        LwwStamp {
            hlc: self.hlc.send(),
            device: self.keychain.device_id(),
            seq,
        }
    }

    /// This device's keychain (device id, signing key, cert). Used by the sync
    /// driver / trust flows on `Core`.
    pub(crate) fn keychain(&self) -> &Keychain {
        &self.keychain
    }

    /// Apply a command end-to-end inside a single transaction.
    pub fn apply(&self, db: &mut Db, cmd: Command) -> Result<CommandResult, EngineError> {
        match cmd {
            Command::CreateTask(d) => self.create_task(db, d),
            Command::UpdateTask { id, patch } => self.update_task(db, id, patch),
            Command::CompleteTask(id) => self.complete_task(db, id),
            Command::DeferTask { id, to_ms } => self.defer_task(db, id, to_ms),
            Command::DeleteTask(id) => self.delete_task(db, id),
            Command::PromoteToStream { id, stream } => self.promote_task(db, id, stream),
            Command::CreateStream(d) => self.create_stream(db, d),
            Command::UpdateStream { id, patch } => self.update_stream(db, id, patch),
            Command::DeleteStream(id) => self.delete_stream(db, id),
            Command::CreateContext(d) => self.create_context(db, d),
            Command::UpdateContext { id, patch } => self.update_context(db, id, patch),
            Command::DeleteContext(id) => self.delete_context(db, id),
            Command::CreateRoutine(d) => self.create_routine(db, d),
            Command::UpdateRoutine { id, patch } => self.update_routine(db, id, patch),
            Command::DeleteRoutine(id) => self.delete_routine(db, id),
            Command::SkipRoutineOccurrence { id, occurrence_key } => {
                self.skip_routine_occurrence(db, id, occurrence_key)
            }
            Command::CreateBlock(d) => self.create_block(db, d),
            Command::ImportBlock { source, uid, draft } => {
                self.import_block(db, &source, &uid, draft)
            }
            Command::UpdateBlock { id, patch } => self.update_block(db, id, patch),
            Command::DeleteBlock(id) => self.delete_block(db, id),
            Command::BindTask { block, task } => self.bind_task(db, block, task),
            Command::UnbindTask { block, task } => self.unbind_task(db, block, task),
            Command::AttachFile(d) => self.attach_file(db, d),
            Command::DetachFile(id) => self.detach_file(db, id),
            Command::MaterializeRoutines { now_ms } => self.materialize_routines(db, now_ms),
            Command::RevokeDevice {
                device_id,
                reason,
                effective_at_ms,
            } => self.revoke_device(db, device_id, reason, effective_at_ms),
            Command::RotateStreamKey { stream } => self.rotate_stream_key(db, stream),
            Command::StartFocus(d) => self.start_focus(db, d),
            Command::EndFocus {
                session,
                actual_focused_ms,
                completed_task,
            } => self.end_focus(db, session, actual_focused_ms, completed_task),
            Command::LogInterruption { session, reason } => {
                self.log_interruption(db, session, reason)
            }
            Command::SaveReviewSnapshot(d) => self.save_review_snapshot(db, d),
        }
    }

    /// Run a read query against the materialized tables.
    pub fn query(&self, db: &Db, q: Query) -> Result<QueryResult, EngineError> {
        match q {
            Query::Today { now_ms, contexts } => self.query_today(db, now_ms, &contexts),
            Query::Inbox => self.query_stream_tasks(db, &inbox_stream_ref()),
            Query::StreamTasks(s) => self.query_stream_tasks(db, &s),
            Query::ContextTasks(c) => self.query_context_tasks(db, &c),
            Query::EntityById(r) => self.query_entity(db, r),
            Query::DeviceList => self.query_device_list(db),
            Query::StreamList => self.query_stream_list(db),
            Query::Contexts => self.query_contexts(db),
            Query::Routines => self.query_routines(db),
            Query::Actionable { stream, limit } => self.query_actionable(db, stream, limit),
            Query::FocusPlan {
                stream,
                energy,
                length,
                limit,
            } => self.query_focus_plan(db, stream, energy, length, limit),
            Query::TaskFocusSessions { task, limit } => {
                self.query_task_focus_sessions(db, task, limit)
            }
            Query::RunningFocusSessions => self.query_running_focus_sessions(db),
            Query::FocusStats {
                stream,
                since_ms,
                now_ms,
            } => self.query_focus_stats(db, stream, since_ms, now_ms),
            Query::UnblockCascade(task) => self.query_unblock_cascade(db, task),
            Query::WeeklyReview {
                week_start_ms,
                now_ms,
            } => self.query_weekly_review(db, week_start_ms, now_ms),
            Query::DailyReview { since_ms, now_ms } => {
                self.query_daily_review(db, since_ms, now_ms)
            }
            Query::StreamTrends { weeks, now_ms } => self.query_stream_trends(db, weeks, now_ms),
            Query::ActivityTimeline { entity, limit } => {
                self.query_activity_timeline(db, entity, limit)
            }
            Query::ReviewHistory { limit } => self.query_review_history(db, limit),
            Query::ExportStats {
                dataset,
                format,
                weeks,
                now_ms,
            } => self.query_export(db, dataset, format, weeks, now_ms),
            Query::TaskAttachments(task) => self.query_task_attachments(db, task),
            Query::MorningSummary { now_ms } => self.query_morning_summary(db, now_ms),
            Query::EndOfDayPlan { now_ms } => self.query_end_of_day_plan(db, now_ms),
            Query::ReminderIntents {
                now_ms,
                horizon_ms,
                settings,
            } => self.query_reminder_intents(db, now_ms, horizon_ms, &settings),
            Query::DayBlocks { day_ms } => self.query_day_blocks(db, day_ms),
            Query::WeekBlocks { week_ms } => self.query_week_blocks(db, week_ms),
            Query::Search { text, limit } => self.query_search(db, &text, limit),
            // Sync status is owned by `Core` (it reads the live `SyncShared` and
            // the DB outbox count); the engine never serves it.
            Query::SyncStatus => Err(EngineError::Invalid(
                "SyncStatus is served by Core, not the Engine".into(),
            )),
        }
    }

    // ---- device lifecycle + remote apply (receive half of sync) ----

    /// Revoke a device: mint a new epoch for every stream it could read, seal
    /// the new keys to the devices that remain, and record the revocation as an
    /// op so every replica makes the same cut.
    ///
    /// Three things happen in one transaction, and the order is the design:
    ///
    /// 1. The `DeviceRevoke` op is emitted and the local `devices` row is
    ///    marked, so nothing after this point treats the device as a recipient.
    /// 2. Every stream in the rotation set — the vault-meta stream and the
    ///    Inbox included, not only user Streams — mints a fresh epoch.
    /// 3. The `key_envelope` ops carrying those keys are sealed under the
    ///    **pre-rotation** vault-meta epoch, because a device that has not yet
    ///    received the new meta key cannot read an op sealed under it.
    ///
    /// The vault-meta stream being in the set is what makes this more than
    /// theatre. Leaving it on a fixed epoch would let a revoked device keep
    /// reading every Stream, Context and Routine created afterwards — the
    /// *shape* of the account, indefinitely — while only its task content went
    /// dark.
    ///
    /// What revocation cannot do, and does not pretend to: the revoked device
    /// keeps every key it already held, so it keeps everything it could already
    /// read. Rotation bounds forward exposure, never backward.
    fn revoke_device(
        &self,
        db: &mut Db,
        device_id: EntityRef,
        reason: RevokeReason,
        effective_at_ms: Option<u64>,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let revoked = *device_id.bytes();
        if revoked == self.keychain.device_id() {
            return Err(EngineError::Invalid(
                "a device cannot revoke itself: it would rotate every key away from the only \
                 device holding them"
                    .into(),
            ));
        }
        let effective_at_ms = effective_at_ms.unwrap_or(now_ms);
        // Validated here as well as on apply, and against a *tighter* window,
        // so that a cut this device emits is one every replica will accept.
        // `Hlc::send` takes `max(now, local)`, and the clock gate keeps the
        // local reading within `MAX_DRIFT_MS` of `now`, so an
        // `effective_at_ms` in `now ..= now + REVOKE_CUT_AHEAD_MS` is inside
        // the apply-side window whatever the envelope's HLC turns out to be.
        // The caller reaches this from `Command::RevokeDevice`, whose
        // `effective_at_ms: Option<u64>` is otherwise unchecked all the way out
        // to the Swift seam.
        if effective_at_ms < now_ms || effective_at_ms > now_ms + REVOKE_CUT_AHEAD_MS {
            return Err(EngineError::Invalid(format!(
                "a revocation takes effect from now onwards: effective_at_ms must be between now \
                 ({now_ms}) and {} days ahead of it, not {effective_at_ms}",
                REVOKE_CUT_AHEAD_MS / (24 * 60 * 60 * 1000)
            )));
        }
        let op_id = self.fresh_op_id(now_ms);
        let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: revoked,
            reason_code: reason,
            effective_at_ms,
        }))?;

        // Read inside the transaction, and after the epoch below has been
        // resolved. Resolving it can mint the vault-meta stream's own first key
        // and emit `key_envelope` ops into this very stream, each taking a
        // `seq`; a number read beforehand would already be spent by the time
        // this op reached the log, and `ops` has a
        // `UNIQUE(stream_id, device_id, seq)`. That is the bug `60ee61d`
        // fixed for `emit_control_op`, and this is the same shape.
        let mut seq = 0u64;
        db.with_tx(|tx| -> rusqlite::Result<()> {
            let known: i64 = tx.query_row(
                "SELECT count(*) FROM devices WHERE device_id = ?",
                params![&revoked[..]],
                |r| r.get(0),
            )?;
            if known == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            // The epoch every rotation op is sealed under: read before
            // anything is minted, so it is the epoch the departing devices and
            // the remaining ones all still share.
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            seq = self.next_seq_tx(tx, &META_STREAM)?;
            let hlc = self.hlc.send();

            // 1. The revocation itself, sealed under the old meta epoch like
            //    every other op in this transaction.
            self.ops_insert_at(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                hlc,
                &inner,
                "device.revoke",
                "device",
                Some(&revoked),
                Some(now_ms),
                None,
                now_ms,
                &[],
                seal_under.0,
                &seal_under.1,
            )?;
            tx.execute(
                "UPDATE devices
                 SET revoked_at_ms = ?, revoked_by = ?, revoke_reason = ?
                 WHERE device_id = ?",
                params![
                    i64::try_from(effective_at_ms).unwrap_or(i64::MAX),
                    &self.keychain.device_id()[..],
                    reason.as_str(),
                    &revoked[..],
                ],
            )?;

            // 2 + 3. Rotate everything, telling only the devices that remain.
            for stream_id in self.keychain.rotation_set(tx)? {
                let (epoch, key) =
                    self.keychain
                        .mint_epoch(tx, &stream_id, self.rng.as_ref(), now_ms)?;
                self.emit_key_envelopes(tx, &stream_id, epoch, &key, now_ms, Some(&seal_under))?;
            }
            Ok(())
        })
        .map_err(|e| match e {
            sunrise_storage::DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                EngineError::NotFound(format!("device {}", hex_short(&revoked)))
            }
            other => EngineError::Storage(other),
        })?;

        Ok(CommandResult::new(device_id, None, op_id, seq))
    }

    /// Mint a new epoch for one Stream and distribute it.
    ///
    /// The narrow form of what [`Self::revoke_device`] does to everything: used
    /// when a Stream key is believed exposed without a device being at fault,
    /// and as the seam a share-revocation will hang off.
    fn rotate_stream_key(
        &self,
        db: &mut Db,
        stream: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let stream_id = *stream.bytes();
        let mut minted = 0u32;
        // Read inside the transaction, for the reason given in
        // [`Self::revoke_device`]: resolving the meta epoch can emit ops into
        // the very stream this number counts.
        let mut seq = 0u64;
        db.with_tx(|tx| -> rusqlite::Result<()> {
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            seq = self.next_seq_tx(tx, &META_STREAM)?;
            let (epoch, key) =
                self.keychain
                    .mint_epoch(tx, &stream_id, self.rng.as_ref(), now_ms)?;
            minted = epoch;
            self.emit_key_envelopes(tx, &stream_id, epoch, &key, now_ms, Some(&seal_under))
        })?;
        Ok(CommandResult::new(
            stream,
            None,
            [0u8; 16],
            u64::from(minted).max(seq),
        ))
    }

    /// Publish this device's identity-signed cert so every replica can verify
    /// its envelopes.
    ///
    /// Emitted by `Core::open` once per vault. It replaces the manual
    /// `Command::TrustDevice` exchange, which accepted a cert from a caller if
    /// it was self-signed — a check every cert passes, including a stranger's.
    ///
    /// # Errors
    /// Storage failures.
    pub fn publish_device_cert(&self, db: &mut Db) -> Result<(), EngineError> {
        let now_ms = self.clock.now_ms();
        let cert = self.keychain.cert_blob().to_vec();
        let device_id = self.keychain.device_id();
        let already: i64 = db.conn().query_row(
            "SELECT count(*) FROM ops WHERE inner_kind = 'device.cert' AND device_id = ?",
            params![&device_id[..]],
            |r| r.get(0),
        )?;
        if already > 0 {
            return Ok(());
        }
        db.with_tx(|tx| -> rusqlite::Result<()> {
            self.emit_control_op(tx, &InnerOp::DeviceCertPublish(cert), now_ms, None)
        })?;
        Ok(())
    }

    /// Apply a remote op envelope: idempotent, entity-level last-writer-wins.
    ///
    /// This is the receive half of sync. The whole pipeline runs under one
    /// `BEGIN IMMEDIATE` transaction (after out-of-tx crypto verification):
    ///
    /// 1. `decode_envelope` — malformed bytes are rejected.
    /// 2. Sender lookup — `envelope.device_id` must be a trusted (non-revoked)
    ///    row in `devices`, else [`EngineError::UnknownDevice`].
    /// 3. `verify_envelope` against the stored device pubkey — a bad signature
    ///    is never applied.
    /// 4. Decrypt under the shared Stream key (same derivation as the sender —
    ///    the pairing model shares a vault root) and decode the inner op. A
    ///    different vault root derives a different key and fails AEAD.
    /// 5. Idempotence gate: `INSERT OR IGNORE` into `ops` on the deterministic
    ///    op-id and the `UNIQUE(stream_id, device_id, seq)` constraint. If the
    ///    op was already present (`changes() == 0`), return `Ok(None)` with no
    ///    materialization and no event.
    /// 5. Clock gate: the envelope's `hlc` is observed into this device's HLC.
    ///    A reading beyond `MAX_DRIFT_MS` in the future is refused outright —
    ///    see [`sunrise_cbor::hlc`].
    /// 6. LWW materialization: the entity's stored `(hlc, device, seq)` stamp
    ///    is compared against the envelope's. The greater tuple wins. A winning
    ///    op performs the same materialized-row upsert the local path does and
    ///    stamps the row with the SENDER's values; a losing op keeps the row
    ///    but stays recorded in the op log.
    /// 7. Advance `sync_cursors(stream_id, device_id)` to `max(seq)`.
    ///
    /// Remote ops are **not** enqueued in the outbox: the relay fans out to
    /// peers, so re-broadcasting a received op would loop.
    ///
    /// Returns the [`DomainEvent`] the caller should broadcast (matching what a
    /// local submit emits for the same op kind), or `Ok(None)` on an idempotent
    /// re-receive.
    ///
    /// # Errors
    /// [`EngineError::RemoteOpInvalid`] for malformed / unverifiable /
    /// undecryptable envelopes, [`EngineError::UnknownDevice`] for an untrusted
    /// sender, and storage errors from the transaction.
    pub fn apply_remote(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
    ) -> Result<Option<DomainEvent>, EngineError> {
        Ok(self
            .apply_remote_all(db, envelope_bytes)?
            .into_iter()
            .next())
    }

    /// [`Self::apply_remote`], returning **every** event the delivery produced.
    ///
    /// One envelope can produce more than one, and the case is not exotic: a
    /// `key_envelope` op is itself silent, but absorbing the key it carries
    /// releases every op that had been parked waiting for it. Those ops
    /// materialize now, and a caller that broadcast only the first — or only
    /// the envelope op's own `None` — would leave a screen showing a vault the
    /// database no longer contains. [`crate::Core::apply_remote`] takes this
    /// form for exactly that reason.
    pub fn apply_remote_all(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
    ) -> Result<Vec<DomainEvent>, EngineError> {
        // a. Decode.
        let env = decode_envelope(envelope_bytes)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("decode: {e}")))?;

        // b. Sender must be a known, non-revoked device.
        //
        //    One family bypasses the lookup, because it is what *creates* the
        //    row the lookup reads: a `DeviceCertPublish` op carries the
        //    sender's own identity-signed cert, and is self-authenticating —
        //    the envelope signature is checked against the cert's own
        //    `d_s_pub`, and the cert against this vault's account identity. A
        //    stranger's cert fails the second check however it is delivered,
        //    which is exactly what `Command::TrustDevice` could not do.
        //
        //    A *revoked* device's row is still found here. Whether its op
        //    stands is a question about the op's timestamp, not about whether
        //    the device is still a member, and step c2 is where that is asked.
        let d_s_pub = match self.lookup_device_cert(db, &env.device_id)? {
            Some(cert_blob) => {
                let cert = DeviceCert::from_cbor(&cert_blob)
                    .map_err(|e| EngineError::RemoteOpInvalid(format!("stored cert: {e}")))?;
                cert.body.d_s_pub
            }
            None => self.self_authenticating_signer(db, envelope_bytes, &env)?,
        };

        // c. Verify the signature before doing anything else.
        verify_envelope(&env, &d_s_pub)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("verify: {e}")))?;

        // c2. A revoked device's past ops still stand; its later ones do not.
        //
        //     The refusal is recorded before it is returned. It is permanent —
        //     there is no un-revoke — and every replica holds the same
        //     revocation op, so every replica makes the same cut. Without the
        //     record the sync cursor would stop one seq short of this op
        //     forever and the relay would replay it, and everything after it,
        //     on every reconnect.
        if self.is_revoked_at(db, &env.device_id, env.hlc.physical_ms)? {
            // An op this replica has already applied is a re-delivery, not a
            // refusal: it is in `ops`, it is materialized, and the answer to
            // seeing it again is the same silence any other re-receive gets.
            // The idempotence gate proper lives at step e, past the decrypt,
            // and this refusal sits ahead of it — so without this check a
            // replay would both write a `refused_ops` row for an op that is in
            // `ops` and turn `Ok(None)` into `Err(DeviceRevoked)`.
            if self.already_applied(db, &env)? {
                return Ok(Vec::new());
            }
            self.record_refusal(db, &env, "device revoked")?;
            return Err(EngineError::DeviceRevoked);
        }

        // d. Decrypt under whichever key at `(stream_id, epoch)` opens it. Two
        //    devices can have minted that epoch concurrently, so this is a
        //    short list rather than a single key, and the AEAD tag is what
        //    picks — not a stored discriminator that could be lied about.
        //
        //    No key at all is NOT an error: the `key_envelope` op that carries
        //    it may simply not have arrived yet, and the two have no ordering
        //    guarantee across streams. Refusing would lose the op (the relay
        //    does not redeliver), and a cursor barrier would stall the whole
        //    stream. It is parked in `deferred_ops` and retried after every
        //    absorbed key.
        let keys = self.keychain.stream_keys_at(&env.stream_id, env.epoch);
        if keys.is_empty() {
            self.defer_op(db, envelope_bytes, &env)?;
            return Ok(Vec::new());
        }
        let inner_cbor = keys
            .iter()
            .find_map(|k| open_envelope(&env, &d_s_pub, Some(k)).ok())
            .ok_or_else(|| {
                EngineError::RemoteOpInvalid(
                    "no key at this (stream, epoch) opens the envelope".into(),
                )
            })?;
        let mut inner = decode_inner_op(&inner_cbor)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("inner op: {e}")))?;
        remap_legacy_inbox(&mut inner);

        // e. Clock gate. A reading far in OUR future is a broken or hostile
        //    clock; absorbing it would drag this device's HLC forward with the
        //    bad one and let the sender win every conflict for the length of
        //    the skew. A reading in the past is fine and common — that is a
        //    device coming back from a week offline.
        self.hlc
            .observe(env.hlc)
            .map_err(|e| EngineError::RemoteOpInvalid(format!("hlc: {e}")))?;

        // The row is stamped with the SENDER'S hlc and seq, never with this
        // device's post-merge reading. Every replica must record the same stamp
        // for the same op, or the LWW winner would depend on delivery order.
        let lww = LwwStamp {
            hlc: env.hlc,
            device: env.device_id,
            seq: env.seq,
        };

        let now_ms = self.clock.now_ms();
        let op_id = remote_op_id(&env.stream_id, &env.device_id, env.seq);
        let target = inner.target_ref();
        let effect = inner.effect();
        let inner_kind = inner.inner_kind();
        let target_kind = inner.target_kind();

        let mut applied = false;
        let mut absorbed: Vec<([u8; 16], u32)> = Vec::new();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            // e. Idempotence gate.
            OpLog::insert(
                tx,
                &op_id,
                &env.stream_id,
                &env.device_id,
                env.seq,
                env.hlc.physical_ms,
                envelope_bytes,
                inner_kind,
                target_kind,
                Some(target.bytes()),
                Some(now_ms),
                Some(&env.device_id),
                now_ms,
                &[],
            )
            .map_err(|e| match e {
                sunrise_storage::OpLogError::Sqlite(s) => s,
                sunrise_storage::OpLogError::Db(_) => rusqlite::Error::ExecuteReturnedResults,
            })?;
            if tx.changes() == 0 {
                // Already applied: nothing further.
                return Ok(());
            }
            applied = true;
            if inner.is_control() {
                // f'. Control ops carry key material and trust, not entity
                //     state. They have no row and no LWW contest; routing one
                //     into `materialize_remote` would file it under `tasks`,
                //     because that function's kind table ends in a `_ =>` arm.
                absorbed =
                    self.apply_control_op(tx, &inner, &env.device_id, env.hlc.physical_ms, now_ms)?;
            } else {
                // f. LWW materialization.
                materialize_remote(tx, &inner, &lww)?;
            }
            // g. Advance the sync cursor to the end of the contiguous prefix.
            upsert_sync_cursor(tx, &env.stream_id, &env.device_id)?;
            Ok(())
        })?;

        if !applied {
            return Ok(Vec::new());
        }
        let mut events = match effect {
            OpEffect::Create => vec![DomainEvent::Created(target)],
            OpEffect::Update => vec![DomainEvent::Updated(target)],
            OpEffect::Delete => vec![DomainEvent::Deleted(target)],
            // The control op itself changed nothing on screen. What it
            // released might have.
            OpEffect::Control => Vec::new(),
        };
        // A newly absorbed key may be the one a parked op was waiting for.
        for (stream_id, epoch) in absorbed {
            events.extend(self.drain_deferred(db, &stream_id, epoch)?);
        }
        Ok(events)
    }

    /// Recover the signing key for an op from a device this vault has never
    /// seen, when — and only when — the op is that device publishing its own
    /// identity-signed cert.
    ///
    /// Both checks matter. The envelope must be signed by the key the cert
    /// names, which proves the sender holds `D_S_priv`; and the cert must
    /// verify under *this vault's* `ID_S_pub` with an `identity_id` recomputed
    /// from it, which proves the account admitted that device. Either one alone
    /// is bypassable: without the first, anyone can replay someone else's cert;
    /// without the second, any self-signed cert is accepted, which is precisely
    /// the hole `Command::TrustDevice` had.
    fn self_authenticating_signer(
        &self,
        db: &Db,
        envelope_bytes: &[u8],
        env: &sunrise_crypto::OpEnvelope,
    ) -> Result<[u8; 32], EngineError> {
        let _ = envelope_bytes;
        // The payload is only readable once we have a key for the stream, and
        // we only have one if the sender is inside the account already. That is
        // the point: the cert travels in the vault-meta stream, sealed under a
        // key only members hold, so a stranger cannot even present one.
        let keys = self.keychain.stream_keys_at(&env.stream_id, env.epoch);
        for key in &keys {
            // Verification is deferred to the caller, so open without checking
            // the signature first — the payload is authenticated by the AEAD
            // tag regardless, and the signature check follows immediately.
            let Ok(inner_cbor) = open_envelope_unverified(env, key) else {
                continue;
            };
            let Ok(InnerOp::DeviceCertPublish(cert_cbor)) = decode_inner_op(&inner_cbor) else {
                continue;
            };
            let cert = DeviceCert::from_cbor(&cert_cbor)
                .map_err(|e| EngineError::RemoteOpInvalid(format!("published cert: {e}")))?;
            if cert.body.device_id != env.device_id {
                return Err(EngineError::RemoteOpInvalid(
                    "published cert names another device".into(),
                ));
            }
            cert.verify_binding(
                &self.keychain.identity_signing_pub(),
                &self.keychain.identity_id(),
            )
            .map_err(|e| EngineError::RemoteOpInvalid(format!("published cert: {e}")))?;
            let _ = db;
            return Ok(cert.body.d_s_pub);
        }
        Err(EngineError::UnknownDevice)
    }

    /// Whether `device_id` was revoked with an effect time at or before
    /// `at_ms`, which is the envelope's own HLC reading.
    ///
    /// The comparison is against a timestamp the *sender* chose and signed, so
    /// this is a **convergence** boundary rather than a cryptographic one: a
    /// revoked device that can still reach a peer can date an op below the cut
    /// and have it applied. Only the direction is constrained — the clock gate
    /// in step e refuses a reading in this device's future, so the lie can only
    /// ever be backwards, into a past this device already agrees existed.
    ///
    /// Closing it is a *transport* question, not a comparison this function
    /// could make. `DELETE /api/v1/devices/{id}` makes the relay refuse the
    /// device's signed uploads and its sync sessions outright, and nothing at
    /// all reaches a peer after that. `Command::RevokeDevice` does not call it:
    /// the `device_revoke` op is vault state, the relay never reads it
    /// (`docs/03-crypto/key-rotation.md` §Revocation step 3, marked not built),
    /// and wiring the two together is its own change.
    ///
    /// The residual is written down in `docs/01-architecture/threat-model.md`
    /// §A3 together with the reason it is the right trade. The alternative —
    /// treating revocation as retroactive, which is what filtering the cert
    /// lookup on `revoked_at_ms` amounted to — throws away honest work a user
    /// did yesterday on the laptop they revoked today. That is a worse and far
    /// more likely failure than a backdating attacker who, by construction,
    /// already had to be reachable and is already inside the account.
    fn is_revoked_at(
        &self,
        db: &Db,
        device_id: &[u8; 16],
        at_ms: u64,
    ) -> Result<bool, EngineError> {
        let revoked: Option<i64> = db
            .conn()
            .query_row(
                "SELECT revoked_at_ms FROM devices WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(revoked.is_some_and(|effective| u64::try_from(effective).unwrap_or(0) <= at_ms))
    }

    /// Whether this replica has already recorded `env` in its op log.
    ///
    /// The same identity the idempotence gate at step e uses, asked early
    /// enough to keep a re-delivery out of the refusal path.
    fn already_applied(
        &self,
        db: &Db,
        env: &sunrise_crypto::OpEnvelope,
    ) -> Result<bool, EngineError> {
        let op_id = remote_op_id(&env.stream_id, &env.device_id, env.seq);
        let found: Option<i64> = db
            .conn()
            .query_row(
                "SELECT 1 FROM ops WHERE op_id = ?",
                params![&op_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Record that this replica will never apply `env`, so the sync cursor can
    /// move past it.
    ///
    /// Only for refusals that are **permanent and converged**: the envelope's
    /// signature has verified, so the bytes are known intact, and the reason is
    /// a function of state every replica shares. Two refusals deliberately do
    /// not qualify:
    ///
    /// * a decode or signature failure — those bytes may have been damaged in
    ///   transit, and the relay's replay is the only thing that would ever
    ///   repair them;
    /// * "no key at this `(stream, epoch)` opens it" — two devices can mint one
    ///   epoch concurrently (D-7), so the key that opens it may still be in a
    ///   `key_envelope` op on its way. Recording that as decided would turn a
    ///   late key into permanent data loss.
    ///
    /// Stored as one **range** per `(stream_id, device_id)` rather than one row
    /// per op: `seq` and `hlc` are both strictly increasing on the emitting
    /// device, so for one pair the seq order is the HLC order, and the only
    /// permanent refusal is a comparison against that HLC whose cut only ever
    /// moves earlier. Refusals are therefore contiguous from the first one
    /// onward. Without that compression a revoked device — which keeps its
    /// relay credentials, since `Command::RevokeDevice` never calls
    /// `DELETE /api/v1/devices/{id}` — turns every op it goes on signing into a
    /// permanent row on every peer, which is the same unbounded-growth defect
    /// [`DEFERRED_TOTAL_CAP`] exists to prevent one table over.
    fn record_refusal(
        &self,
        db: &mut Db,
        env: &sunrise_crypto::OpEnvelope,
        reason: &str,
    ) -> Result<(), EngineError> {
        let now_ms = self.clock.now_ms();
        db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO refused_ops
                 (stream_id, device_id, from_seq, through_seq, reason, refused_at_ms)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5)
                 ON CONFLICT(stream_id, device_id) DO UPDATE SET
                    from_seq = MIN(from_seq, excluded.from_seq),
                    through_seq = MAX(through_seq, excluded.through_seq),
                    reason = excluded.reason,
                    refused_at_ms = excluded.refused_at_ms",
                params![
                    &env.stream_id[..],
                    &env.device_id[..],
                    i64::try_from(env.seq).unwrap_or(i64::MAX),
                    reason,
                    now_ms
                ],
            )?;
            // The refusal is what unblocks the prefix, so the cursor is
            // recomputed inside the same transaction that recorded it.
            upsert_sync_cursor(tx, &env.stream_id, &env.device_id)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Park an op whose Stream key has not arrived yet.
    ///
    /// Bounded by [`DEFERRED_PER_EPOCH_CAP`] and [`DEFERRED_TOTAL_CAP`]. This
    /// runs before anything about the payload has been checked — the key
    /// that would open it is precisely what is missing — so the only thing
    /// standing between a member and unbounded storage on every peer is the
    /// two caps. Overflow evicts oldest-first; see [`DEFERRED_TOTAL_CAP`] for
    /// why that direction and not the other.
    fn defer_op(
        &self,
        db: &mut Db,
        envelope_bytes: &[u8],
        env: &sunrise_crypto::OpEnvelope,
    ) -> Result<(), EngineError> {
        let op_id = remote_op_id(&env.stream_id, &env.device_id, env.seq);
        let now_ms = self.clock.now_ms();
        let mut evicted = 0usize;
        db.with_tx(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO deferred_ops
                 (op_id, stream_id, epoch, envelope, received_at_ms)
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    &op_id[..],
                    &env.stream_id[..],
                    env.epoch,
                    envelope_bytes,
                    now_ms
                ],
            )?;
            // `received_at_ms` alone does not order rows within one clock
            // millisecond, so `op_id` breaks the tie and keeps the eviction
            // total rather than arbitrary.
            evicted += tx.execute(
                "DELETE FROM deferred_ops WHERE op_id IN (
                     SELECT op_id FROM deferred_ops
                     WHERE stream_id = ?1 AND epoch = ?2
                     ORDER BY received_at_ms DESC, op_id DESC
                     LIMIT -1 OFFSET ?3
                 )",
                params![&env.stream_id[..], env.epoch, DEFERRED_PER_EPOCH_CAP],
            )?;
            evicted += tx.execute(
                "DELETE FROM deferred_ops WHERE op_id IN (
                     SELECT op_id FROM deferred_ops
                     ORDER BY received_at_ms DESC, op_id DESC
                     LIMIT -1 OFFSET ?1
                 )",
                params![DEFERRED_TOTAL_CAP],
            )?;
            Ok(())
        })?;
        if evicted > 0 {
            tracing::warn!(
                ev = "core.op.deferred_evicted",
                n = evicted,
                stream_h = hex_short(&env.stream_id),
                "the parked-op buffer is full; the oldest entries were dropped"
            );
        }
        Ok(())
    }

    /// Re-apply every op parked against `(stream_id, epoch)`.
    ///
    /// Called after every absorbed key. A drained op takes the ordinary
    /// `apply_remote` path, so it goes through the same trust, clock and LWW
    /// gates it would have on first delivery; it is only its *arrival order*
    /// that was wrong. The row is deleted before the retry so a permanently
    /// unopenable op cannot make every subsequent absorb replay it forever.
    fn drain_deferred(
        &self,
        db: &mut Db,
        stream_id: &[u8; 16],
        epoch: u32,
    ) -> Result<Vec<DomainEvent>, EngineError> {
        let parked: Vec<Vec<u8>> = {
            let mut stmt = db.conn().prepare(
                "SELECT envelope FROM deferred_ops
                 WHERE stream_id = ? AND epoch = ? ORDER BY received_at_ms",
            )?;
            let rows = stmt
                .query_map(params![&stream_id[..], epoch], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if parked.is_empty() {
            return Ok(Vec::new());
        }
        db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM deferred_ops WHERE stream_id = ? AND epoch = ?",
                params![&stream_id[..], epoch],
            )?;
            // Sweep whatever else has aged out while we are here. A drain is
            // the only moment this table is known to be changing, so it is the
            // one place a prune costs nothing extra; see [`DEFERRED_TTL_MS`]
            // for why a row this old is ciphertext nobody will ever open.
            let cutoff = i64::try_from(self.clock.now_ms().saturating_sub(DEFERRED_TTL_MS))
                .unwrap_or(i64::MAX);
            tx.execute(
                "DELETE FROM deferred_ops WHERE received_at_ms < ?",
                params![cutoff],
            )?;
            Ok(())
        })?;
        let mut events = Vec::new();
        for envelope in parked {
            // A drained op that still cannot be applied is dropped rather than
            // failing the absorb that released it: the key arrived correctly,
            // and one bad op must not undo that.
            if let Ok(more) = self.apply_remote_all(db, &envelope) {
                events.extend(more);
            }
        }
        Ok(events)
    }

    /// Apply one control op. Returns the `(stream_id, epoch)` pairs whose keys
    /// this device newly learned, so the caller can drain their parked ops.
    fn apply_control_op(
        &self,
        tx: &Transaction<'_>,
        inner: &InnerOp,
        sender: &[u8; 16],
        hlc_ms: u64,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        match inner {
            InnerOp::KeyEnvelope(p) => {
                let recipient = match p.recipient {
                    Recipient::Device(id) if id == self.keychain.device_id() => {
                        EnvelopeRecipient::Device
                    }
                    // The identity copy is opened too. A device that already
                    // holds the key learns nothing; one that was paired from a
                    // recovery blob learns everything, and the alternative is a
                    // second op family that says the same thing.
                    Recipient::Identity(_) => EnvelopeRecipient::Identity,
                    // Somebody else's copy. Retained in the log — the relay
                    // fans out to every device — and simply not opened.
                    Recipient::Device(_) => return Ok(Vec::new()),
                };
                let Ok(key) = self.keychain.open_key_envelope(
                    recipient,
                    &p.stream_id,
                    p.epoch,
                    &p.hpke_ciphertext,
                ) else {
                    return Ok(Vec::new());
                };
                // The `key_id` is a routing hint, so it is re-derived from the
                // opened key rather than trusted. A mismatch means the sender
                // is confused or hostile; either way the key itself is what
                // opens ops, and it is filed under its real id.
                if stream_key_id(&key) != p.key_id {
                    return Ok(Vec::new());
                }
                // An epoch far above this replica's live one is refused
                // before it can be written. `MAX(epoch)` is what makes a key
                // live, so absorbing an absurd one strands `mint_epoch` at the
                // saturation point and redirects every op this device seals
                // afterwards to a key nobody else holds. See
                // [`MAX_EPOCH_LEAP`].
                let live = self
                    .keychain
                    .current_epoch_tx(tx, &p.stream_id)?
                    .unwrap_or(0);
                if p.epoch > live.saturating_add(MAX_EPOCH_LEAP) {
                    tracing::warn!(
                        ev = "core.key.epoch_refused",
                        stream_h = hex_short(&p.stream_id),
                        epoch = p.epoch,
                        live,
                        "a key envelope names an epoch too far above this vault's own"
                    );
                    return Ok(Vec::new());
                }
                let learned = self.keychain.absorb_stream_key(
                    tx,
                    &p.stream_id,
                    p.epoch,
                    &key,
                    KeySource::Envelope,
                    self.rng.as_ref(),
                    now_ms,
                )?;
                Ok(if learned {
                    vec![(p.stream_id, p.epoch)]
                } else {
                    Vec::new()
                })
            }
            InnerOp::DeviceRevoke(p) => {
                // The cut is bounded against the op's own HLC before it is
                // allowed to mean anything.
                //
                // A revocation says "from now": it is emitted by a device that
                // has just decided another one is gone, so its cut belongs
                // beside the reading it was signed with. Below that reading it
                // would reach backwards and invalidate work the revoked device
                // did honestly — which `MIN` below would let a single op do to
                // an account's whole history. Far above it, it is an immunity:
                // `u64::MAX` sets a cut no HLC reaches and, because `MIN`
                // converges on the earliest, every later genuine revocation
                // lands on it and refuses nothing. See [`REVOKE_CUT_AHEAD_MS`]
                // for why the forward window is a week rather than a minute.
                //
                // `MAX_DRIFT_MS` backwards, because that is exactly the gap an
                // honest emitter produces: `effective_at_ms` defaults to the
                // local wall clock while the envelope carries a merged HLC,
                // which the clock gate lets run that far ahead of it.
                let floor = hlc_ms.saturating_sub(sunrise_cbor::hlc::MAX_DRIFT_MS);
                let ceiling = hlc_ms.saturating_add(REVOKE_CUT_AHEAD_MS);
                if p.effective_at_ms < floor || p.effective_at_ms > ceiling {
                    tracing::warn!(
                        ev = "core.device.revoke_refused",
                        reason = "cut_out_of_range",
                        target_h = hex_short(&p.revoked_device_id),
                        effective_at_ms = p.effective_at_ms,
                        hlc_ms,
                        "a device_revoke's cut is not close enough to the op that declares it"
                    );
                    return Ok(Vec::new());
                }
                // The **earliest** cut wins, and every revocation is applied
                // rather than only the first to arrive.
                //
                // `WHERE revoked_at_ms IS NULL` was first-writer-wins, which
                // was harmless while any non-NULL value untrusted the device
                // outright and is not now that the column is compared against
                // an HLC: the first revocation to land would be the only one
                // that ever did, so two honest concurrent revocations with
                // different cuts would resolve by arrival order and two
                // replicas holding identical op sets would disagree about
                // which ops stand.
                //
                // `MIN` over `(effective_at_ms, revoked_by)` is a join —
                // commutative, associative, idempotent — so the row is a
                // function of the *set* of revocation ops applied and not of
                // their order. `revoked_by` is in the key so that two cuts at
                // the same millisecond still pick the same winner everywhere,
                // rather than leaving those two columns order-dependent while
                // the timestamp converges. It is also safe-directional: the cut
                // only ever moves earlier, so an op refused once stays refused.
                let effective = i64::try_from(p.effective_at_ms).unwrap_or(i64::MAX);
                tx.execute(
                    "UPDATE devices
                     SET revoked_by = CASE
                             WHEN revoked_at_ms IS NULL
                               OR ?1 < revoked_at_ms
                               OR (?1 = revoked_at_ms AND (revoked_by IS NULL OR ?2 < revoked_by))
                             THEN ?2 ELSE revoked_by END,
                         revoke_reason = CASE
                             WHEN revoked_at_ms IS NULL
                               OR ?1 < revoked_at_ms
                               OR (?1 = revoked_at_ms AND (revoked_by IS NULL OR ?2 < revoked_by))
                             THEN ?3 ELSE revoke_reason END,
                         revoked_at_ms = MIN(COALESCE(revoked_at_ms, ?1), ?1)
                     WHERE device_id = ?4",
                    params![
                        effective,
                        &sender[..],
                        p.reason_code.as_str(),
                        &p.revoked_device_id[..],
                    ],
                )?;
                Ok(Vec::new())
            }
            InnerOp::DeviceCertPublish(cert_cbor) => {
                // Verified in `self_authenticating_signer` before we ever got
                // here for an unknown sender; re-verified here because a known
                // sender's op takes the ordinary path and could otherwise
                // publish an unchecked cert for a third device.
                //
                // Every rejection below is logged rather than returned. The
                // caller is `apply_remote`, which has already accepted the
                // envelope — the signature verified and the sender is a member
                // — so failing the whole delivery would put a well-formed op
                // into the refusal path over a payload defect. But dropping it
                // *silently* is how a `d_d_pub` stays NULL forever while the
                // vault looks healthy: the device is never a `key_envelope`
                // recipient, its peers' ops park in `deferred_ops`, and nothing
                // anywhere says why.
                let Ok(cert) = DeviceCert::from_cbor(cert_cbor) else {
                    tracing::warn!(
                        ev = "core.device.cert_rejected",
                        reason = "undecodable",
                        sender_h = hex_short(sender),
                        "a published device cert did not decode"
                    );
                    return Ok(Vec::new());
                };
                // A cert is published by the device it names, and by no one
                // else. Without this, any member could rebind a sibling's
                // `cert_blob` — and with it that sibling's `d_s_pub` and
                // `d_d_pub` — through the `ON CONFLICT DO UPDATE` below: a
                // converging denial of service on the sibling's future ops and
                // a redirect of its future `key_envelope`s. The unknown-sender
                // path in `self_authenticating_signer` has always made this
                // check; the two paths now agree.
                if cert.body.device_id != *sender {
                    tracing::warn!(
                        ev = "core.device.cert_rejected",
                        reason = "names_another_device",
                        sender_h = hex_short(sender),
                        subject_h = hex_short(&cert.body.device_id),
                        "a device published a cert naming another device"
                    );
                    return Ok(Vec::new());
                }
                if cert
                    .verify_binding(
                        &self.keychain.identity_signing_pub(),
                        &self.keychain.identity_id(),
                    )
                    .is_err()
                {
                    tracing::warn!(
                        ev = "core.device.cert_rejected",
                        reason = "binding",
                        sender_h = hex_short(sender),
                        "a published device cert does not verify under this account identity"
                    );
                    return Ok(Vec::new());
                }
                tx.execute(
                    "INSERT INTO devices
                     (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms,
                      identity_id, d_d_pub)
                     VALUES (?, ?, ?, ?, ?, NULL, ?, ?)
                     ON CONFLICT(device_id) DO UPDATE SET
                        cert_blob = excluded.cert_blob,
                        nickname = excluded.nickname,
                        platform = excluded.platform,
                        identity_id = excluded.identity_id,
                        d_d_pub = excluded.d_d_pub",
                    params![
                        &cert.body.device_id[..],
                        cert_cbor,
                        cert.body.nickname,
                        cert.body.platform,
                        i64::try_from(cert.body.created_at_ms).unwrap_or(i64::MAX),
                        &cert.body.identity_id[..],
                        &cert.body.d_d_pub[..],
                    ],
                )?;
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// The stored cert for `device_id`, revoked or not. `None` = a device this
    /// vault has never admitted.
    ///
    /// Deliberately **not** filtered on `revoked_at_ms`. Filtering here is what
    /// made a revocation retroactive: with the row hidden, every op from a
    /// revoked device fell through to [`Self::self_authenticating_signer`],
    /// which knows only `DeviceCertPublish`, and so came back `UnknownDevice`
    /// whatever its HLC said — including work the device did honestly, months
    /// before anyone revoked it. The cut is a *time*, and the comparison that
    /// applies it lives in [`Self::is_revoked_at`], one step further on, where
    /// the envelope's HLC is available to compare against.
    fn lookup_device_cert(
        &self,
        db: &Db,
        device_id: &[u8; 16],
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let blob: Option<Vec<u8>> = db
            .conn()
            .query_row(
                "SELECT cert_blob FROM devices WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(blob)
    }

    // ---- command handlers ----

    fn create_task(&self, db: &mut Db, d: TaskDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let task_id = self.fresh_id(EntityKind::Task, now_ms);
        let stream = d.stream_id.unwrap_or_else(inbox_stream_ref);
        let op_id = self.fresh_op_id(now_ms);
        let task = Task {
            reminder_lead_s: d.reminder_lead_s,
            id: task_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            title: d.title.trim().to_string(),
            body: d.body.clone(),
            stream_id: stream,
            contexts: d.contexts.iter().copied().collect(),
            state: TaskState::Todo,
            priority: d.priority,
            energy: d.energy,
            estimated_duration_s: d.estimated_duration_s,
            scheduled_at: d.scheduled_at,
            due_at: d.due_at,
            scheduling_constraints: d.scheduling_constraints.clone(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: d.assignee,
            routine_id: None,
            routine_occurrence: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };

        // Scheduling gate: a `hard` constraint violated by this `scheduled_at`
        // rejects the create outright; `soft` ones ride back on the result.
        let soft_violations = self.check_schedule_constraints(&task)?;

        let inner_op = encode_inner_op(&InnerOp::TaskCreate(task.clone()))?;
        let seq = self.next_seq(db, stream.bytes())?;
        let lww = self.lww_stamp(seq);

        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms, None, false, false, false)?;
            insert_task_row(tx, &task, &lww)?;
            insert_task_contexts(tx, &task)?;
            replace_task_blockers(tx, &task)?;
            ftsr_upsert_task(tx, &task)?;
            self.ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.create",
                "task",
                Some(task_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(
            CommandResult::new(task_id, Some(TaskState::Todo), op_id, seq)
                .with_soft_violations(soft_violations),
        )
    }

    fn update_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: TaskPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        let prev_state = task.state;
        // Which gates this patch has to clear. Scheduling constraints are only
        // re-evaluated when the patch actually *schedules* (per
        // docs/02-domain/scheduling-constraints.md a `hard` violation "fails
        // validation when the user schedules a Task against it") — renaming a
        // task that already sits outside its window must not be rejected.
        let touches_schedule =
            patch.scheduled_at.is_some() || patch.scheduling_constraints.is_some();
        let touches_blockers = patch.blocked_by.is_some();

        if let Some(t) = patch.title {
            task.title = t.trim().to_string();
        }
        if let Some(b) = patch.body {
            task.body = b;
        }
        if let Some(s) = patch.stream_id {
            require_kind(s, EntityKind::Stream)?;
            task.stream_id = s;
        }
        if let Some(cs) = patch.contexts {
            task.contexts = cs.into_iter().collect();
        }
        if let Some(state) = patch.state {
            if !task.state.can_transition_to(state) {
                return Err(EngineError::Invalid(format!(
                    "task state {:?} cannot transition to {:?}",
                    task.state, state
                )));
            }
            task.state = state;
            if state == TaskState::Done {
                task.completed_at = Some(ms_to_ts(now_ms as i64).into());
            } else {
                task.completed_at = None;
            }
        }
        if let Some(p) = patch.priority {
            task.priority = p;
        }
        if let Some(e) = patch.energy {
            task.energy = e;
        }
        if let Some(d) = patch.estimated_duration_s {
            task.estimated_duration_s = d;
        }
        if let Some(s) = patch.scheduled_at {
            task.scheduled_at = s;
        }
        if let Some(d) = patch.due_at {
            task.due_at = d;
        }
        if let Some(list) = patch.scheduling_constraints {
            task.scheduling_constraints = list;
        }
        if let Some(bs) = patch.blocked_by {
            task.blocked_by = bs.into_iter().collect();
        }
        if let Some(a) = patch.assignee {
            task.assignee = a;
        }
        if let Some(lead) = patch.reminder_lead_s {
            task.reminder_lead_s = lead;
        }
        if let Some(arch) = patch.archived {
            task.archived = arch;
        }
        // Re-check cross-field invariants on the patched Task. A patch that
        // sets only `due_at` earlier than the existing `scheduled_at` (or an
        // invalid constraint list) would otherwise pass silently.
        task.validate_invariants()?;
        if touches_blockers {
            // Local cycle/self-block check over the dependency index, per
            // docs/02-domain/tasks.md §Validation. Deliberately local: a cycle
            // formed concurrently on two devices is broken by the merge
            // tie-breaker, not here.
            let graph = read_dependency_graph(db.conn())?;
            if graph.would_cycle(task.id, &task.blocked_by) {
                return Err(ValidationError::BlockedByCycle.into());
            }
        }
        let soft_violations = if touches_schedule {
            self.check_schedule_constraints(&task)?
        } else {
            Vec::new()
        };
        task.updated_at = ms_to_ts(now_ms as i64);

        // Routine streak: the *first* pending -> done transition of an
        // occurrence moves the counter. `record_occurrence_completion` owns the
        // grace-window / forgiveness / idempotency rules and reports
        // `Duplicate` when this occurrence was already counted, in which case
        // no routine op is emitted at all.
        let streak_op = if patch.state == Some(TaskState::Done) && prev_state != TaskState::Done {
            self.streak_advance(db, &task, now_ms)?
        } else {
            None
        };

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        let seq = self.next_seq(db, task.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);

        let task_for_persist = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(
                tx,
                &task_for_persist.stream_id,
                now_ms,
                None,
                false,
                false,
                false,
            )?;
            update_task_row(tx, &task_for_persist, &lww)?;
            replace_task_contexts(tx, &task_for_persist)?;
            replace_task_blockers(tx, &task_for_persist)?;
            ftsr_upsert_task(tx, &task_for_persist)?;
            self.ops_insert(
                tx,
                &op_id,
                task_for_persist.stream_id.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.update",
                "task",
                Some(task_for_persist.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            // The streak advance is a routine.update op in the SAME
            // transaction, so the counter can never diverge from the
            // completion that caused it, and it converges on every other
            // replica through the ordinary full-state routine LWW path.
            if let Some((routine, routine_inner)) = &streak_op {
                let routine_seq = self.next_seq_tx(tx, &META_STREAM)?;
                let routine_lww = self.lww_stamp(routine_seq);
                let routine_op_id = self.fresh_op_id(now_ms);
                // Stamped with the ROUTINE op's own stamp, which is what every
                // remote replica will stamp this row with when the op merges.
                update_routine_row(tx, routine, &routine_lww)?;
                self.ops_insert(
                    tx,
                    &routine_op_id,
                    &META_STREAM,
                    routine_seq,
                    routine_lww.hlc,
                    routine_inner,
                    "routine.update",
                    "routine",
                    Some(routine.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
            }
            Ok(())
        })?;

        Ok(CommandResult::new(id, Some(task.state), op_id, seq)
            .with_soft_violations(soft_violations))
    }

    /// Advance the owning Routine's streak for a task that just went `done`.
    ///
    /// Returns the mutated Routine plus its encoded `RoutineUpdate` inner op,
    /// or `None` when there is nothing to emit: the task is not
    /// routine-generated, its routine is gone, it carries no occurrence
    /// instant, or the occurrence was already counted (the idempotency key set
    /// makes a `done -> todo -> done` round trip a no-op).
    fn streak_advance(
        &self,
        db: &Db,
        task: &Task,
        now_ms: u64,
    ) -> Result<Option<(Routine, Vec<u8>)>, EngineError> {
        let (Some(rid), Some(occurrence_at)) = (task.routine_id, task.routine_occurrence) else {
            return Ok(None);
        };
        let Some(mut routine) = read_routine(db.conn(), rid.bytes())? else {
            return Ok(None);
        };
        let Ok(key) = occurrence_key_at(&routine.timezone, occurrence_at) else {
            return Ok(None);
        };
        let outcome =
            routine.record_occurrence_completion(&key, occurrence_at, ms_to_ts(now_ms as i64));
        if outcome == StreakOutcome::Duplicate {
            return Ok(None);
        }
        routine.updated_at = ms_to_ts(now_ms as i64);
        let inner = encode_inner_op(&InnerOp::RoutineUpdate(Box::new(routine.clone())))?;
        Ok(Some((routine, inner)))
    }

    /// Evaluate a Task's `scheduled_at` against its scheduling constraints.
    ///
    /// Per `docs/02-domain/scheduling-constraints.md`: window dimensions are
    /// civil (zone-less) values pinned to instants by the **device-local**
    /// timezone for a Task, which reaches the engine through the injected
    /// [`Clock`](crate::config::Clock) rather than from ambient process state.
    /// A `hard` violation is rejected; the `soft` ones are returned so the
    /// caller can surface them.
    fn check_schedule_constraints(
        &self,
        task: &Task,
    ) -> Result<Vec<ScheduleConstraint>, EngineError> {
        let Some(at) = task.scheduled_at.as_ref() else {
            return Ok(Vec::new());
        };
        if task.scheduling_constraints.is_empty() {
            return Ok(Vec::new());
        }
        let tz = jiff::tz::TimeZone::get(&self.clock.timezone()).unwrap_or(jiff::tz::TimeZone::UTC);
        // Constraints are civil windows, so a zone-less `scheduled_at` must be
        // resolved in the DEVICE zone before they can be evaluated — which is
        // exactly what `to_instant` does, and the reason it takes a zone.
        let zdt = at.to_instant(&tz).to_zoned(tz);
        let (hard, soft) = violations_by_severity(&task.scheduling_constraints, &zdt);
        if !hard.is_empty() {
            return Err(ValidationError::HardScheduleConstraint.into());
        }
        Ok(soft)
    }

    fn complete_task(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        let patch = TaskPatch {
            state: Some(TaskState::Done),
            ..Default::default()
        };
        self.update_task(db, id, patch)
    }

    fn defer_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        to_ms: u64,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.scheduled_at = Some(ms_to_ts(to_ms as i64).into());
        task.deferred_count = task.deferred_count.saturating_add(1);
        // Deferring *is* scheduling, so it clears the same constraint gate.
        let soft_violations = self.check_schedule_constraints(&task)?;
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        let seq = self.next_seq(db, task.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let task_clone = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_task_row(tx, &task_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                task_clone.stream_id.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.defer",
                "task",
                Some(task_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, Some(task.state), op_id, seq)
            .with_soft_violations(soft_violations))
    }

    fn delete_task(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.deleted = true;
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskDelete(task.clone()))?;
        let seq = self.next_seq(db, task.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let task_clone = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_task_row(tx, &task_clone, &lww)?;
            ftsr_delete_task(tx, &task_clone.id)?;
            self.ops_insert(
                tx,
                &op_id,
                task_clone.stream_id.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.delete",
                "task",
                Some(task_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, Some(task.state), op_id, seq))
    }

    fn promote_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        stream: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        require_kind(stream, EntityKind::Stream)?;
        let patch = TaskPatch {
            stream_id: Some(stream),
            ..Default::default()
        };
        self.update_task(db, id, patch)
    }

    fn create_stream(&self, db: &mut Db, d: StreamDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let stream_id = self.fresh_id(EntityKind::Stream, now_ms);
        let last_key = last_stream_sort_order(db.conn())?;
        let stream = Stream {
            reminder_lead_s: d.reminder_lead_s,
            id: stream_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            name: d.name.trim().to_string(),
            description: d.description.clone(),
            color: d.color.unwrap_or(StreamColor::Slate),
            icon: d.icon.clone(),
            parent_id: d.parent_id,
            // "New streams default between the last and 'end'"
            // (`docs/02-domain/streams.md` §Sort order): one digit's worth of
            // work, and no sibling is rewritten to make room.
            sort_order: sort_order::append_after(last_key.as_deref()),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: d.review_cadence.unwrap_or(StreamReviewCadence::Weekly),
            default_context: d.default_context,
            deleted: false,
            unknown: Unknowns::new(),
        };

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamCreate(stream.clone()))?;
        // Stream lifecycle ops route to the vault-meta log, not the new Stream.
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_stream_row(tx, &stream_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "stream.create",
                "stream",
                Some(stream_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(stream_id, None, op_id, seq))
    }

    fn update_stream(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: StreamPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Stream)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut stream = read_stream(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("stream {id}")))?;
        if let Some(n) = patch.name {
            stream.name = n.trim().to_string();
        }
        if let Some(desc) = patch.description {
            stream.description = desc;
        }
        if let Some(c) = patch.color {
            stream.color = c;
        }
        if let Some(p) = patch.parent_id {
            stream.parent_id = p;
        }
        if let Some(lead) = patch.reminder_lead_s {
            stream.reminder_lead_s = lead;
        }
        if let Some(rc) = patch.review_cadence {
            stream.review_cadence = rc;
        }
        if let Some(icon) = patch.icon {
            stream.icon = icon;
        }
        if let Some(ctx) = patch.default_context {
            stream.default_context = ctx;
        }
        // A reorder. Validated by `patch.validate()` above, so a key that
        // could not have come from `sort_order::between` never reaches the
        // column. Note what this is NOT doing: no sibling is read, and none is
        // rewritten. That is the fractional index paying for itself, and it is
        // also why a reorder merges as one entity-level LWW write — see
        // `Stream::sort_order`.
        if let Some(k) = patch.sort_order {
            stream.sort_order = k;
        }
        if let Some(a) = patch.archived {
            stream.archived = a;
        }
        if let Some(p) = patch.paused {
            stream.paused = p;
        }
        if let Some(pu) = patch.paused_until {
            stream.paused_until = pu;
        }
        stream.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamUpdate(stream.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_stream_row(tx, &stream_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "stream.update",
                "stream",
                Some(stream_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    fn delete_stream(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Stream)?;
        // Compared against the Inbox's own id, not against sixteen zero bytes:
        // those are the vault-meta stream now, and a guard that names the wrong
        // constant is a guard that protects the wrong thing.
        if id.bytes() == &INBOX_STREAM_BYTES || id.bytes() == &META_STREAM {
            return Err(EngineError::Invalid(
                "cannot delete the inbox or vault-meta stream".into(),
            ));
        }
        let now_ms = self.clock.now_ms();
        let mut stream = read_stream(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("stream {id}")))?;
        stream.deleted = true;
        stream.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamDelete(stream.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_stream_row(tx, &stream_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "stream.delete",
                "stream",
                Some(stream_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    // ---- context command handlers ----
    //
    // Context lifecycle ops route to the vault-meta log (same as Streams and
    // routines): a Context is not owned by any one Stream.

    fn create_context(&self, db: &mut Db, d: ContextDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let name = Context::validate_name(&d.name)?;
        let description = Context::validate_description(d.description.as_deref())?;
        // Names are the handle `@name` capture resolves against, so a second
        // live Context with the same name (case-insensitively) would make every
        // such mention permanently ambiguous. Reject it at the source.
        if let Some(clash) = find_context_by_name(db.conn(), &name, None)? {
            return Err(EngineError::Invalid(format!(
                "context name {name:?} is already used by {clash}"
            )));
        }
        let now_ms = self.clock.now_ms();
        let ctx_id = self.fresh_id(EntityKind::Context, now_ms);
        let ctx = Context {
            id: ctx_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            name,
            description,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::ContextCreate(ctx.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_context_row(tx, &ctx, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "context.create",
                "context",
                Some(ctx_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(ctx_id, None, op_id, seq))
    }

    fn update_context(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: ContextPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Context)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut ctx = read_context(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("context {id}")))?;
        if let Some(n) = patch.name {
            let name = Context::validate_name(&n)?;
            if let Some(clash) = find_context_by_name(db.conn(), &name, Some(id.bytes()))? {
                return Err(EngineError::Invalid(format!(
                    "context name {name:?} is already used by {clash}"
                )));
            }
            ctx.name = name;
        }
        if let Some(desc) = patch.description {
            ctx.description = Context::validate_description(desc.as_deref())?;
        }
        // Archiving deliberately leaves `task_contexts` alone: per the spec it
        // only hides the Context from pickers, it does not strip it from Tasks.
        if let Some(a) = patch.archived {
            ctx.archived = a;
        }
        ctx.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::ContextUpdate(ctx.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_context_row(tx, &ctx, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "context.update",
                "context",
                Some(ctx.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Soft-delete a Context and strip it from every Task carrying it.
    ///
    /// Per `docs/02-domain/contexts-and-tags.md` the tombstone and the
    /// membership removal are one transaction: a crash between them would leave
    /// tasks pointing at a context no picker can show. Remote replicas perform
    /// the same purge when they apply the `ContextDelete` op, so the two sides
    /// stay in step without shipping one op per affected Task.
    fn delete_context(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Context)?;
        let now_ms = self.clock.now_ms();
        let mut ctx = read_context(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("context {id}")))?;
        ctx.deleted = true;
        ctx.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::ContextDelete(ctx.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_context_row(tx, &ctx, &lww)?;
            purge_context_from_tasks(tx, id.bytes())?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "context.delete",
                "context",
                Some(ctx.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    // ---- routine command handlers ----

    fn create_routine(&self, db: &mut Db, d: RoutineDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let routine_id = self.fresh_id(EntityKind::Routine, now_ms);
        let routine = Routine {
            id: routine_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            template: d.template,
            rrule: d.rrule,
            timezone: d.timezone,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            scheduling_constraints: d.scheduling_constraints,
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: d.catchup_policy,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };
        let stream = routine.template.stream_id;
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineCreate(Box::new(routine.clone())))?;
        // Routine ops route to the vault-meta log.
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms, None, false, false, false)?;
            insert_routine_row(tx, &routine_clone, now_ms, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.create",
                "routine",
                Some(routine_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        // Materialize the near-horizon occurrences for the freshly-created
        // routine (first run: `materialized_until` is 0, so the window opens at
        // `starts_at` and any missed occurrences run the catchup policy).
        self.materialize_one_routine(db, &routine, now_ms)?;

        Ok(CommandResult::new(routine_id, None, op_id, seq))
    }

    fn update_routine(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: RoutinePatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;

        // Track whether the change affects which occurrences exist (and thus
        // requires regenerating future not-started tasks).
        let mut structural = false;
        if let Some(t) = patch.template {
            routine.template = t;
        }
        if let Some(r) = patch.rrule {
            structural |= routine.rrule != r;
            routine.rrule = r;
        }
        if let Some(tz) = patch.timezone {
            structural |= routine.timezone != tz;
            routine.timezone = tz;
        }
        if let Some(s) = patch.starts_at {
            structural |= routine.starts_at != s;
            routine.starts_at = s;
        }
        if let Some(e) = patch.ends_at {
            structural |= routine.ends_at != e;
            routine.ends_at = e;
        }
        if let Some(list) = patch.scheduling_constraints {
            routine.scheduling_constraints = list;
        }
        if let Some(c) = patch.catchup_policy {
            routine.catchup_policy = c;
        }
        if let Some(g) = patch.grace_window_s {
            routine.grace_window_s = g;
        }
        if let Some(f) = patch.forgiveness_enabled {
            routine.forgiveness_enabled = f;
        }
        if let Some(p) = patch.paused {
            routine.paused = p;
        }
        if let Some(pu) = patch.paused_until {
            routine.paused_until = pu;
        }
        if let Some(a) = patch.archived {
            routine.archived = a;
        }
        routine.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineUpdate(Box::new(routine.clone())))?;
        let stream = routine.template.stream_id;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms, None, false, false, false)?;
            update_routine_row(tx, &routine_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.update",
                "routine",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        if structural {
            self.regenerate_future_tasks(db, &routine, now_ms)?;
        }
        self.materialize_one_routine(db, &routine, now_ms)?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    fn delete_routine(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;
        routine.deleted = true;
        routine.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineDelete(Box::new(routine.clone())))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_routine_row(tx, &routine_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.delete",
                "routine",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    fn skip_routine_occurrence(
        &self,
        db: &mut Db,
        id: EntityRef,
        occurrence_key: String,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;
        if !routine.skipped_keys.contains(&occurrence_key) {
            routine.skipped_keys.push(occurrence_key.clone());
        }
        routine.updated_at = ms_to_ts(now_ms as i64);

        // Tombstone the already-materialized task for this occurrence, but only
        // if it exists and is still an untouched (Todo, non-deferred) routine
        // task — a user who already started/edited it keeps it.
        let task_id = occurrence_task_id(&id, &occurrence_key);
        let existing = read_task(db.conn(), task_id.bytes())?;
        let drop_task = existing
            .as_ref()
            .filter(|t| {
                !t.deleted
                    && t.state == TaskState::Todo
                    && t.deferred_count == 0
                    && t.routine_id == Some(id)
            })
            .cloned();

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineUpdate(Box::new(routine.clone())))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_routine_row(tx, &routine_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.update",
                "routine",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            if let Some(mut t) = drop_task {
                t.deleted = true;
                t.updated_at = ms_to_ts(now_ms as i64);
                let task_seq = self.next_seq_tx(tx, t.stream_id.bytes())?;
                let task_lww = self.lww_stamp(task_seq);
                let del_op = encode_inner_op(&InnerOp::TaskDelete(t.clone()))
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                // Stamped with the TASK op's own stamp, not the routine op's:
                // see the note on `Engine::lww_stamp`.
                update_task_row(tx, &t, &task_lww)?;
                ftsr_delete_task(tx, &t.id)?;
                let del_op_id = self.fresh_op_id(now_ms);
                self.ops_insert(
                    tx,
                    &del_op_id,
                    t.stream_id.bytes(),
                    task_seq,
                    task_lww.hlc,
                    &del_op,
                    "task.delete",
                    "task",
                    Some(t.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
            }
            Ok(())
        })?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    fn materialize_routines(&self, db: &mut Db, now_ms: u64) -> Result<CommandResult, EngineError> {
        let routines = read_routines(db.conn())?;
        for routine in routines {
            self.materialize_one_routine(db, &routine, now_ms)?;
        }
        // No single target entity; return a routine-kind sentinel.
        Ok(CommandResult::new(
            EntityRef::new(EntityKind::Routine, [0u8; 16]),
            None,
            [0u8; 16],
            0,
        ))
    }

    /// Expand a routine over `[max(starts_at, materialized_until), now+horizon)`
    /// and INSERT-OR-IGNORE the resulting tasks, applying the catchup policy to
    /// occurrences at or before `now`. Runs in one transaction per routine:
    /// the inserts and the `materialized_until` advance commit together, so a
    /// crash never leaves the watermark ahead of the tasks it represents.
    fn materialize_one_routine(
        &self,
        db: &mut Db,
        routine: &Routine,
        now_ms: u64,
    ) -> Result<(), EngineError> {
        if routine.paused || routine.archived || routine.deleted {
            return Ok(());
        }
        let mat_until = read_materialized_until(db.conn(), routine.id.bytes())?;
        let horizon_ms = u64::from(materialization_horizon_days(routine.rrule.freq)) * 86_400_000;
        let window_end_ms = now_ms.saturating_add(horizon_ms);
        let starts_ms = u64::try_from(routine.starts_at.as_millisecond().max(0)).unwrap_or(0);
        let window_start_ms = starts_ms.max(mat_until);
        if window_end_ms <= window_start_ms {
            return Ok(());
        }
        let window = (
            ms_to_ts(window_start_ms as i64),
            ms_to_ts(window_end_ms as i64),
        );
        let occ = routine
            .occurrences_in(window)
            .map_err(|e| EngineError::Invalid(format!("routine expand: {e}")))?;
        let now_ts = ms_to_ts(now_ms as i64);

        // Partition into missed (<= now) and future (> now).
        let (missed, future): (Vec<_>, Vec<_>) = occ.into_iter().partition(|o| o.at <= now_ts);

        // Build the (key, instant, optional-title-override) tuples to insert.
        let mut jobs: Vec<(String, jiff::Timestamp, Option<String>)> = Vec::new();
        match routine.catchup_policy {
            RoutineCatchupPolicy::Skip => {}
            RoutineCatchupPolicy::Queue => {
                for o in &missed {
                    jobs.push((o.key.clone(), o.at, None));
                }
            }
            RoutineCatchupPolicy::Merge => {
                let n = missed.len();
                if n >= 2 {
                    let latest = &missed[n - 1];
                    let title = format!("{} (x{n} catch-up)", routine.template.title);
                    jobs.push((latest.key.clone(), latest.at, Some(title)));
                } else if let Some(o) = missed.first() {
                    jobs.push((o.key.clone(), o.at, None));
                }
            }
        }
        for o in &future {
            jobs.push((o.key.clone(), o.at, None));
        }

        let routine = routine.clone();
        let clock = self.clock.clone();
        let rng = self.rng.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(
                tx,
                &routine.template.stream_id,
                now_ms,
                None,
                false,
                false,
                false,
            )?;
            for (key, at, title_override) in &jobs {
                let task = build_routine_task(&routine, key, *at, title_override.clone(), now_ms);
                // Each materialized occurrence is its OWN op, so it gets its own
                // seq and its own HLC tick. Minting both before the insert keeps
                // the row's stamp and the op's envelope in agreement.
                let seq = self.next_seq_tx(tx, task.stream_id.bytes())?;
                let lww = self.lww_stamp(seq);
                let inserted = insert_task_row_or_ignore(tx, &task, &lww)?;
                if !inserted {
                    continue;
                }
                insert_task_contexts(tx, &task)?;
                ftsr_upsert_task(tx, &task)?;
                let inner_op = encode_inner_op(&InnerOp::TaskCreate(task.clone()))
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                let mut rand = [0u8; 10];
                rng.fill_bytes(&mut rand);
                let op_id = *Ulid::from_timestamp_and_random(clock.now_ms(), rand).as_bytes();
                self.ops_insert(
                    tx,
                    &op_id,
                    task.stream_id.bytes(),
                    seq,
                    lww.hlc,
                    &inner_op,
                    "task.create",
                    "task",
                    Some(task.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
            }
            set_materialized_until(tx, routine.id.bytes(), window_end_ms)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Regeneration policy (v1): after a structural routine edit, delete future
    /// routine-materialized tasks that are still untouched (Todo, non-deferred)
    /// and whose deterministic id no longer matches any current future
    /// occurrence. Started, completed, deferred, or user-moved tasks are kept.
    fn regenerate_future_tasks(
        &self,
        db: &mut Db,
        routine: &Routine,
        now_ms: u64,
    ) -> Result<(), EngineError> {
        let horizon_ms = u64::from(materialization_horizon_days(routine.rrule.freq)) * 86_400_000;
        let now_ts = ms_to_ts(now_ms as i64);
        let window = (now_ts, ms_to_ts(now_ms.saturating_add(horizon_ms) as i64));
        let valid: BTreeSet<[u8; 16]> = routine
            .occurrences_in(window)
            .map_err(|e| EngineError::Invalid(format!("routine expand: {e}")))?
            .iter()
            .map(|o| *occurrence_task_id(&routine.id, &o.key).bytes())
            .collect();

        // Candidate future, not-started routine tasks for this routine.
        let rid_blob: Vec<u8> = routine.id.bytes().to_vec();
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE routine_id = ? AND deleted = 0 AND state = 'todo'
               AND deferred_count = 0
               AND scheduled_at_ms IS NOT NULL AND scheduled_at_ms > ?",
        )?;
        let ids = stmt
            .query_map(params![rid_blob, now_ms as i64], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if valid.contains(&bytes) {
                continue;
            }
            let Some(mut t) = read_task(db.conn(), &bytes)? else {
                continue;
            };
            t.deleted = true;
            t.updated_at = ms_to_ts(now_ms as i64);
            let op_id = self.fresh_op_id(now_ms);
            let inner_op = encode_inner_op(&InnerOp::TaskDelete(t.clone()))?;
            let seq = self.next_seq(db, t.stream_id.bytes())?;
            let lww = self.lww_stamp(seq);
            let t_clone = t.clone();
            db.with_tx(|tx| -> rusqlite::Result<()> {
                update_task_row(tx, &t_clone, &lww)?;
                ftsr_delete_task(tx, &t_clone.id)?;
                self.ops_insert(
                    tx,
                    &op_id,
                    t_clone.stream_id.bytes(),
                    seq,
                    lww.hlc,
                    &inner_op,
                    "task.delete",
                    "task",
                    Some(t_clone.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    // ---- query handlers ----

    // ---- focus sessions (ADR-0013) ----

    /// Open a focus session: ADR-0013's `start` op.
    ///
    /// Writes an append-only record keyed by a fresh `fcs_` id and touches the
    /// Task not at all. Two devices doing this concurrently mint different ids,
    /// so both rows survive a merge and both feed the stats — the property that
    /// used to need an OR-Set and now falls out of the key.
    ///
    /// The planned length, the break cadence and the `chunk N of M` marker are
    /// all **derived here** from the Task's estimate and its prior sessions,
    /// not taken from the caller, so every client records the same shape.
    fn start_focus(&self, db: &mut Db, d: FocusStartDraft) -> Result<CommandResult, EngineError> {
        require_kind(d.task_id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let task = read_task(db.conn(), d.task_id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {}", d.task_id)))?;
        if task.deleted {
            return Err(EngineError::Invalid(
                "cannot start a focus session on a deleted task".into(),
            ));
        }
        let prior = count_work_sessions(db.conn(), d.task_id.bytes())?;
        // A break is sized by the pomodoro cycle rather than by the task's
        // estimate: every fourth one is the long break.
        let (planned_ms, chunk) = match d.kind {
            FocusKind::Break => (Some(break_after(prior.max(1)).default_ms()), None),
            FocusKind::Work => {
                let p = plan_session(d.length, task.estimated_duration_s, prior, POMODORO_MS);
                (p.planned_ms, p.chunk)
            }
        };
        let session_id = self.fresh_id(EntityKind::FocusSession, now_ms);
        let start = FocusStart {
            id: session_id,
            task_id: d.task_id,
            stream_id: task.stream_id,
            started_at: sunrise_domain::epoch_ms::from_u64(now_ms),
            planned_ms,
            // The declared budget wins; the Task's own facet is the fallback.
            energy: d.energy.or(task.energy),
            kind: d.kind,
            chunk,
            unknown: Unknowns::new(),
        };
        let inner = encode_inner_op(&InnerOp::FocusStart(Box::new(start.clone())))?;
        let op_id = self.fresh_op_id(now_ms);
        let stream_bytes = *start.stream_id.bytes();
        let seq = self.next_seq(db, &stream_bytes)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &start.stream_id, now_ms, None, false, false, false)?;
            insert_focus_start_row(tx, &start, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner,
                "focus.start",
                "focus_session",
                Some(session_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(session_id, None, op_id, seq))
    }

    /// Close a focus session: ADR-0013's `end` op, a *separate* append-only
    /// record addressed to the same session id.
    ///
    /// This is the one moment the elapsed time becomes a fact: everywhere else
    /// it is derived on read, and nothing ticking is ever written.
    fn end_focus(
        &self,
        db: &mut Db,
        session: EntityRef,
        actual_focused_ms: Option<u64>,
        completed_task: bool,
    ) -> Result<CommandResult, EngineError> {
        require_kind(session, EntityKind::FocusSession)?;
        let now_ms = self.clock.now_ms();
        let view = read_focus_session(db.conn(), session.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("focus session {session}")))?;
        if view.end.is_some() {
            // A session is immutable once closed. Re-closing it would be the
            // mutable-register behaviour ADR-0013 exists to avoid.
            return Err(EngineError::Invalid(format!(
                "focus session {session} has already ended"
            )));
        }
        // Freeze the focused time. The caller may pass a smaller figure when it
        // tracked pauses; the default is the derived elapsed span.
        let actual = actual_focused_ms.unwrap_or_else(|| view.elapsed_ms(now_ms));
        let end = FocusEnd {
            session_id: session,
            ended_at: sunrise_domain::epoch_ms::from_u64(now_ms.max(view.start.started_at_ms())),
            actual_focused_ms: actual,
            interruptions: view.interruptions.clone(),
            completed_task,
            unknown: Unknowns::new(),
        };
        // Issue #10. The user pressed "complete" in the focus UI; the task
        // register is what has to change, and until now nothing changed it.
        // Derived HERE, on the originating device only, and emitted as an
        // ordinary `task.update` — see `autocomplete_focused_task`.
        let completion = if completed_task {
            self.autocomplete_focused_task(db, view.start.task_id, now_ms)?
        } else {
            None
        };
        let inner = encode_inner_op(&InnerOp::FocusEnd(Box::new(end.clone())))?;
        let op_id = self.fresh_op_id(now_ms);
        let stream_bytes = *view.start.stream_id.bytes();
        let seq = self.next_seq(db, &stream_bytes)?;
        let lww = self.lww_stamp(seq);
        let state = completion.as_ref().map(|c| c.task.state);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_focus_end_row(tx, &end, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner,
                "focus.end",
                "focus_session",
                Some(session.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            if let Some(c) = &completion {
                let task_seq = self.next_seq_tx(tx, c.task.stream_id.bytes())?;
                let task_lww = self.lww_stamp(task_seq);
                let task_op_id = self.fresh_op_id(now_ms);
                update_task_row(tx, &c.task, &task_lww)?;
                ftsr_upsert_task(tx, &c.task)?;
                self.ops_insert(
                    tx,
                    &task_op_id,
                    c.task.stream_id.bytes(),
                    task_seq,
                    task_lww.hlc,
                    &c.inner,
                    "task.update",
                    "task",
                    Some(c.task.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
                // Same rule as a hand-completed occurrence: the streak advance
                // rides in the same transaction as the completion that caused
                // it (`docs/08-features/focus-mode.md`: "a focus session that
                // completes a routine occurrence increments the routine
                // streak").
                if let Some((routine, routine_inner)) = &c.streak {
                    let routine_seq = self.next_seq_tx(tx, &META_STREAM)?;
                    let routine_lww = self.lww_stamp(routine_seq);
                    let routine_op_id = self.fresh_op_id(now_ms);
                    update_routine_row(tx, routine, &routine_lww)?;
                    self.ops_insert(
                        tx,
                        &routine_op_id,
                        &META_STREAM,
                        routine_seq,
                        routine_lww.hlc,
                        routine_inner,
                        "routine.update",
                        "routine",
                        Some(routine.id.bytes()),
                        Some(now_ms),
                        None,
                        now_ms,
                        &[],
                    )?;
                }
            }
            Ok(())
        })?;
        Ok(CommandResult::new(session, state, op_id, seq))
    }

    /// **Auto-completion (issue #10).** Close the task a focus session says was
    /// finished, if it is still open.
    ///
    /// # The signal
    ///
    /// Exactly one: `EndFocus { completed_task: true }`. That is not an
    /// inference about the user, it is a **statement by the user** — the focus
    /// screen offers three actions and "complete" is one of them
    /// (`docs/08-features/focus-mode.md` §Focus mode). The fact was already
    /// recorded on the session; the task register simply never moved, which is
    /// what issue #10 calls out as "`completed_at` is never set automatically".
    ///
    /// Signals deliberately NOT taken, because each is an inference rather than
    /// a statement, and inferring is where a task manager turns into the
    /// "no if-this-then-that" non-goal:
    ///
    /// * *A routine occurrence whose window elapsed.* Missed is not done.
    ///   Marking it complete would inflate every completion count in the
    ///   review; the catch-up policy is where a missed occurrence belongs.
    /// * *All of a task's blockers completing.* Blockers describe order, not
    ///   content. A task with no remaining blockers is ready, which is exactly
    ///   what `EffectiveTaskState` already derives.
    /// * *A Block ending with the task still bound.* A Block is a plan for
    ///   when to do work; the spec says outright that there is no "ran the
    ///   block" state and that we trust the user.
    ///
    /// # Determinism and convergence
    ///
    /// The derivation happens **once, on the originating device**, and its
    /// result is an ordinary full-state `task.update` op. A replica applying
    /// the `focus.end` op derives nothing — `materialize_focus_remote` writes
    /// the session record and stops — so the completion cannot be re-derived,
    /// re-timed, or derived differently anywhere. It merges through the same
    /// entity-level LWW path as any hand-edit, so a concurrent edit of the same
    /// task on another device is resolved by the same `(hlc, device, seq)` rule
    /// as every other conflict, with no second writer of task state.
    ///
    /// Returns `None` — a plain no-op, never an error — when there is nothing
    /// to complete: the task is gone, already `done`, or `cancelled`. Cancelled
    /// is not a mistake to correct: `TaskState::can_transition_to` forbids
    /// `cancelled -> done`, and a session closing over a task the user
    /// cancelled must not silently overrule them.
    fn autocomplete_focused_task(
        &self,
        db: &Db,
        task_id: EntityRef,
        now_ms: u64,
    ) -> Result<Option<FocusCompletion>, EngineError> {
        let Some(mut task) = read_task(db.conn(), task_id.bytes())? else {
            return Ok(None);
        };
        if task.deleted || !matches!(task.state, TaskState::Todo | TaskState::InProgress) {
            return Ok(None);
        }
        task.state = TaskState::Done;
        task.completed_at = Some(ms_to_ts(now_ms as i64).into());
        task.updated_at = ms_to_ts(now_ms as i64);
        let streak = self.streak_advance(db, &task, now_ms)?;
        let inner = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        Ok(Some(FocusCompletion {
            task,
            inner,
            streak,
        }))
    }

    /// Log one interruption against a session — a grow-only set member, keyed
    /// by `(session, at_ms, reason)` so re-delivery is a no-op and two devices'
    /// interruptions both survive.
    ///
    /// Allowed against an already-ended session too: a client that closes the
    /// timer before the user picks a reason must not lose the reason.
    fn log_interruption(
        &self,
        db: &mut Db,
        session: EntityRef,
        reason: InterruptionReason,
    ) -> Result<CommandResult, EngineError> {
        require_kind(session, EntityKind::FocusSession)?;
        let now_ms = self.clock.now_ms();
        let start = read_focus_start(db.conn(), session.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("focus session {session}")))?;
        let interruption = Interruption {
            session_id: session,
            at: sunrise_domain::epoch_ms::from_u64(now_ms),
            reason,
        };
        let inner = encode_inner_op(&InnerOp::FocusInterrupt(interruption))?;
        let op_id = self.fresh_op_id(now_ms);
        let stream_bytes = *start.stream_id.bytes();
        let seq = self.next_seq(db, &stream_bytes)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_interruption_row(tx, &interruption)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner,
                "focus.interrupt",
                "focus_session",
                Some(session.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(session, None, op_id, seq))
    }

    /// The Focus Planner (`docs/08-features/focus-mode.md` §Focus Planner).
    ///
    /// Reuses [`Self::query_actionable`]'s graph walk — same SQL, with the
    /// blocked rows filtered out in the database rather than after — and hands
    /// the result to the pure `rank_focus_plan`, which applies the energy match
    /// and the leverage ordering. The scan is capped (see
    /// [`FOCUS_PLAN_SCAN_CAP`]) because the ranking cannot be pushed into SQL.
    fn query_focus_plan(
        &self,
        db: &Db,
        stream: Option<EntityRef>,
        energy: Option<Energy>,
        length: SessionLength,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        if let Some(s) = stream {
            require_kind(s, EntityKind::Stream)?;
        }
        let scan = actionable_scan(db.conn(), stream, FOCUS_PLAN_SCAN_CAP, true)?;
        let priors = work_session_counts(db.conn())?;
        let mut tasks: BTreeMap<EntityRef, Task> = BTreeMap::new();
        let mut candidates = Vec::with_capacity(scan.len());
        for (bytes, open_blockers, unblocks) in scan {
            let Some(task) = read_task(db.conn(), &bytes)? else {
                continue;
            };
            candidates.push(PlanCandidate {
                task: task.id,
                energy: task.energy,
                unblocks,
                open_blockers,
                priority: task.priority,
                due_at_ms: task
                    .due_at
                    .as_ref()
                    .and_then(|t| u64::try_from(t.index_ms()).ok()),
                scheduled_at_ms: task
                    .scheduled_at
                    .as_ref()
                    .and_then(|t| u64::try_from(t.index_ms()).ok()),
                estimated_duration_s: task.estimated_duration_s,
            });
            tasks.insert(task.id, task);
        }
        let ranked = rank_focus_plan(candidates, energy);
        let mut out = Vec::with_capacity(ranked.len().min(limit as usize));
        for r in ranked.into_iter().take(limit as usize) {
            let Some(task) = tasks.remove(&r.candidate.task) else {
                continue;
            };
            let prior = priors.get(&task.id).copied().unwrap_or(0);
            out.push(FocusPlanRow {
                unblocks: r.candidate.unblocks,
                energy_fit: r.fit,
                suggested: plan_session(length, task.estimated_duration_s, prior, POMODORO_MS),
                prior_sessions: prior,
                task,
            });
        }
        Ok(QueryResult::FocusPlan(out))
    }

    /// Focus sessions recorded against one Task, newest first — running ones
    /// included, because a `start` with no `end` is a valid session.
    fn query_task_focus_sessions(
        &self,
        db: &Db,
        task: EntityRef,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        require_kind(task, EntityKind::Task)?;
        let sessions = read_focus_sessions(
            db.conn(),
            "WHERE s.task_id = ?1 ORDER BY s.started_at_ms DESC, s.id DESC LIMIT ?2",
            rusqlite::params![task.bytes().to_vec(), limit],
        )?;
        Ok(QueryResult::FocusSessions(self.focus_rows(sessions)))
    }

    /// Every session with no `end` op yet. This is how a client resumes after a
    /// crash: the dangling start is the state, and reading it is the repair.
    fn query_running_focus_sessions(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let sessions = read_focus_sessions(
            db.conn(),
            "WHERE e.session_id IS NULL ORDER BY s.started_at_ms ASC, s.id ASC",
            rusqlite::params![],
        )?;
        Ok(QueryResult::FocusSessions(self.focus_rows(sessions)))
    }

    /// Decorate session views with the two clock-derived numbers, so callers
    /// never have to reach for a clock of their own.
    fn focus_rows(&self, sessions: Vec<FocusSession>) -> Vec<FocusSessionRow> {
        let now_ms = self.clock.now_ms();
        sessions
            .into_iter()
            .map(|session| FocusSessionRow {
                running: session.is_running(),
                focused_ms: session.focused_ms(now_ms),
                session,
            })
            .collect()
    }

    /// Estimate calibration + focus totals: read the immutable session log,
    /// join each session to its Task's estimate, and hand the whole thing to
    /// the pure `fold_focus_stats`. All the arithmetic lives in the domain
    /// crate so it unit-tests with no database at all.
    fn query_focus_stats(
        &self,
        db: &Db,
        stream: Option<EntityRef>,
        since_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        if let Some(s) = stream {
            require_kind(s, EntityKind::Stream)?;
        }
        let stream_blob: Option<Vec<u8>> = stream.map(|s| s.bytes().to_vec());
        let since = since_ms.and_then(|v| i64::try_from(v).ok());
        let interruptions = read_all_interruptions(db.conn())?;
        let mut stmt = db.conn().prepare(
            "SELECT s.id, s.task_id, s.stream_id, s.energy, s.kind, s.started_at_ms,
                    e.ended_at_ms, e.actual_focused_ms, e.completed_task, t.estimated_min
             FROM focus_sessions s
               LEFT JOIN focus_session_ends e ON e.session_id = s.id
               LEFT JOIN tasks t ON t.id = s.task_id
             WHERE (?1 IS NULL OR s.stream_id = ?1)
               AND (?2 IS NULL OR s.started_at_ms >= ?2)
             ORDER BY s.started_at_ms ASC, s.id ASC",
        )?;
        let rows = stmt
            .query_map(params![stream_blob, since], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let records: Vec<SessionRecord> = rows
            .into_iter()
            .map(|v| {
                let session = ref_of(EntityKind::FocusSession, &v.0);
                SessionRecord {
                    session,
                    task: ref_of(EntityKind::Task, &v.1),
                    stream: ref_of(EntityKind::Stream, &v.2),
                    energy: v.3.as_deref().and_then(parse_energy),
                    kind: FocusKind::from_str_opt(&v.4).unwrap_or(FocusKind::Work),
                    started_at_ms: u64::try_from(v.5.max(0)).unwrap_or(0),
                    ended_at_ms: v.6.and_then(|m| u64::try_from(m.max(0)).ok()),
                    actual_focused_ms: v.7.and_then(|m| u64::try_from(m.max(0)).ok()),
                    // `tasks.estimated_min` is the minute-granular projection
                    // of `Task.estimated_duration_s`; widen it back the same
                    // way `read_task` does so both reads agree.
                    estimated_duration_s: v.9.and_then(|m| {
                        let secs = m.checked_mul(60)?;
                        u64::try_from(secs).ok()
                    }),
                    completed_task: v.8.unwrap_or(0) != 0,
                    interruptions: interruptions.get(&session).cloned().unwrap_or_default(),
                }
            })
            .collect();
        Ok(QueryResult::FocusStats(Box::new(fold_focus_stats(
            &records, now_ms,
        ))))
    }

    /// What completing `task` released — the mid-session unblock cascade.
    ///
    /// Recomputes the frontier from the dependency index against the blockers'
    /// *current* states, exactly like every other derived-`blocked` read, so it
    /// is right whether the completion happened locally or merged in.
    fn query_unblock_cascade(&self, db: &Db, task: EntityRef) -> Result<QueryResult, EngineError> {
        require_kind(task, EntityKind::Task)?;
        // Dependents that are still open, with their *remaining* open-blocker
        // count. An unknown blocker counts as open, matching `query_actionable`.
        let mut stmt = db.conn().prepare(
            "SELECT d.id,
                    (SELECT COUNT(*) FROM task_blockers tb2
                       LEFT JOIN tasks b ON b.id = tb2.blocker_id
                      WHERE tb2.task_id = d.id
                        AND (b.id IS NULL
                             OR (b.deleted = 0
                                 AND b.state NOT IN ('done', 'cancelled')))) AS open_blockers
             FROM task_blockers tb
               JOIN tasks d ON d.id = tb.task_id
             WHERE tb.blocker_id = ?1
               AND d.deleted = 0 AND d.archived = 0
               AND d.state NOT IN ('done', 'cancelled')
             ORDER BY d.id ASC",
        )?;
        let rows = stmt
            .query_map(params![task.bytes().to_vec()], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut graph = DependencyGraph::new();
        let mut open_after: BTreeMap<EntityRef, u32> = BTreeMap::new();
        for (raw, open) in rows {
            let dep = ref_of(EntityKind::Task, &raw);
            graph.add_edge(dep, task);
            open_after.insert(dep, u32::try_from(open.max(0)).unwrap_or(u32::MAX));
        }
        Ok(QueryResult::UnblockCascade(Box::new(unblock_cascade(
            &graph,
            task,
            &open_after,
        ))))
    }

    fn query_routines(&self, db: &Db) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Routines(read_routines(db.conn())?))
    }

    fn query_today(
        &self,
        db: &Db,
        now_ms: u64,
        contexts: &[EntityRef],
    ) -> Result<QueryResult, EngineError> {
        // Today = (a) tasks scheduled within today's local-day window OR
        //         (b) tasks due_at <= now + 24h, AND not done/deleted.
        // For v1 we use a simple rolling 24h window forward from `now_ms`.
        let window_end = now_ms.saturating_add(24 * 60 * 60 * 1000);

        // `contexts` is an OR-set filter per docs/02-domain/contexts-and-tags.md:
        // an empty slice means "no filter", and a non-empty one keeps tasks
        // carrying at least one of the named contexts. The placeholders are
        // generated from the slice length and every value is still bound, so
        // no caller-controlled bytes reach the SQL text.
        let ctx_clause = if contexts.is_empty() {
            String::new()
        } else {
            let placeholders = std::iter::repeat_n("?", contexts.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                " AND EXISTS (SELECT 1 FROM task_contexts tc
                              WHERE tc.task_id = tasks.id
                                AND tc.context_id IN ({placeholders}))"
            )
        };
        let sql = format!(
            "SELECT id FROM tasks
             WHERE deleted = 0 AND archived = 0
               AND state != 'done' AND state != 'cancelled'
               AND ((scheduled_at_ms IS NOT NULL AND scheduled_at_ms <= ?)
                 OR (due_at_ms IS NOT NULL AND due_at_ms <= ?)){ctx_clause}
             ORDER BY COALESCE(scheduled_at_ms, due_at_ms) ASC"
        );
        let mut stmt = db.conn().prepare(&sql)?;

        let mut bound: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(window_end), Box::new(window_end)];
        for c in contexts {
            bound.push(Box::new(c.bytes().to_vec()));
        }
        let ids = stmt
            .query_map(rusqlite::params_from_iter(bound.iter()), |row| {
                let id: Vec<u8> = row.get(0)?;
                Ok(id)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                tasks.push(t);
            }
        }
        Ok(QueryResult::Tasks(tasks))
    }

    /// Open tasks with their derived dependency state, ranked for a planner.
    ///
    /// Neither `blocked` nor `blocks_others` is stored on a Task — both are
    /// recomputed here against the blockers' *current* states, which is what
    /// makes "completing a blocker flips its dependents to actionable" true
    /// with no repair pass and no extra op, locally or after a merge.
    ///
    /// Ordering is the read path a Focus Planner wants: actionable first, then
    /// by how many open dependents finishing the task would release, then by id
    /// so the result is deterministic.
    fn query_actionable(
        &self,
        db: &Db,
        stream: Option<EntityRef>,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        if let Some(s) = stream {
            require_kind(s, EntityKind::Stream)?;
        }
        let rows = actionable_scan(db.conn(), stream, limit, false)?;
        let mut out = Vec::with_capacity(rows.len());
        for (bytes, open, unblocks) in rows {
            let Some(task) = read_task(db.conn(), &bytes)? else {
                continue;
            };
            out.push(ActionableTask {
                effective_state: effective_state(task.state, open),
                open_blockers: open,
                unblocks,
                task,
            });
        }
        Ok(QueryResult::Actionable(out))
    }

    fn query_stream_tasks(&self, db: &Db, stream: &EntityRef) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE stream_id = ? AND deleted = 0
             ORDER BY COALESCE(scheduled_at_ms, due_at_ms) ASC, id ASC",
        )?;
        let stream_blob: Vec<u8> = stream.bytes().to_vec();
        let ids = stmt
            .query_map(params![stream_blob], |row| {
                let id: Vec<u8> = row.get(0)?;
                Ok(id)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                tasks.push(t);
            }
        }
        Ok(QueryResult::StreamTasks(tasks))
    }

    /// Tasks carrying one Context, newest scheduling first.
    ///
    /// Deleted tasks are excluded but *done* ones are not: a Context listing
    /// is "what is tagged this", and hiding completed work would make the
    /// count in the picker disagree with the list it opens.
    fn query_context_tasks(
        &self,
        db: &Db,
        context: &EntityRef,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT t.id FROM tasks t
             JOIN task_contexts tc ON tc.task_id = t.id
             WHERE tc.context_id = ? AND t.deleted = 0
             ORDER BY COALESCE(t.scheduled_at_ms, t.due_at_ms) ASC, t.id ASC",
        )?;
        let blob: Vec<u8> = context.bytes().to_vec();
        let ids = stmt
            .query_map(params![blob], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                tasks.push(t);
            }
        }
        Ok(QueryResult::StreamTasks(tasks))
    }

    fn query_entity(&self, db: &Db, r: EntityRef) -> Result<QueryResult, EngineError> {
        match r.kind() {
            EntityKind::Task => {
                let t = read_task(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("task {r}")))?;
                Ok(QueryResult::Task(Box::new(t)))
            }
            EntityKind::Stream => {
                let s = read_stream(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("stream {r}")))?;
                Ok(QueryResult::Stream(Box::new(s)))
            }
            EntityKind::Context => {
                let c = read_context(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("context {r}")))?;
                Ok(QueryResult::Context(Box::new(c)))
            }
            EntityKind::Routine => {
                let rt = read_routine(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("routine {r}")))?;
                Ok(QueryResult::Routine(Box::new(rt)))
            }
            EntityKind::Block => {
                let b = read_block(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("block {r}")))?;
                Ok(QueryResult::Blocks(vec![block_row(db.conn(), b)?]))
            }
            // A single-row `Attachments`, not a variant of its own: the
            // attachment byte path reads one row by id before it can find the
            // blob, and inventing a second result shape for the same record
            // would make the two reads disagree the first time one gained a
            // field.
            EntityKind::Attachment => {
                let a = read_attachment(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("attachment {r}")))?;
                Ok(QueryResult::Attachments(vec![a]))
            }
            _ => Err(EngineError::Invalid(format!(
                "EntityById not supported for kind {:?} in v1",
                r.kind()
            ))),
        }
    }

    fn query_device_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT device_id, nickname, platform, revoked_at_ms IS NOT NULL FROM devices",
        )?;
        let rows = stmt.query_map([], |row| {
            let blob: Vec<u8> = row.get(0)?;
            let mut a = [0u8; 16];
            let take = blob.len().min(16);
            a[..take].copy_from_slice(&blob[..take]);
            Ok(DeviceRow {
                device_id: a,
                nickname: row.get(1)?,
                platform: row.get(2)?,
                revoked: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(QueryResult::Devices(out))
    }

    fn query_stream_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let inbox = inbox_stream_ref();
        let inbox_blob: Vec<u8> = inbox.bytes().to_vec();

        // Synthetic Inbox row: count of open tasks parked in the inbox stream.
        let inbox_open: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE stream_id = ? AND deleted = 0
               AND state IN ('todo', 'in_progress')",
            params![inbox_blob.clone()],
            |r| r.get(0),
        )?;

        let mut rows = Vec::new();
        rows.push(StreamRow {
            id: inbox,
            name: "Inbox".to_string(),
            color: StreamColor::Slate,
            open_task_count: u64::try_from(inbox_open).unwrap_or(0),
            archived: false,
            paused: false,
            // The Inbox is not a Stream entity and cannot be reordered: it is
            // pushed to the front of this list unconditionally, above. The
            // sentinel says "no position", which is the truth about it.
            sort_order: String::new(),
        });

        // Real streams (exclude deleted and the synthetic inbox row, which
        // `ensure_stream_row` may have materialized with an empty name).
        //
        // Ordered by the fractional index, which is the user's hand-made
        // order and the reason `sort_order` exists. Sorting the *strings* is
        // sorting the numbers they encode — see `sunrise_domain::sort_order`
        // — so this needs no decoding and no index beyond the column itself.
        //
        // Name and id are the tiebreak, not the ordering. They matter in two
        // cases: a row still holding the `''` sentinel because nothing has
        // ever ordered it, which sorts first exactly as an empty name did
        // before this column existed; and two rows that ended up on the same
        // key, which entity-level LWW permits — a device can only lose a
        // reorder wholesale, so it can lose it onto a key a sibling already
        // holds. Ties break the same way on every replica, which is the
        // property that matters.
        let mut stmt = db.conn().prepare(
            "SELECT s.stream_id, s.name, s.color, s.archived, s.paused, s.sort_order,
                    (SELECT COUNT(*) FROM tasks t
                     WHERE t.stream_id = s.stream_id AND t.deleted = 0
                       AND t.state IN ('todo', 'in_progress')) AS open_count
             FROM streams s
             WHERE s.deleted = 0 AND s.stream_id != ?
             ORDER BY s.sort_order ASC, s.name COLLATE NOCASE ASC, s.stream_id ASC",
        )?;
        let mapped = stmt.query_map(params![inbox_blob], |row| {
            let id_blob: Vec<u8> = row.get(0)?;
            let name: String = row.get(1)?;
            let color_str: String = row.get(2)?;
            let archived: i64 = row.get(3)?;
            let paused: i64 = row.get(4)?;
            let sort_order: String = row.get(5)?;
            let open_count: i64 = row.get(6)?;
            let mut a = [0u8; 16];
            let take = id_blob.len().min(16);
            a[..take].copy_from_slice(&id_blob[..take]);
            Ok(StreamRow {
                id: EntityRef::new(EntityKind::Stream, a),
                name,
                color: StreamColor::from_str_lossy(&color_str),
                open_task_count: u64::try_from(open_count).unwrap_or(0),
                archived: archived != 0,
                paused: paused != 0,
                sort_order,
            })
        })?;
        for r in mapped {
            rows.push(r?);
        }
        Ok(QueryResult::Streams(rows))
    }

    /// All live contexts with the number of live tasks carrying each.
    ///
    /// Archived contexts are still listed (with `archived = true`) — archiving
    /// hides a Context from pickers, and that is the caller's filter to apply;
    /// only the tombstone removes it from the list.
    fn query_contexts(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT c.id, c.name, c.description, c.archived,
                    (SELECT COUNT(*) FROM task_contexts tc
                     JOIN tasks t ON t.id = tc.task_id
                     WHERE tc.context_id = c.id AND t.deleted = 0) AS task_count
             FROM contexts c
             WHERE c.deleted = 0
             ORDER BY c.name COLLATE NOCASE ASC, c.id ASC",
        )?;
        let mapped = stmt.query_map([], |row| {
            let id_blob: Vec<u8> = row.get(0)?;
            let mut a = [0u8; 16];
            let take = id_blob.len().min(16);
            a[..take].copy_from_slice(&id_blob[..take]);
            let archived: i64 = row.get(3)?;
            let count: i64 = row.get(4)?;
            Ok(ContextRow {
                id: EntityRef::new(EntityKind::Context, a),
                name: row.get(1)?,
                description: row.get(2)?,
                archived: archived != 0,
                task_count: u64::try_from(count).unwrap_or(0),
            })
        })?;
        let mut rows = Vec::new();
        for r in mapped {
            rows.push(r?);
        }
        Ok(QueryResult::Contexts(rows))
    }

    fn query_search(&self, db: &Db, text: &str, limit: u32) -> Result<QueryResult, EngineError> {
        let match_expr = sanitize_fts_query(text);
        // Empty/whitespace-only input yields no results (never touch FTS).
        if match_expr.is_empty() {
            return Ok(QueryResult::Tasks(Vec::new()));
        }
        let mut stmt = db.conn().prepare(
            "SELECT id FROM search_idx
             WHERE search_idx MATCH ? AND kind = 'task'
             ORDER BY bm25(search_idx)
             LIMIT ?",
        )?;
        let ids = stmt
            .query_map(params![match_expr, i64::from(limit)], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                if !t.deleted {
                    tasks.push(t);
                }
            }
        }
        Ok(QueryResult::Tasks(tasks))
    }

    // ---- id helpers ----

    fn fresh_id(&self, kind: EntityKind, now_ms: u64) -> EntityRef {
        let mut rand = [0u8; 10];
        self.rng.fill_bytes(&mut rand);
        let ulid = Ulid::from_timestamp_and_random(now_ms, rand);
        EntityRef::from_ulid(kind, ulid)
    }

    fn fresh_op_id(&self, now_ms: u64) -> [u8; 16] {
        let mut rand = [0u8; 10];
        self.rng.fill_bytes(&mut rand);
        *Ulid::from_timestamp_and_random(now_ms, rand).as_bytes()
    }
}

// ---- table operations ----

impl Engine {
    /// Seal `inner_op` into a real [`OpEnvelope`] under the routing stream's
    /// **live** epoch and append it to the op log, enqueueing it in the outbox
    /// — all inside the caller's transaction.
    ///
    /// `stream_id` is the op's *routing* stream (the meta stream for
    /// Stream/routine/control ops; the owning Stream for task ops); it is bound
    /// into the envelope and the outbox row. The signing device id is taken
    /// from the keychain, so envelope `seq` is per `(stream_id, device_id)`.
    ///
    /// If the stream has no key yet this mints epoch 1 and emits the
    /// `key_envelope` ops that give every other member of the account a copy —
    /// see [`Self::ensure_stream_epoch`]. That is the whole reason a key can
    /// never be minted silently: an epoch nobody was told about is an epoch
    /// whose ops nobody else can read.
    #[allow(clippy::too_many_arguments)]
    fn ops_insert(
        &self,
        tx: &Transaction<'_>,
        op_id: &[u8; 16],
        stream_id: &[u8; 16],
        seq: u64,
        hlc: Hlc,
        inner_op: &[u8],
        inner_kind: &str,
        target_kind: &str,
        target_id: Option<&[u8; 16]>,
        applied_at_ms: Option<u64>,
        received_from: Option<&[u8; 16]>,
        received_at_ms: u64,
        deps: &[[u8; 16]],
    ) -> rusqlite::Result<()> {
        let (epoch, key) = self.ensure_stream_epoch(tx, stream_id, hlc.physical_ms)?;
        self.ops_insert_at(
            tx,
            op_id,
            stream_id,
            seq,
            hlc,
            inner_op,
            inner_kind,
            target_kind,
            target_id,
            applied_at_ms,
            received_from,
            received_at_ms,
            deps,
            epoch,
            &key,
        )
    }

    /// [`Self::ops_insert`] with the seal epoch chosen by the caller.
    ///
    /// A rotation needs this. The ops that *carry* the new keys have to be
    /// sealed under the **old** epoch: a device that does not yet hold the new
    /// key could not otherwise read the op that gives it one. Sealing them
    /// under the new epoch would be a deadlock — and one that only shows up on
    /// the second device, weeks later.
    ///
    /// A revoked device can read those rotation ops, because it still holds the
    /// old epoch. It learns that a rotation happened and learns nothing else:
    /// each envelope's payload is HPKE-sealed to a recipient's `D_D_pub`, and
    /// the revoked device is not among them.
    #[allow(clippy::too_many_arguments)]
    fn ops_insert_at(
        &self,
        tx: &Transaction<'_>,
        op_id: &[u8; 16],
        stream_id: &[u8; 16],
        seq: u64,
        hlc: Hlc,
        inner_op: &[u8],
        inner_kind: &str,
        target_kind: &str,
        target_id: Option<&[u8; 16]>,
        applied_at_ms: Option<u64>,
        received_from: Option<&[u8; 16]>,
        received_at_ms: u64,
        deps: &[[u8; 16]],
        epoch: u32,
        stream_key: &StreamKey,
    ) -> rusqlite::Result<()> {
        let device_id = self.keychain.device_id();
        let ts_ms = hlc.physical_ms;
        let envelope = self
            .keychain
            .seal_op_at(
                *stream_id,
                seq,
                hlc,
                inner_op,
                self.rng.as_ref(),
                epoch,
                stream_key,
            )
            .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        if let Err(e) = OpLog::insert(
            tx,
            op_id,
            stream_id,
            &device_id,
            seq,
            ts_ms,
            &envelope,
            inner_kind,
            target_kind,
            target_id,
            applied_at_ms,
            received_from,
            received_at_ms,
            deps,
        ) {
            return match e {
                sunrise_storage::OpLogError::Sqlite(s) => Err(s),
                sunrise_storage::OpLogError::Db(_) => Err(rusqlite::Error::ExecuteReturnedResults),
            };
        }
        // Same-transaction outbox enqueue: the pending marker commits with the op.
        Outbox::enqueue(tx, op_id, stream_id, ts_ms)
            .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        // A device has certainly applied its own ops, so its own cursor belongs
        // in `sync_cursors` alongside every peer's. Without it the Subscribe
        // frame claims nothing about this device, and the relay — which filters
        // replay by those cursors — hands the device its entire own history
        // back on every reconnect for it to re-dedupe.
        upsert_sync_cursor(tx, stream_id, &device_id)?;
        Ok(())
    }

    /// The live `(epoch, key)` for `stream_id`, minting epoch 1 and telling
    /// every other member about it if the stream has none.
    ///
    /// The order inside the mint branch matters and is not incidental: the key
    /// row is written **before** the `key_envelope` ops are emitted, because
    /// emitting one is itself an `ops_insert` into the vault-meta stream, which
    /// re-enters here. With the row already present the re-entry terminates
    /// immediately; without it, minting the meta stream's own first key would
    /// recurse forever.
    fn ensure_stream_epoch(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<(u32, StreamKey)> {
        if let Some(found) = self.keychain.current_stream_key_tx(tx, stream_id)? {
            return Ok(found);
        }
        let (epoch, key) = self
            .keychain
            .mint_epoch(tx, stream_id, self.rng.as_ref(), now_ms)?;
        self.emit_key_envelopes(tx, stream_id, epoch, &key, now_ms, None)?;
        Ok((epoch, key))
    }

    /// Emit one `key_envelope` op per recipient for `(stream_id, epoch)`.
    ///
    /// Recipients are every non-revoked device other than this one — which
    /// already holds the key — plus the account identity, whose copy is what
    /// makes a recovery with no surviving device restore readable content
    /// rather than an empty vault.
    ///
    /// `seal_under` chooses the epoch these ops are themselves sealed at; see
    /// [`Self::ops_insert_at`]. `None` means "whatever the meta stream's live
    /// epoch is", which is right for a first mint and wrong for a rotation.
    fn emit_key_envelopes(
        &self,
        tx: &Transaction<'_>,
        stream_id: &[u8; 16],
        epoch: u32,
        key: &StreamKey,
        now_ms: u64,
        seal_under: Option<&(u32, StreamKey)>,
    ) -> rusqlite::Result<()> {
        let key_id = stream_key_id(key);
        let mut recipients: Vec<(Recipient, [u8; 32])> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT device_id, d_d_pub FROM devices
                 WHERE revoked_at_ms IS NULL AND d_d_pub IS NOT NULL",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, pubkey) in rows {
                let id = to16(&id);
                if id == self.keychain.device_id() {
                    continue;
                }
                if pubkey.len() != 32 {
                    continue;
                }
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&pubkey);
                recipients.push((Recipient::Device(id), pk));
            }
        }
        recipients.push((
            Recipient::Identity(self.keychain.identity_id()),
            self.keychain.identity_dh_pub(),
        ));

        for (recipient, recipient_pub) in recipients {
            let Ok(hpke_ciphertext) = self.keychain.seal_key_envelope(
                &recipient_pub,
                stream_id,
                epoch,
                key,
                self.rng.as_ref(),
            ) else {
                // A device whose stored `d_d_pub` is not a usable X25519 point
                // is skipped rather than failing the whole command: one corrupt
                // row must not make the vault unwritable.
                continue;
            };
            let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
                stream_id: *stream_id,
                epoch,
                recipient,
                key_id,
                hpke_ciphertext,
            });
            self.emit_control_op(tx, &inner, now_ms, seal_under)?;
        }
        Ok(())
    }

    /// Log one control op into the vault-meta stream.
    ///
    /// The epoch is resolved **before** the sequence number is read, and that
    /// order is load-bearing. Resolving it can mint the vault-meta stream's own
    /// first key, which emits `key_envelope` ops into this same stream; a `seq`
    /// read before that happened would already be taken by the time this op
    /// reached the log, and `ops` has a `UNIQUE(stream_id, device_id, seq)`.
    /// The op would be ignored, and its outbox row would then fail its foreign
    /// key — which is exactly how this was found.
    fn emit_control_op(
        &self,
        tx: &Transaction<'_>,
        inner: &InnerOp,
        now_ms: u64,
        seal_under: Option<&(u32, StreamKey)>,
    ) -> rusqlite::Result<()> {
        let blob = encode_inner_op(inner).map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        let (epoch, key) = match seal_under {
            Some((epoch, key)) => (*epoch, key.clone()),
            None => self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?,
        };
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.next_seq_tx(tx, &META_STREAM)?;
        let hlc = self.hlc.send();
        self.ops_insert_at(
            tx,
            &op_id,
            &META_STREAM,
            seq,
            hlc,
            &blob,
            inner.inner_kind(),
            inner.target_kind(),
            None,
            Some(now_ms),
            None,
            now_ms,
            &[],
            epoch,
            &key,
        )
    }

    /// Next `seq` for `(stream_id, this device)`, read from the committed DB.
    fn next_seq(&self, db: &Db, stream_id: &[u8; 16]) -> Result<u64, EngineError> {
        let stream_blob: Vec<u8> = stream_id.to_vec();
        let device_blob: Vec<u8> = self.keychain.device_id().to_vec();
        let max: Option<i64> = db
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE stream_id = ? AND device_id = ?",
                params![stream_blob, device_blob],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let next = max.map_or(1, |v| v.saturating_add(1));
        Ok(u64::try_from(next).unwrap_or(1))
    }

    /// Next `seq` for `(stream_id, this device)`, read inside a transaction so it
    /// sees uncommitted inserts from earlier in the same transaction.
    fn next_seq_tx(&self, tx: &Transaction<'_>, stream_id: &[u8; 16]) -> rusqlite::Result<u64> {
        let stream_blob: Vec<u8> = stream_id.to_vec();
        let device_blob: Vec<u8> = self.keychain.device_id().to_vec();
        let max: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE stream_id = ? AND device_id = ?",
            params![stream_blob, device_blob],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(max.saturating_add(1)).unwrap_or(1))
    }

    /// Decode + verify + open the stored envelope for `op_id` back to its
    /// inner-op CBOR, using the keychain. Proves the full seal/unseal cycle;
    /// used by the engine's own tests.
    #[cfg(test)]
    pub(crate) fn open_op_row(&self, db: &Db, op_id: &[u8; 16]) -> Result<Vec<u8>, EngineError> {
        let env = OpLog::get_envelope(db, op_id)?
            .ok_or_else(|| EngineError::NotFound(format!("op {}", hex_short(op_id))))?;
        self.keychain
            .open_op(&env)
            .map_err(|e| EngineError::Invalid(e.to_string()))
    }
}

// ---- remote (LWW) materialization ----

/// Deterministic op-id for a received op, derived from
/// `(stream_id, device_id, seq)`. Two replicas that receive the same op assign
/// it the same op-log primary key, reinforcing the `UNIQUE(stream, device, seq)`
/// idempotence gate.
fn remote_op_id(stream_id: &[u8; 16], device_id: &[u8; 16], seq: u64) -> [u8; 16] {
    let mut km = Vec::with_capacity(16 + 16 + 8);
    km.extend_from_slice(stream_id);
    km.extend_from_slice(device_id);
    km.extend_from_slice(&seq.to_be_bytes());
    let bytes = sunrise_crypto::derive_key("sunrise.remote_op_id.v1", &km, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

/// The end of the run of `ops` seqs starting at `start`, or `start - 1` when
/// `start` itself is absent.
fn ops_run_end(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    start: i64,
) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT CASE
                  WHEN EXISTS (SELECT 1 FROM ops
                               WHERE stream_id = ?1 AND device_id = ?2 AND seq = ?3)
                  THEN (SELECT MIN(o.seq) FROM ops o
                        WHERE o.stream_id = ?1 AND o.device_id = ?2 AND o.seq >= ?3
                          AND NOT EXISTS (SELECT 1 FROM ops n
                                          WHERE n.stream_id = ?1 AND n.device_id = ?2
                                            AND n.seq = o.seq + 1))
                  ELSE ?3 - 1
                END",
        params![&stream_id[..], &device_id[..], start],
        |row| row.get(0),
    )
}

/// The `(from_seq, through_seq)` this replica has permanently refused for
/// `(stream_id, device_id)`, if any.
fn refused_range(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
) -> rusqlite::Result<Option<(i64, i64)>> {
    tx.query_row(
        "SELECT from_seq, through_seq FROM refused_ops
         WHERE stream_id = ? AND device_id = ?",
        params![&stream_id[..], &device_id[..]],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// Set `sync_cursors(stream_id, device_id)` to the end of the **contiguous**
/// applied prefix — the largest `n` for which every seq `1..=n` from that
/// device on that stream is in the op log.
///
/// A high-water mark would be wrong, and used to be what this wrote. Ops do
/// arrive out of order: a dropped frame followed by a later one leaves the log
/// holding seqs `{1, 3}`. A `MAX` cursor then claims 3, and the relay — which
/// now filters its replay by exactly this number (issue #19) — would skip the
/// frame carrying seq 2 forever. The hole would never be refilled and never be
/// noticed. That is silent data loss produced by the very mechanism meant to
/// prevent it, so the cursor has to mean "I have everything through n", which
/// is also how `CursorEntry.last_applied_seq` is read on the wire.
///
/// Regressing the cursor is impossible: the prefix is a function of the op log
/// and the refusal log, and rows are only ever inserted into either, so it can
/// only grow.
fn upsert_sync_cursor(
    tx: &Transaction<'_>,
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
) -> rusqlite::Result<()> {
    // The prefix is the run of *decided* seqs starting at 1, where decided is
    // "applied, or permanently refused". The union is the whole point: a
    // refused op never reaches `ops`, so a prefix read from `ops` alone would
    // stop one short of it forever — the relay filters its replay by this
    // number, so it would re-send that op and everything after it on every
    // reconnect, none of it ever advancing anything. The cursor means "do not
    // send me this again", and a refusal is exactly that.
    //
    // Refusals are held as one range per pair rather than one row per op (see
    // [`Engine::record_refusal`]), so the walk is: the run of ops from 1; then,
    // if the refused range begins at or before the next missing seq, over that
    // range; then the run of ops again. At most one range exists per pair, so
    // this terminates in two steps rather than looping.
    let mut prefix = ops_run_end(tx, stream_id, device_id, 1)?;
    if let Some((from, through)) = refused_range(tx, stream_id, device_id)? {
        if from <= prefix.saturating_add(1) && through > prefix {
            prefix = through;
            prefix = ops_run_end(tx, stream_id, device_id, prefix.saturating_add(1))?;
        }
    }
    tx.execute(
        "INSERT INTO sync_cursors (stream_id, device_id, last_applied_seq)
         VALUES (?, ?, ?)
         ON CONFLICT(stream_id, device_id) DO UPDATE SET
            last_applied_seq = MAX(last_applied_seq, excluded.last_applied_seq)",
        params![&stream_id[..], &device_id[..], prefix],
    )?;
    Ok(())
}

/// The stamp that decides which of two writers to a materialized row survives.
///
/// Ordering is `(hlc, device, seq)`, in that order, and every term earns its
/// place:
///
/// * `hlc` — a hybrid logical clock, not a wall clock. It is the whole point:
///   an unbounded `ts_ms` let a device with a fast clock win every conflict it
///   ever entered (issue #21), and gave no way to order two writes inside one
///   millisecond.
/// * `device` — raw 16-byte memcmp, higher wins. Breaks *cross-device* ties
///   deterministically, so every replica picks the same winner.
/// * `seq` — the writer's per-`(stream, device)` counter, already envelope
///   field 4. Reached only when two ops from the SAME device carry an equal
///   `hlc`, which the send rule makes impossible while a device's HLC state
///   lives; it becomes possible across a process restart, when the logical
///   counter resets to 0. In that window `seq` is what still orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LwwStamp {
    /// Causal timestamp from the writing device.
    pub hlc: Hlc,
    /// The writing device's 16-byte id.
    pub device: [u8; 16],
    /// The writing device's per-`(stream, device)` sequence number.
    pub seq: u64,
}

/// What `EndFocus` derives when the session says the task was finished: the
/// completed Task, its encoded `TaskUpdate` op, and the streak advance the
/// completion triggers when the task is a routine occurrence.
struct FocusCompletion {
    task: Task,
    inner: Vec<u8>,
    streak: Option<(Routine, Vec<u8>)>,
}

/// The stamp stored on a materialized row. `device` is `None` only for a
/// placeholder row that no real op has yet stamped (the lazily created
/// inbox/meta stream), which loses to everything.
#[derive(Debug, Clone)]
struct RowLww {
    hlc: Hlc,
    device: Option<Vec<u8>>,
    seq: u64,
}

/// Read the stored LWW stamp for `id` in `table`. `None` when the row is
/// absent.
fn read_row_lww(
    tx: &Transaction<'_>,
    table: &str,
    id_col: &str,
    id: &[u8; 16],
) -> rusqlite::Result<Option<RowLww>> {
    let sql = format!(
        "SELECT lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device
         FROM {table} WHERE {id_col} = ?"
    );
    tx.query_row(&sql, params![&id[..]], |r| {
        Ok(RowLww {
            hlc: Hlc {
                physical_ms: u64::try_from(r.get::<_, i64>(0)?.max(0)).unwrap_or(0),
                logical: u32::try_from(r.get::<_, i64>(1)?.max(0)).unwrap_or(u32::MAX),
            },
            device: r.get::<_, Option<Vec<u8>>>(3)?,
            seq: u64::try_from(r.get::<_, i64>(2)?.max(0)).unwrap_or(0),
        })
    })
    .optional()
}

/// Entity-level LWW decision: does the incoming stamp beat the row's stored
/// one? Compares `(hlc, device, seq)` in that order.
fn lww_wins(incoming: &LwwStamp, row: &RowLww) -> bool {
    if incoming.hlc != row.hlc {
        return incoming.hlc > row.hlc;
    }
    let Some(row_dev) = row.device.as_deref() else {
        // A placeholder row no op has stamped loses to any real op.
        return true;
    };
    if row_dev != incoming.device.as_slice() {
        return incoming.device.as_slice() > row_dev;
    }
    // Same device, same HLC. This is NOT a conflict, and the device-id memcmp
    // must NOT be applied here: `dev > dev` is false, so a device's own later
    // op would lose to its own earlier one — silently discarded on every remote
    // replica while the originating replica kept it. Permanent, undetected
    // divergence, and easy to hit before HLCs, when creating a task and
    // immediately patching it landed both ops in one millisecond.
    //
    // Ops from one device are causally ordered by their per-(stream, device)
    // `seq`, so the higher `seq` is the newer state and wins. Equal `seq` on the
    // same device is the same op re-delivered; the caller's idempotence gate has
    // already handled that, and `true` keeps a replay a harmless no-op rewrite.
    //
    // See `docs/05-sync/conflict-resolution.md` and ADR-0016.
    incoming.seq >= row.seq
}

/// Apply one decoded remote inner op to the materialized tables under LWW.
///
/// Compares the envelope's `(hlc, device, seq)` stamp against the target row's
/// stored one. If the envelope wins (or the row is absent for a
/// create/update), it performs the same insert/update the local path does and
/// stamps the LWW columns from the envelope. A losing op is a no-op here (it is
/// still recorded in the op log by the caller).
///
/// `*Delete` ops are full-state like every other op: each carries its entity
/// with `deleted` set, so a winning delete replaces the whole row and a delete
/// that overtakes its own create still materializes the tombstone (see
/// [`crate::inner_op`] docs and ADR-0014).
//
// Two arms below have empty bodies and are deliberately not merged: the
// append-only families and the control families are handled by different
// guards earlier in the function, and spelling both out is what makes adding a
// fifth family a compile-time decision rather than a silent fall-through.
/// Re-point a pre-0017 Inbox reference — the sixteen zero bytes the Inbox once
/// shared with the vault-meta stream — at [`INBOX_STREAM_BYTES`].
///
/// Migration 0017 does exactly this to the local tables of a vault that
/// upgrades. It cannot do it to the already-signed envelopes in `ops`, which
/// still name the old id and cannot be rewritten without breaking their
/// signatures. That is fine on the device that migrated, and not fine on a
/// device paired *after* the upgrade: that device receives the legacy
/// `([0u8; 16], 1)` key in its pairing payload, replays the whole history, and
/// materializes those tasks under the id in the payload — which is the
/// vault-meta stream id. `materialize_remote` would then `ensure_stream_row`
/// the one stream that must never have a `streams` row, and the two replicas of
/// one account would disagree about where the user's oldest tasks live.
///
/// Remapping here rather than refusing the ops is deliberate. Refusing would
/// drop the user's pre-0017 Inbox on the new device while the migrated device
/// kept it — permanent, silent divergence, and a data loss the user did not ask
/// for. Remapping reproduces the migration's own rewrite at the only other
/// place the same rows can be born, so both replicas land on the same state.
///
/// Only *entity* payloads are touched. A `key_envelope` naming the vault-meta
/// stream at `[0u8; 16]` is naming it correctly and is left alone.
///
/// Delete at 1.0, with the rest of the 0017 legacy path.
fn remap_legacy_inbox(inner: &mut InnerOp) {
    fn fix(r: &mut EntityRef) {
        if r.bytes() == &META_STREAM {
            *r = EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES);
        }
    }
    match inner {
        InnerOp::TaskCreate(t) | InnerOp::TaskUpdate(t) | InnerOp::TaskDelete(t) => {
            fix(&mut t.stream_id);
        }
        InnerOp::RoutineCreate(r) | InnerOp::RoutineUpdate(r) | InnerOp::RoutineDelete(r) => {
            fix(&mut r.template.stream_id);
        }
        InnerOp::BlockCreate(b) | InnerOp::BlockUpdate(b) | InnerOp::BlockDelete(b) => {
            fix(&mut b.stream_id);
        }
        InnerOp::FocusStart(f) => fix(&mut f.stream_id),
        _ => {}
    }
}

#[allow(clippy::match_same_arms)]
fn materialize_remote(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    // A control op has no entity and must never reach the kind table below,
    // whose `_ =>` arm would file anything it does not recognise under
    // `tasks`. `apply_remote` routes them away before this is called; this is
    // the belt to that braces, and it is a `debug_assert` rather than a silent
    // return so a routing mistake surfaces in a test run instead of as a
    // mysterious task row in production.
    if inner.is_control() {
        debug_assert!(false, "a control op reached the entity materializer");
        return Ok(());
    }
    let ts_ms = lww.hlc.physical_ms;
    // Focus ops never enter the LWW contest. Each writes one immutable record
    // keyed by the session's own id (start, end, and interruption in three
    // distinct tables), so there is nothing for a later op to overwrite and
    // nothing for an earlier one to lose — including when an `end` overtakes
    // its own `start`. Running an LWW comparison here would be actively wrong:
    // a `start` stamped later than its `end` would suppress the `end`.
    if matches!(
        inner,
        InnerOp::FocusStart(_) | InnerOp::FocusEnd(_) | InnerOp::FocusInterrupt(_)
    ) {
        return materialize_focus_remote(tx, inner, lww);
    }
    // A review snapshot is append-only for the same reason: it is keyed by its
    // own `rvw_` id, written once, and never edited. Running LWW here would let
    // one device's review of a week suppress another device's, which is exactly
    // the loss the representation exists to prevent. It also deliberately does
    // NOT `ensure_stream_row` for the Streams it names — the counts live inside
    // an opaque blob, so a snapshot that overtakes a `stream.create` still
    // lands intact.
    if let InnerOp::ReviewSnapshotCreate(snapshot) = inner {
        return insert_review_snapshot_row(tx, snapshot, lww);
    }
    let (table, id_col) = match inner.entity_kind() {
        EntityKind::Stream => ("streams", "stream_id"),
        EntityKind::Context => ("contexts", "id"),
        EntityKind::Routine => ("routines", "id"),
        EntityKind::Block => ("blocks", "id"),
        EntityKind::Attachment => ("attachments", "id"),
        // Task (and any future kind) key on `id`.
        _ => ("tasks", "id"),
    };
    let target = inner.target_ref();
    let existing = read_row_lww(tx, table, id_col, target.bytes())?;
    let wins = existing.as_ref().is_none_or(|row| lww_wins(lww, row));
    if !wins {
        return Ok(());
    }
    let present = existing.is_some();
    match inner {
        InnerOp::TaskCreate(t) | InnerOp::TaskUpdate(t) => {
            // The owning stream must exist before the task (FK).
            ensure_stream_row(tx, &t.stream_id, ts_ms, None, false, false, false)?;
            if present {
                update_task_row(tx, t, lww)?;
                replace_task_contexts(tx, t)?;
            } else {
                insert_task_row(tx, t, lww)?;
                insert_task_contexts(tx, t)?;
            }
            // Dependency edges ride along with the full-state task op. They are
            // written even when the blockers they name have not arrived on this
            // replica yet — an unknown blocker reads as still-open, so the
            // dependent shows blocked until its blocker turns up, whichever
            // order the two ops land in.
            replace_task_blockers(tx, t)?;
            ftsr_upsert_task(tx, t)?;
        }
        InnerOp::TaskDelete(t) => {
            // Applied exactly like TaskUpdate, because that is what it now is:
            // a full-state op whose state happens to have `deleted` set. The
            // whole row is replaced, so two replicas that had diverged on
            // `title` before the delete converge on the deleting replica's
            // state rather than each keeping its own.
            ensure_stream_row(tx, &t.stream_id, ts_ms, None, false, false, false)?;
            if present {
                update_task_row(tx, t, lww)?;
                replace_task_contexts(tx, t)?;
            } else {
                insert_task_row(tx, t, lww)?;
                insert_task_contexts(tx, t)?;
            }
            replace_task_blockers(tx, t)?;
            // Not `ftsr_upsert_task`: a tombstoned task must leave the search
            // index, and `update_task_row` does not touch it.
            ftsr_delete_task(tx, &t.id)?;
        }
        InnerOp::StreamCreate(s) | InnerOp::StreamUpdate(s) => {
            if present {
                update_stream_row(tx, s, lww)?;
            } else {
                insert_stream_row(tx, s, lww)?;
            }
        }
        InnerOp::StreamDelete(s) => {
            // Applied as the full-state op it now is, so the whole row is
            // replaced rather than only its tombstone flag.
            if present {
                update_stream_row(tx, s, lww)?;
            } else {
                insert_stream_row(tx, s, lww)?;
            }
        }
        InnerOp::ContextCreate(c) | InnerOp::ContextUpdate(c) => {
            if present {
                update_context_row(tx, c, lww)?;
            } else {
                insert_context_row(tx, c, lww)?;
            }
        }
        InnerOp::ContextDelete(c) => {
            // The membership purge is keyed on the context id alone and is
            // idempotent, so it runs even when this replica has not yet
            // materialized the Context row itself (a delete that overtook its
            // create). That keeps "deleting a Context removes it from all
            // Tasks" true on every replica that sees the delete.
            purge_context_from_tasks(tx, target.bytes())?;
            // Full-state, like every other delete: insert when this replica has
            // not materialized the Context yet. Without the `else` the delete
            // was dropped on arrival, the older create then landed live, and
            // the replica sat permanently out of step with the one that
            // deleted — the purge above ran, so the memberships were stripped
            // while the Context itself stayed alive, which is worse than
            // either outcome alone.
            if present {
                update_context_row(tx, c, lww)?;
            } else {
                insert_context_row(tx, c, lww)?;
            }
        }
        InnerOp::RoutineCreate(r) | InnerOp::RoutineUpdate(r) => {
            ensure_stream_row(tx, &r.template.stream_id, ts_ms, None, false, false, false)?;
            if present {
                update_routine_row(tx, r, lww)?;
            } else {
                insert_routine_row(tx, r, ts_ms, lww)?;
            }
        }
        InnerOp::RoutineDelete(rt) => {
            if present {
                update_routine_row(tx, rt, lww)?;
            } else {
                insert_routine_row(tx, rt, ts_ms, lww)?;
            }
        }
        // The delete rides the same arm as create and update: it is a
        // full-state op now, so the whole row is replaced rather than only its
        // tombstone flag. That also makes a delete that overtakes its create
        // land as a tombstoned row instead of vanishing.
        InnerOp::BlockCreate(b) | InnerOp::BlockUpdate(b) | InnerOp::BlockDelete(b) => {
            ensure_stream_row(tx, &b.stream_id, ts_ms, None, false, false, false)?;
            upsert_block_row(tx, b, lww)?;
            // Bindings ride along with the full-state Block op, and are written
            // even for Tasks this replica has not materialized yet: a binding
            // to an unknown Task is a fact, and `Task.blocks` picks it up the
            // moment that Task's own op lands.
            replace_block_tasks(tx, b)?;
        }
        // Create and delete share an arm for the same reason Block's do: the
        // delete carries the attachment's full state, so it replaces the row.
        // Deliberately no `ensure` of the parent task: an attachment op that
        // overtook its task's create still lands, and the two join up when the
        // task turns up. Nothing about the row depends on the parent existing.
        InnerOp::AttachmentCreate(a) | InnerOp::AttachmentDelete(a) => {
            upsert_attachment_row(tx, a, lww)?;
        }
        // Handled by the append-only branch at the top of this function; the
        // arm exists so a new append-only op cannot be added without deciding
        // here.
        InnerOp::FocusStart(_)
        | InnerOp::FocusEnd(_)
        | InnerOp::FocusInterrupt(_)
        | InnerOp::ReviewSnapshotCreate(_) => {}
        // Unreachable: the guard at the top of this function returns before
        // the LWW read. Spelled out rather than caught by a `_ =>` arm so a
        // fourth control family cannot be added without being considered here.
        InnerOp::KeyEnvelope(_) | InnerOp::DeviceRevoke(_) | InnerOp::DeviceCertPublish(_) => {}
    }
    Ok(())
}

/// Materialize one focus op. Every write is an insert that ignores a conflict
/// on its own primary key, which is what makes the whole family idempotent
/// under re-delivery and order-independent between `start` and `end`.
fn materialize_focus_remote(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let ts_ms = lww.hlc.physical_ms;
    match inner {
        InnerOp::FocusStart(f) => {
            // The owning stream row must exist (the session's stream is also
            // its op-log routing stream).
            ensure_stream_row(tx, &f.stream_id, ts_ms, None, false, false, false)?;
            insert_focus_start_row(tx, f, lww)
        }
        InnerOp::FocusEnd(f) => {
            // Deliberately no `ensure` of the start row: an `end` that arrived
            // first still lands, and the two join up when the start turns up.
            insert_focus_end_row(tx, f, lww)?;
            for i in &f.interruptions {
                insert_interruption_row(tx, i)?;
            }
            Ok(())
        }
        InnerOp::FocusInterrupt(i) => insert_interruption_row(tx, i),
        _ => Ok(()),
    }
}

/// Write the `start` record. Idempotent on the session id.
fn insert_focus_start_row(
    tx: &Transaction<'_>,
    f: &FocusStart,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&f.unknown)?;
    tx.execute(
        "INSERT OR IGNORE INTO focus_sessions
         (id, task_id, stream_id, started_at_ms, planned_ms, energy, kind,
          chunk_index, chunk_total, extra, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &f.id.bytes()[..],
            &f.task_id.bytes()[..],
            &f.stream_id.bytes()[..],
            f.started_at_ms() as i64,
            f.planned_ms.map(|v| v as i64),
            f.energy.map(energy_str),
            f.kind.as_str(),
            f.chunk.map(|c| i64::from(c.index)),
            f.chunk.map(|c| i64::from(c.total)),
            extra_blob,
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// Write the `end` record. Idempotent on the session id; a second `end` for the
/// same session (two devices closing one session concurrently) keeps the first
/// to arrive rather than letting a clock skew rewrite a frozen measurement.
fn insert_focus_end_row(
    tx: &Transaction<'_>,
    f: &FocusEnd,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&f.unknown)?;
    tx.execute(
        "INSERT OR IGNORE INTO focus_session_ends
         (session_id, ended_at_ms, actual_focused_ms, completed_task, extra,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &f.session_id.bytes()[..],
            f.ended_at_ms() as i64,
            f.actual_focused_ms as i64,
            i64::from(f.completed_task),
            extra_blob,
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// Add one interruption to the grow-only set.
fn insert_interruption_row(tx: &Transaction<'_>, i: &Interruption) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO focus_interruptions (session_id, at_ms, reason)
         VALUES (?, ?, ?)",
        params![
            &i.session_id.bytes()[..],
            i.at_ms() as i64,
            i.reason.as_str(),
        ],
    )?;
    Ok(())
}

/// How many **work** sessions a Task has already had — the input that makes a
/// chunk marker read "3 of 4" instead of always "1 of 4".
fn count_work_sessions(conn: &rusqlite::Connection, task: &[u8; 16]) -> Result<u32, EngineError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM focus_sessions WHERE task_id = ? AND kind = 'work'",
        params![&task[..]],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(n.max(0)).unwrap_or(u32::MAX))
}

/// The same count for every task at once, for the planner.
fn work_session_counts(
    conn: &rusqlite::Connection,
) -> Result<BTreeMap<EntityRef, u32>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT task_id, COUNT(*) FROM focus_sessions WHERE kind = 'work' GROUP BY task_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(raw, n)| {
            (
                ref_of(EntityKind::Task, &raw),
                u32::try_from(n.max(0)).unwrap_or(u32::MAX),
            )
        })
        .collect())
}

/// Every interruption in the vault, grouped by session.
fn read_all_interruptions(
    conn: &rusqlite::Connection,
) -> Result<BTreeMap<EntityRef, Vec<Interruption>>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT session_id, at_ms, reason FROM focus_interruptions
         ORDER BY session_id ASC, at_ms ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: BTreeMap<EntityRef, Vec<Interruption>> = BTreeMap::new();
    for (raw, at, reason) in rows {
        let session_id = ref_of(EntityKind::FocusSession, &raw);
        out.entry(session_id).or_default().push(Interruption {
            session_id,
            at: sunrise_domain::epoch_ms::from_u64(u64::try_from(at.max(0)).unwrap_or(0)),
            reason: InterruptionReason::from_str_lossy(&reason),
        });
    }
    Ok(out)
}

/// The `start` record alone, without assembling the whole session view.
fn read_focus_start(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<FocusStart>, EngineError> {
    Ok(
        read_focus_sessions(conn, "WHERE s.id = ?1", rusqlite::params![&id[..]])?
            .pop()
            .map(|v| v.start),
    )
}

/// One assembled session view (start + optional end + interruptions).
fn read_focus_session(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<FocusSession>, EngineError> {
    Ok(read_focus_sessions(conn, "WHERE s.id = ?1", rusqlite::params![&id[..]])?.pop())
}

/// Assemble session views from the two append-only tables.
///
/// The `LEFT JOIN` is the mechanism behind "a dangling start is a valid state":
/// no `end` row simply means the session is still running, and no repair pass,
/// tombstone, or synthesized value is involved.
fn read_focus_sessions(
    conn: &rusqlite::Connection,
    tail_sql: &str,
    p: &[&dyn rusqlite::ToSql],
) -> Result<Vec<FocusSession>, EngineError> {
    let sql = format!(
        "SELECT s.id, s.task_id, s.stream_id, s.started_at_ms, s.planned_ms, s.energy, s.kind,
                s.chunk_index, s.chunk_total, s.extra,
                e.ended_at_ms, e.actual_focused_ms, e.completed_task, e.extra
         FROM focus_sessions s
           LEFT JOIN focus_session_ends e ON e.session_id = s.id
         {tail_sql}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(p, |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<Vec<u8>>>(9)?,
                r.get::<_, Option<i64>>(10)?,
                r.get::<_, Option<i64>>(11)?,
                r.get::<_, Option<i64>>(12)?,
                r.get::<_, Option<Vec<u8>>>(13)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let interruptions = read_all_interruptions(conn)?;
    let mut out = Vec::with_capacity(rows.len());
    for v in rows {
        let id = ref_of(EntityKind::FocusSession, &v.0);
        let chunk = match (v.7, v.8) {
            (Some(index), Some(total)) => Some(Chunk {
                index: u32::try_from(index.max(0)).unwrap_or(0),
                total: u32::try_from(total.max(0)).unwrap_or(0),
            }),
            _ => None,
        };
        let start = FocusStart {
            id,
            task_id: ref_of(EntityKind::Task, &v.1),
            stream_id: ref_of(EntityKind::Stream, &v.2),
            started_at: sunrise_domain::epoch_ms::from_u64(u64::try_from(v.3.max(0)).unwrap_or(0)),
            planned_ms: v.4.and_then(|m| u64::try_from(m.max(0)).ok()),
            energy: v.5.as_deref().and_then(parse_energy),
            kind: FocusKind::from_str_opt(&v.6).unwrap_or(FocusKind::Work),
            chunk,
            unknown: decode_unknowns(v.9),
        };
        let mine = interruptions.get(&id).cloned().unwrap_or_default();
        let end = v.10.map(|ended| FocusEnd {
            session_id: id,
            ended_at: sunrise_domain::epoch_ms::from_u64(u64::try_from(ended.max(0)).unwrap_or(0)),
            actual_focused_ms: v.11.and_then(|m| u64::try_from(m.max(0)).ok()).unwrap_or(0),
            interruptions: mine.clone(),
            completed_task: v.12.unwrap_or(0) != 0,
            unknown: decode_unknowns(v.13),
        });
        out.push(FocusSession {
            start,
            end,
            interruptions: mine,
        });
    }
    Ok(out)
}

/// The dependency-graph walk shared by [`Query::Actionable`] and
/// [`Query::FocusPlan`]. Returns `(task id, open blockers, unblocks)`.
///
/// `open_blockers` counts an *unknown* blocker (LEFT JOIN miss) as open: the op
/// that creates it may simply not have arrived yet, and treating it as
/// satisfied would flash the task as actionable and then take it away again.
///
/// `actionable_only` pushes the planner's first criterion into SQL, so a vault
/// full of blocked work cannot crowd the scan cap with rows the planner would
/// throw away anyway.
fn actionable_scan(
    conn: &rusqlite::Connection,
    stream: Option<EntityRef>,
    limit: u32,
    actionable_only: bool,
) -> Result<Vec<([u8; 16], u32, u32)>, EngineError> {
    let stream_blob: Option<Vec<u8>> = stream.map(|s| s.bytes().to_vec());
    let mut stmt = conn.prepare(
        "SELECT id, open_blockers, unblocks FROM (
             SELECT t.id AS id,
                    (SELECT COUNT(*) FROM task_blockers tb
                       LEFT JOIN tasks b ON b.id = tb.blocker_id
                      WHERE tb.task_id = t.id
                        AND (b.id IS NULL
                             OR (b.deleted = 0
                                 AND b.state NOT IN ('done', 'cancelled')))) AS open_blockers,
                    (SELECT COUNT(*) FROM task_blockers tb2
                       JOIN tasks d ON d.id = tb2.task_id
                      WHERE tb2.blocker_id = t.id
                        AND d.deleted = 0 AND d.archived = 0
                        AND d.state NOT IN ('done', 'cancelled')) AS unblocks
             FROM tasks t
             WHERE t.deleted = 0 AND t.archived = 0
               AND t.state IN ('todo', 'in_progress')
               AND (?1 IS NULL OR t.stream_id = ?1)
         )
         WHERE (?3 = 0 OR open_blockers = 0)
         ORDER BY open_blockers ASC, unblocks DESC, id ASC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(
            params![stream_blob, limit, i64::from(actionable_only)],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(raw, open, unblocks)| {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            (
                bytes,
                u32::try_from(open.max(0)).unwrap_or(u32::MAX),
                u32::try_from(unblocks.max(0)).unwrap_or(u32::MAX),
            )
        })
        .collect())
}

/// Widen a stored 16-byte id back into a typed [`EntityRef`].
fn ref_of(kind: EntityKind, raw: &[u8]) -> EntityRef {
    let mut bytes = [0u8; 16];
    let take = raw.len().min(16);
    bytes[..take].copy_from_slice(&raw[..take]);
    EntityRef::new(kind, bytes)
}

fn ensure_stream_row(
    tx: &Transaction<'_>,
    stream: &EntityRef,
    now_ms: u64,
    parent: Option<&EntityRef>,
    archived: bool,
    deleted: bool,
    overwrite: bool,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = stream.bytes().to_vec();
    let exists: i64 = tx.query_row(
        "SELECT count(*) FROM streams WHERE stream_id = ?",
        params![&id_blob],
        |r| r.get(0),
    )?;
    if exists > 0 && !overwrite {
        return Ok(());
    }
    if exists > 0 {
        return Ok(());
    }
    let parent_blob: Option<Vec<u8>> = parent.map(|p| p.bytes().to_vec());
    tx.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms)
         VALUES (?, ?, 0, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            vec![0u8; 32],
            parent_blob,
            archived as i64,
            deleted as i64,
            now_ms,
            now_ms,
        ],
    )?;
    Ok(())
}

/// The largest `sort_order` among live streams, or `None` for an empty vault.
///
/// Restricted to keys this build can compute against — `A`..=`Z` and nothing
/// else. Two kinds of row would otherwise poison every subsequent create:
/// a placeholder from [`ensure_stream_row`], which holds the `''` sentinel,
/// and a key from a peer running a schema this build does not model. Neither
/// admits a key after it, so neither is allowed to be the anchor; the new
/// stream lands after the last key that *is* well-formed instead of failing.
fn last_stream_sort_order(conn: &rusqlite::Connection) -> Result<Option<String>, EngineError> {
    let key: Option<String> = conn.query_row(
        "SELECT MAX(sort_order) FROM streams
         WHERE deleted = 0
           AND sort_order GLOB '[A-Z]*'
           AND sort_order NOT GLOB '*[^A-Z]*'",
        [],
        |r| r.get(0),
    )?;
    Ok(key)
}

fn insert_stream_row(tx: &Transaction<'_>, s: &Stream, lww: &LwwStamp) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    let extra_blob = encode_unknowns(&s.unknown)?;
    let description_blob: Option<Vec<u8>> = s.description.as_ref().map(|b| b.0.clone());
    let default_ctx_blob: Option<Vec<u8>> = s.default_context.map(|c| c.bytes().to_vec());
    tx.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms, name, color, icon,
          paused, paused_until_ms, review_cadence, reminder_lead_s, sort_order, extra,
          description, default_context,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            vec![0u8; 32],
            parent_blob,
            s.archived as i64,
            s.deleted as i64,
            s.created_at.as_millisecond(),
            s.updated_at.as_millisecond(),
            s.name,
            s.color.as_str(),
            s.icon,
            s.paused as i64,
            s.paused_until.map(|t| t.as_millisecond()),
            cadence_str(s.review_cadence),
            s.reminder_lead_s,
            s.sort_order,
            extra_blob,
            description_blob,
            default_ctx_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

fn update_stream_row(tx: &Transaction<'_>, s: &Stream, lww: &LwwStamp) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    let extra_blob = encode_unknowns(&s.unknown)?;
    let description_blob: Option<Vec<u8>> = s.description.as_ref().map(|b| b.0.clone());
    let default_ctx_blob: Option<Vec<u8>> = s.default_context.map(|c| c.bytes().to_vec());
    tx.execute(
        "UPDATE streams
         SET parent_id = ?, archived = ?, deleted = ?, updated_at_ms = ?,
             name = ?, color = ?, icon = ?, paused = ?, paused_until_ms = ?,
             review_cadence = ?, reminder_lead_s = ?, sort_order = ?, extra = ?,
             description = ?, default_context = ?,
             lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE stream_id = ?",
        params![
            parent_blob,
            s.archived as i64,
            s.deleted as i64,
            s.updated_at.as_millisecond(),
            s.name,
            s.color.as_str(),
            s.icon,
            s.paused as i64,
            s.paused_until.map(|t| t.as_millisecond()),
            cadence_str(s.review_cadence),
            s.reminder_lead_s,
            s.sort_order,
            extra_blob,
            description_blob,
            default_ctx_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
}

fn read_stream(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<Option<Stream>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let row = conn
        .query_row(
            "SELECT parent_id, archived, deleted, created_at_ms, updated_at_ms, name, color,
                    paused, paused_until_ms, review_cadence, icon, reminder_lead_s, sort_order,
                    extra, description, default_context
             FROM streams WHERE stream_id = ?",
            params![id_blob],
            |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<u32>>(11)?,
                    r.get::<_, String>(12)?,
                    r.get::<_, Option<Vec<u8>>>(13)?,
                    r.get::<_, Option<Vec<u8>>>(14)?,
                    r.get::<_, Option<Vec<u8>>>(15)?,
                ))
            },
        )
        .optional()?;
    let Some((
        parent_raw,
        archived,
        deleted,
        created_ms,
        updated_ms,
        name,
        color_str,
        paused,
        paused_until_ms,
        cadence_str,
        icon,
        reminder_lead_s,
        sort_order,
        extra,
        description_raw,
        default_ctx_raw,
    )) = row
    else {
        return Ok(None);
    };
    let parent = parent_raw.map(|b| {
        let mut a = [0u8; 16];
        let take = b.len().min(16);
        a[..take].copy_from_slice(&b[..take]);
        EntityRef::new(EntityKind::Stream, a)
    });
    let stream = Stream {
        reminder_lead_s,
        id: EntityRef::new(EntityKind::Stream, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        name,
        description: description_raw.map(sunrise_domain::NoteBody),
        // Unknown/forward-compatible color strings fall back to Slate.
        color: StreamColor::from_str_lossy(&color_str),
        icon,
        parent_id: parent,
        sort_order,
        archived: archived != 0,
        paused: paused != 0,
        paused_until: paused_until_ms.map(ms_to_ts),
        review_cadence: parse_cadence(&cadence_str),
        default_context: default_ctx_raw.map(|b| {
            let mut a = [0u8; 16];
            let take = b.len().min(16);
            a[..take].copy_from_slice(&b[..take]);
            EntityRef::new(EntityKind::Context, a)
        }),
        deleted: deleted != 0,
        unknown: decode_unknowns(extra),
    };
    Ok(Some(stream))
}

// ---- context table operations ----

fn insert_context_row(tx: &Transaction<'_>, c: &Context, lww: &LwwStamp) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&c.unknown)?;
    tx.execute(
        "INSERT INTO contexts
         (id, name, description, archived, deleted, created_at_ms, updated_at_ms, extra,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            c.id.bytes().to_vec(),
            c.name,
            c.description,
            c.archived as i64,
            c.deleted as i64,
            c.created_at.as_millisecond(),
            c.updated_at.as_millisecond(),
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

fn update_context_row(tx: &Transaction<'_>, c: &Context, lww: &LwwStamp) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&c.unknown)?;
    tx.execute(
        "UPDATE contexts
         SET name = ?, description = ?, archived = ?, deleted = ?, updated_at_ms = ?,
             extra = ?,
             lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            c.name,
            c.description,
            c.archived as i64,
            c.deleted as i64,
            c.updated_at.as_millisecond(),
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            c.id.bytes().to_vec(),
        ],
    )?;
    Ok(())
}

/// Drop `ctx` from every Task carrying it and refresh those Tasks' FTS rows.
/// Returns the number of memberships removed. Idempotent.
fn purge_context_from_tasks(tx: &Transaction<'_>, ctx: &[u8; 16]) -> rusqlite::Result<usize> {
    let task_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare("SELECT task_id FROM task_contexts WHERE context_id = ?")?;
        let rows = stmt.query_map(params![&ctx[..]], |r| r.get::<_, Vec<u8>>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute(
        "DELETE FROM task_contexts WHERE context_id = ?",
        params![&ctx[..]],
    )?;
    for tid in &task_ids {
        refresh_task_contexts_fts(tx, tid)?;
    }
    Ok(task_ids.len())
}

/// Rewrite a task's `search_idx.contexts` column from its surviving
/// `task_contexts` rows, so a purged context stops matching search.
fn refresh_task_contexts_fts(tx: &Transaction<'_>, task_id: &[u8]) -> rusqlite::Result<()> {
    let joined = {
        let mut stmt = tx.prepare(
            "SELECT context_id FROM task_contexts WHERE task_id = ? ORDER BY context_id",
        )?;
        let rows = stmt.query_map(params![task_id], |r| r.get::<_, Vec<u8>>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .map(|raw| {
                let mut a = [0u8; 16];
                let take = raw.len().min(16);
                a[..take].copy_from_slice(&raw[..take]);
                EntityRef::new(EntityKind::Context, a).to_string()
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    tx.execute(
        "UPDATE search_idx SET contexts = ? WHERE kind = 'task' AND id = ?",
        params![joined, task_id],
    )?;
    Ok(())
}

fn read_context(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Context>, EngineError> {
    let row = conn
        .query_row(
            "SELECT name, description, archived, deleted, created_at_ms, updated_at_ms, extra
             FROM contexts WHERE id = ?",
            params![&id[..]],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<Vec<u8>>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((name, description, archived, deleted, created_ms, updated_ms, extra)) = row else {
        return Ok(None);
    };
    Ok(Some(Context {
        id: EntityRef::new(EntityKind::Context, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        name,
        description,
        archived: archived != 0,
        deleted: deleted != 0,
        unknown: decode_unknowns(extra),
    }))
}

/// Find a live Context whose name matches `name` after
/// [`Context::normalize_name`], excluding `except`.
///
/// Comparison happens in Rust rather than via SQL `COLLATE NOCASE`, which only
/// case-folds ASCII — `@Ärger` and `@ärger` are one name to a user and must be
/// one name here too.
fn find_context_by_name(
    conn: &rusqlite::Connection,
    name: &str,
    except: Option<&[u8; 16]>,
) -> Result<Option<EntityRef>, EngineError> {
    let wanted = Context::normalize_name(name);
    let mut stmt = conn.prepare("SELECT id, name FROM contexts WHERE deleted = 0")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id_blob, existing) = row?;
        let mut a = [0u8; 16];
        let take = id_blob.len().min(16);
        a[..take].copy_from_slice(&id_blob[..take]);
        if except == Some(&a) {
            continue;
        }
        if Context::normalize_name(&existing) == wanted {
            return Ok(Some(EntityRef::new(EntityKind::Context, a)));
        }
    }
    Ok(None)
}

/// Encode an entity's preserved unknown fields for the `extra` column, or
/// `None` when there are none.
///
/// Hardcoding this column to `NULL` — which is what the three task writers did
/// until now — is how a v2 field arriving on a v1 device got dropped on the
/// floor: the entity carried it in memory for exactly one transaction, then the
/// materialized row forgot it and the next outbound op re-emitted the entity
/// without it. Every replica then converged on the truncated value.
fn encode_unknowns(u: &sunrise_domain::Unknowns) -> rusqlite::Result<Option<Vec<u8>>> {
    if u.is_empty() {
        return Ok(None);
    }
    sunrise_cbor::encode_canonical(u)
        .map(Some)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// Decode the `extra` column back into an entity's unknown map.
///
/// A blob this build cannot parse degrades to "no unknowns" rather than
/// failing the read: losing a field nobody here can interpret is bad, and
/// losing the whole Task because of it is worse.
fn decode_unknowns(blob: Option<Vec<u8>>) -> sunrise_domain::Unknowns {
    blob.and_then(|b| sunrise_cbor::decode_lenient(&b).ok())
        .unwrap_or_default()
}

/// Split an optional [`SunriseTime`] into its three storage columns:
/// `(index_ms, kind, tz)`.
///
/// The index key is what every range query and `ORDER BY` in the engine reads,
/// so adding kinds did not require touching one of them; the two sidecars are
/// what let the kind survive the round trip. See the `tasks` table comment in
/// `0013_baseline.sql`.
fn time_to_parts(t: Option<&SunriseTime>) -> (Option<i64>, Option<&'static str>, Option<String>) {
    match t {
        None => (None, None, None),
        Some(v) => {
            let (ms, kind, tz) = v.to_parts();
            (Some(ms), Some(kind), tz.map(ToOwned::to_owned))
        }
    }
}

/// Rebuild an optional [`SunriseTime`] from its three storage columns.
///
/// A row with an index key but no kind is read as an instant: that is what a
/// column written before the kind existed means, and what a build that does not
/// understand a future kind should fall back to.
fn time_from_parts(ms: Option<i64>, kind: Option<&str>, tz: Option<&str>) -> Option<SunriseTime> {
    ms.map(|ms| {
        SunriseTime::from_parts(ms, kind.unwrap_or(sunrise_domain::time::kind::INSTANT), tz)
    })
}

fn insert_task_row(tx: &Transaction<'_>, t: &Task, lww: &LwwStamp) -> rusqlite::Result<()> {
    let (sched_ms, sched_kind, sched_tz) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;

    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "INSERT INTO tasks
         (id, stream_id, title, state, priority, energy, estimated_min,
          scheduled_at_ms, scheduled_at_kind, scheduled_at_tz,
          due_at_ms, due_at_kind, due_at_tz,
          completed_at_ms, completed_at_kind, completed_at_tz, deferred_count,
          routine_id, routine_occurrence, reminder_lead_s, archived, deleted, body,
          scheduling_constraints, extra, head_root,
          created_at_ms, updated_at_ms, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL,
                 ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s.and_then(|s| {
                let m = s / 60;
                i64::try_from(m).ok()
            }),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            t.deferred_count,
            t.routine_id.as_ref().map(|r| r.bytes().to_vec()),
            t.routine_occurrence.map(|d| d.as_millisecond()),
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.created_at.as_millisecond(),
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

fn update_task_row(tx: &Transaction<'_>, t: &Task, lww: &LwwStamp) -> rusqlite::Result<()> {
    let (sched_ms, sched_kind, sched_tz) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;

    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "UPDATE tasks SET
            stream_id = ?, title = ?, state = ?, priority = ?,
            energy = ?, estimated_min = ?,
            scheduled_at_ms = ?, scheduled_at_kind = ?, scheduled_at_tz = ?,
            due_at_ms = ?, due_at_kind = ?, due_at_tz = ?,
            completed_at_ms = ?, completed_at_kind = ?, completed_at_tz = ?,
            deferred_count = ?, reminder_lead_s = ?, archived = ?, deleted = ?,
            body = ?, scheduling_constraints = ?, extra = ?,
            updated_at_ms = ?, lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s
                .and_then(|s| i64::try_from(s / 60).ok()),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            t.deferred_count,
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
}

/// Encode a scheduling-constraint list to a canonical-CBOR blob for the
/// `scheduling_constraints` column. An empty list stores `NULL`.
fn encode_constraints(list: &[ScheduleConstraint]) -> rusqlite::Result<Option<Vec<u8>>> {
    if list.is_empty() {
        return Ok(None);
    }
    let v = list.to_vec();
    let bytes = sunrise_cbor::encode_canonical(&v)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(Some(bytes))
}

/// Decode the `scheduling_constraints` column blob (NULL == empty list).
fn decode_constraints(blob: Option<Vec<u8>>) -> Result<Vec<ScheduleConstraint>, EngineError> {
    match blob {
        None => Ok(Vec::new()),
        Some(bytes) => {
            sunrise_cbor::decode_canonical(&bytes).map_err(|e| EngineError::Cbor(e.to_string()))
        }
    }
}

fn insert_task_contexts(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    for c in &t.contexts {
        tx.execute(
            "INSERT OR IGNORE INTO task_contexts (task_id, context_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), c.bytes().to_vec()],
        )?;
    }
    Ok(())
}

/// Replace a task's rows in the dependency index with its current
/// `blocked_by` set.
///
/// The index is what makes both derived directions cheap — `blocked` (forward,
/// covered by the primary key) and `blocks_others` (reverse, covered by
/// `task_blockers_by_blocker`). Edges naming a blocker this replica has not
/// materialized yet are written unchanged: see `0008_task_blockers.sql` for why
/// the table carries no foreign keys.
fn replace_task_blockers(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_blockers WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    for b in &t.blocked_by {
        tx.execute(
            "INSERT OR IGNORE INTO task_blockers (task_id, blocker_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), b.bytes().to_vec()],
        )?;
    }
    Ok(())
}

/// A task's `blocked_by` set, read back out of the dependency index.
fn read_task_blockers(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> rusqlite::Result<BTreeSet<EntityRef>> {
    let mut stmt =
        conn.prepare("SELECT blocker_id FROM task_blockers WHERE task_id = ? ORDER BY blocker_id")?;
    let rows = stmt.query_map(params![id.to_vec()], |r| r.get::<_, Vec<u8>>(0))?;
    let mut out = BTreeSet::new();
    for raw in rows {
        let raw = raw?;
        let mut a = [0u8; 16];
        let take = raw.len().min(16);
        a[..take].copy_from_slice(&raw[..take]);
        out.insert(EntityRef::new(EntityKind::Task, a));
    }
    Ok(out)
}

/// The whole dependency graph, for the local submit-time cycle check.
fn read_dependency_graph(conn: &rusqlite::Connection) -> Result<DependencyGraph, EngineError> {
    let mut stmt = conn.prepare("SELECT task_id, blocker_id FROM task_blockers")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut graph = DependencyGraph::new();
    for row in rows {
        let (task, blocker) = row?;
        graph.add_edge(task_ref(&task), task_ref(&blocker));
    }
    Ok(graph)
}

/// Widen a stored 16-byte id back into a Task [`EntityRef`].
fn task_ref(raw: &[u8]) -> EntityRef {
    let mut a = [0u8; 16];
    let take = raw.len().min(16);
    a[..take].copy_from_slice(&raw[..take]);
    EntityRef::new(EntityKind::Task, a)
}

fn replace_task_contexts(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_contexts WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    insert_task_contexts(tx, t)
}

fn ftsr_upsert_task(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    tx.execute(
        "DELETE FROM search_idx WHERE kind = 'task' AND id = ?",
        params![id_blob.clone()],
    )?;
    if t.deleted {
        return Ok(());
    }
    let body_text = t
        .body
        .as_ref()
        .map(|b| String::from_utf8_lossy(&b.0).to_string())
        .unwrap_or_default();
    let contexts_text = t
        .contexts
        .iter()
        .map(EntityRef::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    tx.execute(
        "INSERT INTO search_idx (kind, id, stream_id, title, body, contexts)
         VALUES ('task', ?, ?, ?, ?, ?)",
        params![id_blob, stream_blob, t.title, body_text, contexts_text],
    )?;
    Ok(())
}

fn ftsr_delete_task(tx: &Transaction<'_>, id: &EntityRef) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = id.bytes().to_vec();
    tx.execute(
        "DELETE FROM search_idx WHERE kind = 'task' AND id = ?",
        params![id_blob],
    )?;
    Ok(())
}

fn read_task(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<Option<Task>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let row = conn
        .query_row(
            "SELECT stream_id, title, state, priority, energy, estimated_min,
                    scheduled_at_ms, due_at_ms, completed_at_ms, deferred_count,
                    archived, deleted, body, scheduling_constraints,
                    routine_id, routine_occurrence, created_at_ms, updated_at_ms,
                    scheduled_at_kind, scheduled_at_tz,
                    due_at_kind, due_at_tz,
                    completed_at_kind, completed_at_tz, extra, reminder_lead_s
             FROM tasks WHERE id = ?",
            params![id_blob],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, Option<Vec<u8>>>(12)?,
                    r.get::<_, Option<Vec<u8>>>(13)?,
                    r.get::<_, Option<Vec<u8>>>(14)?,
                    r.get::<_, Option<i64>>(15)?,
                    r.get::<_, i64>(16)?,
                    r.get::<_, i64>(17)?,
                    r.get::<_, Option<String>>(18)?,
                    r.get::<_, Option<String>>(19)?,
                    r.get::<_, Option<String>>(20)?,
                    r.get::<_, Option<String>>(21)?,
                    r.get::<_, Option<String>>(22)?,
                    r.get::<_, Option<String>>(23)?,
                    r.get::<_, Option<Vec<u8>>>(24)?,
                    r.get::<_, Option<u32>>(25)?,
                ))
            },
        )
        .optional()?;
    let Some(t) = row else {
        return Ok(None);
    };
    let mut stream_bytes = [0u8; 16];
    let take = t.0.len().min(16);
    stream_bytes[..take].copy_from_slice(&t.0[..take]);
    let mut contexts = BTreeSet::new();
    {
        let mut stmt = conn.prepare("SELECT context_id FROM task_contexts WHERE task_id = ?")?;
        let rows = stmt.query_map(params![id_blob.clone()], |row| row.get::<_, Vec<u8>>(0))?;
        for r in rows {
            let raw = r?;
            let mut a = [0u8; 16];
            let take = raw.len().min(16);
            a[..take].copy_from_slice(&raw[..take]);
            contexts.insert(EntityRef::new(EntityKind::Context, a));
        }
    }
    let task = Task {
        reminder_lead_s: t.25,
        id: EntityRef::new(EntityKind::Task, *id),
        created_at: ms_to_ts(t.16.max(0)),
        updated_at: ms_to_ts(t.17.max(0)),
        title: t.1,
        body: t.12.map(NoteBody),
        stream_id: EntityRef::new(EntityKind::Stream, stream_bytes),
        contexts,
        state: parse_task_state(&t.2),
        priority: t.3.and_then(|v| u8::try_from(v).ok()),
        energy: t.4.as_deref().and_then(parse_energy),
        estimated_duration_s: t.5.and_then(|m| {
            let secs = m.checked_mul(60)?;
            u64::try_from(secs).ok()
        }),
        scheduled_at: time_from_parts(t.6, t.18.as_deref(), t.19.as_deref()),
        due_at: time_from_parts(t.7, t.20.as_deref(), t.21.as_deref()),
        scheduling_constraints: decode_constraints(t.13)?,
        completed_at: time_from_parts(t.8, t.22.as_deref(), t.23.as_deref()),
        deferred_count: t.9,
        // Derived from `block_tasks`, never stored on the task row: the Block
        // op is the only writer of the binding, so the symmetry
        // `docs/02-domain/time-blocks.md` asks for holds by construction and
        // cannot be lost to an entity-level LWW contest over the Task.
        blocks: read_task_blocks(conn, id)?,
        // Restored from the dependency index (migration 0008). Before it
        // existed, `blocked_by` survived only inside the op-log inner op and
        // the materialized projection always read back empty, so nothing could
        // derive `blocked` from it.
        blocked_by: read_task_blockers(conn, id)?,
        assignee: None,
        routine_id: t.14.map(|b| {
            let mut a = [0u8; 16];
            let take = b.len().min(16);
            a[..take].copy_from_slice(&b[..take]);
            EntityRef::new(EntityKind::Routine, a)
        }),
        routine_occurrence: t.15.map(|m| ms_to_ts(m.max(0))),
        archived: t.10 != 0,
        deleted: t.11 != 0,
        unknown: decode_unknowns(t.24),
    };
    Ok(Some(task))
}

// ---- routine table operations ----

/// INSERT OR IGNORE a task row (materialization dedup by deterministic id).
/// Returns `true` when a new row was actually inserted.
fn insert_task_row_or_ignore(
    tx: &Transaction<'_>,
    t: &Task,
    lww: &LwwStamp,
) -> rusqlite::Result<bool> {
    let (sched_ms, sched_kind, sched_tz) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    let changed = tx.execute(
        "INSERT OR IGNORE INTO tasks
         (id, stream_id, title, state, priority, energy, estimated_min,
          scheduled_at_ms, scheduled_at_kind, scheduled_at_tz,
          due_at_ms, due_at_kind, due_at_tz,
          completed_at_ms, completed_at_kind, completed_at_tz, deferred_count,
          routine_id, routine_occurrence, reminder_lead_s, archived, deleted, body,
          scheduling_constraints, extra, head_root,
          created_at_ms, updated_at_ms, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL,
                 ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s
                .and_then(|s| i64::try_from(s / 60).ok()),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            t.deferred_count,
            t.routine_id.as_ref().map(|r| r.bytes().to_vec()),
            t.routine_occurrence.map(|d| d.as_millisecond()),
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.created_at.as_millisecond(),
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(changed > 0)
}

/// Build a materialized routine task for `(key, at)`. `title_override` carries
/// the merge suffix when a catch-up collapses ≥2 missed occurrences.
fn build_routine_task(
    routine: &Routine,
    key: &str,
    at: jiff::Timestamp,
    title_override: Option<String>,
    now_ms: u64,
) -> Task {
    let draft = routine.template.to_draft(Some(at));
    Task {
        reminder_lead_s: draft.reminder_lead_s,
        id: occurrence_task_id(&routine.id, key),
        created_at: ms_to_ts(now_ms as i64),
        updated_at: ms_to_ts(now_ms as i64),
        title: title_override.unwrap_or(draft.title),
        body: draft.body,
        stream_id: routine.template.stream_id,
        contexts: draft.contexts.into_iter().collect(),
        state: TaskState::Todo,
        priority: draft.priority,
        energy: draft.energy,
        estimated_duration_s: draft.estimated_duration_s,
        scheduled_at: Some(at.into()),
        due_at: None,
        // Routine constraints are copied verbatim onto each occurrence.
        scheduling_constraints: routine.scheduling_constraints.clone(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: Some(routine.id),
        routine_occurrence: Some(at),
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

/// A Stream's review cadence in its storage string form (matches the serde
/// representation, so the column and the op agree).
fn cadence_str(c: StreamReviewCadence) -> &'static str {
    match c {
        StreamReviewCadence::Weekly => "weekly",
        StreamReviewCadence::Biweekly => "biweekly",
        StreamReviewCadence::Monthly => "monthly",
        StreamReviewCadence::None => "none",
    }
}

/// Parse a stored cadence. Unknown values fall back to `Weekly` so a vault
/// written by a newer binary still loads.
fn parse_cadence(s: &str) -> StreamReviewCadence {
    match s {
        "biweekly" => StreamReviewCadence::Biweekly,
        "monthly" => StreamReviewCadence::Monthly,
        "none" => StreamReviewCadence::None,
        _ => StreamReviewCadence::Weekly,
    }
}

fn catchup_policy_str(p: RoutineCatchupPolicy) -> &'static str {
    match p {
        RoutineCatchupPolicy::Skip => "skip",
        RoutineCatchupPolicy::Merge => "merge",
        RoutineCatchupPolicy::Queue => "queue",
    }
}

fn parse_catchup_policy(s: &str) -> RoutineCatchupPolicy {
    match s {
        "merge" => RoutineCatchupPolicy::Merge,
        "queue" => RoutineCatchupPolicy::Queue,
        _ => RoutineCatchupPolicy::Skip,
    }
}

/// Encode a CBOR blob for a non-empty serializable value, or `None` when empty
/// (mirrors `encode_constraints`, keeping empty lists as SQL NULL).
fn encode_blob_opt<T: serde::Serialize>(
    value: &T,
    is_empty: bool,
) -> rusqlite::Result<Option<Vec<u8>>> {
    if is_empty {
        return Ok(None);
    }
    let bytes = sunrise_cbor::encode_canonical(value)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(Some(bytes))
}

fn decode_blob_opt<T: serde::de::DeserializeOwned + serde::Serialize + Default>(
    blob: Option<Vec<u8>>,
) -> Result<T, EngineError> {
    match blob {
        None => Ok(T::default()),
        Some(bytes) => {
            sunrise_cbor::decode_canonical(&bytes).map_err(|e| EngineError::Cbor(e.to_string()))
        }
    }
}

/// The streak fields of a Routine that migration 0009 projects into the single
/// `routines.streak_state` blob.
///
/// `streak_counter` and `last_completed_at_ms` keep their own columns (0001 /
/// 0004); these five are read and written as a unit and are opaque to SQL, so
/// one canonical-CBOR blob beats five more positional columns. All-default
/// means `NULL`, which is also what every pre-v9 row upgrades into.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StreakStateProj {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grace_window_s: Option<u64>,
    #[serde(default)]
    forgiveness_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    streak_started_at_ms: Option<i64>,
    #[serde(default)]
    forgivenesses_in_window: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    streak_keys: Vec<String>,
}

impl StreakStateProj {
    fn from_routine(r: &Routine) -> Self {
        Self {
            grace_window_s: r.grace_window_s,
            forgiveness_disabled: !r.forgiveness_enabled,
            streak_started_at_ms: r.streak_started_at.map(|t| t.as_millisecond()),
            forgivenesses_in_window: r.forgivenesses_in_window,
            streak_keys: r.streak_keys.clone(),
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Encode the streak projection, storing `NULL` for the all-default state.
fn encode_streak_state(r: &Routine) -> rusqlite::Result<Option<Vec<u8>>> {
    let proj = StreakStateProj::from_routine(r);
    let empty = proj.is_default();
    encode_blob_opt(&proj, empty)
}

fn insert_routine_row(
    tx: &Transaction<'_>,
    r: &Routine,
    now_ms: u64,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    let streak_blob = encode_streak_state(r)?;
    let extra_blob = encode_unknowns(&r.unknown)?;
    tx.execute(
        "INSERT INTO routines
         (id, stream_id, rrule_text, timezone, starts_at_ms, ends_at_ms,
          streak_counter, paused, archived, deleted, scheduling_constraints,
          template, skip_dates, skipped_keys, catchup_policy,
          last_completed_at_ms, paused_until_ms, created_at_ms, updated_at_ms,
          streak_state, extra, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            rrule_text,
            r.timezone,
            r.starts_at.as_millisecond(),
            r.ends_at.map(|d| d.as_millisecond()),
            r.streak_counter,
            r.paused as i64,
            r.archived as i64,
            r.deleted as i64,
            constraints_blob,
            template_blob,
            skip_dates_blob,
            skipped_keys_blob,
            catchup_policy_str(r.catchup_policy),
            r.last_completed_at.map(|d| d.as_millisecond()),
            r.paused_until.map(|d| d.as_millisecond()),
            now_ms,
            now_ms,
            streak_blob,
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

fn update_routine_row(tx: &Transaction<'_>, r: &Routine, lww: &LwwStamp) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    let streak_blob = encode_streak_state(r)?;
    let extra_blob = encode_unknowns(&r.unknown)?;
    tx.execute(
        "UPDATE routines SET
            stream_id = ?, rrule_text = ?, timezone = ?,
            starts_at_ms = ?, ends_at_ms = ?, streak_counter = ?, paused = ?,
            archived = ?, deleted = ?, scheduling_constraints = ?, template = ?,
            skip_dates = ?, skipped_keys = ?, catchup_policy = ?,
            last_completed_at_ms = ?, paused_until_ms = ?, updated_at_ms = ?,
            streak_state = ?, extra = ?,
            lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            stream_blob,
            rrule_text,
            r.timezone,
            r.starts_at.as_millisecond(),
            r.ends_at.map(|d| d.as_millisecond()),
            r.streak_counter,
            r.paused as i64,
            r.archived as i64,
            r.deleted as i64,
            constraints_blob,
            template_blob,
            skip_dates_blob,
            skipped_keys_blob,
            catchup_policy_str(r.catchup_policy),
            r.last_completed_at.map(|d| d.as_millisecond()),
            r.paused_until.map(|d| d.as_millisecond()),
            r.updated_at.as_millisecond(),
            streak_blob,
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
}

fn read_materialized_until(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<u64, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let v: Option<i64> = conn
        .query_row(
            "SELECT materialized_until_ms FROM routines WHERE id = ?",
            params![id_blob],
            |r| r.get(0),
        )
        .optional()?;
    Ok(u64::try_from(v.unwrap_or(0).max(0)).unwrap_or(0))
}

fn set_materialized_until(tx: &Transaction<'_>, id: &[u8; 16], ms: u64) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = id.to_vec();
    tx.execute(
        "UPDATE routines SET materialized_until_ms = ? WHERE id = ?",
        params![i64::try_from(ms).unwrap_or(i64::MAX), id_blob],
    )?;
    Ok(())
}

fn routine_from_row(
    id: &[u8; 16],
    rrule_text: &str,
    timezone: String,
    starts_ms: i64,
    ends_ms: Option<i64>,
    streak: i64,
    paused: i64,
    archived: i64,
    deleted: i64,
    constraints: Option<Vec<u8>>,
    template: Option<Vec<u8>>,
    skip_dates: Option<Vec<u8>>,
    skipped_keys: Option<Vec<u8>>,
    catchup: &str,
    last_completed_ms: Option<i64>,
    paused_until_ms: Option<i64>,
    created_ms: i64,
    updated_ms: i64,
    streak_state: Option<Vec<u8>>,
    extra: Option<Vec<u8>>,
) -> Result<Routine, EngineError> {
    let rrule = sunrise_domain::RRule::parse(rrule_text)
        .map_err(|e| EngineError::Invalid(format!("stored rrule: {e}")))?;
    let template: TaskTemplate = match template {
        Some(b) => {
            sunrise_cbor::decode_canonical(&b).map_err(|e| EngineError::Cbor(e.to_string()))?
        }
        None => return Err(EngineError::Invalid("routine missing template".into())),
    };
    // NULL (every pre-v9 row, and every routine never completed) decodes to the
    // all-default streak state.
    let streak_proj: StreakStateProj = decode_blob_opt(streak_state)?;
    Ok(Routine {
        id: EntityRef::new(EntityKind::Routine, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        template,
        rrule,
        timezone,
        starts_at: ms_to_ts(starts_ms.max(0)),
        ends_at: ends_ms.map(|m| ms_to_ts(m.max(0))),
        scheduling_constraints: decode_constraints(constraints)?,
        skip_dates: decode_blob_opt(skip_dates)?,
        skipped_keys: decode_blob_opt(skipped_keys)?,
        catchup_policy: parse_catchup_policy(catchup),
        streak_counter: streak,
        last_completed_at: last_completed_ms.map(|m| ms_to_ts(m.max(0))),
        grace_window_s: streak_proj.grace_window_s,
        forgiveness_enabled: !streak_proj.forgiveness_disabled,
        streak_started_at: streak_proj.streak_started_at_ms.map(|m| ms_to_ts(m.max(0))),
        forgivenesses_in_window: streak_proj.forgivenesses_in_window,
        streak_keys: streak_proj.streak_keys,
        paused: paused != 0,
        paused_until: paused_until_ms.map(|m| ms_to_ts(m.max(0))),
        archived: archived != 0,
        deleted: deleted != 0,
        unknown: decode_unknowns(extra),
    })
}

const ROUTINE_COLUMNS: &str = "rrule_text, timezone, starts_at_ms, ends_at_ms,
     streak_counter, paused, archived, deleted, scheduling_constraints,
     template, skip_dates, skipped_keys, catchup_policy, last_completed_at_ms,
     paused_until_ms, created_at_ms, updated_at_ms, streak_state, extra";

fn read_routine(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Routine>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let sql = format!("SELECT {ROUTINE_COLUMNS} FROM routines WHERE id = ?");
    let raw = conn
        .query_row(&sql, params![id_blob], |r| {
            // Collect the columns as owned values first (query_row's closure
            // must return rusqlite::Result); decode outside.
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<Vec<u8>>>(8)?,
                r.get::<_, Option<Vec<u8>>>(9)?,
                r.get::<_, Option<Vec<u8>>>(10)?,
                r.get::<_, Option<Vec<u8>>>(11)?,
                r.get::<_, String>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<i64>>(14)?,
                r.get::<_, i64>(15)?,
                r.get::<_, i64>(16)?,
                r.get::<_, Option<Vec<u8>>>(17)?,
                r.get::<_, Option<Vec<u8>>>(18)?,
            ))
        })
        .optional()?;
    let Some(v) = raw else {
        return Ok(None);
    };
    let routine = routine_from_row(
        id, &v.0, v.1, v.2, v.3, v.4, v.5, v.6, v.7, v.8, v.9, v.10, v.11, &v.12, v.13, v.14, v.15,
        v.16, v.17, v.18,
    )?;
    Ok(Some(routine))
}

fn read_routines(conn: &rusqlite::Connection) -> Result<Vec<Routine>, EngineError> {
    let sql =
        format!("SELECT id, {ROUTINE_COLUMNS} FROM routines WHERE deleted = 0 ORDER BY id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            let id_raw: Vec<u8> = r.get(0)?;
            Ok((
                id_raw,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, Option<Vec<u8>>>(9)?,
                r.get::<_, Option<Vec<u8>>>(10)?,
                r.get::<_, Option<Vec<u8>>>(11)?,
                r.get::<_, Option<Vec<u8>>>(12)?,
                r.get::<_, String>(13)?,
                r.get::<_, Option<i64>>(14)?,
                r.get::<_, Option<i64>>(15)?,
                r.get::<_, i64>(16)?,
                r.get::<_, i64>(17)?,
                r.get::<_, Option<Vec<u8>>>(18)?,
                r.get::<_, Option<Vec<u8>>>(19)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for v in rows {
        let mut id = [0u8; 16];
        let take = v.0.len().min(16);
        id[..take].copy_from_slice(&v.0[..take]);
        out.push(routine_from_row(
            &id, &v.1, v.2, v.3, v.4, v.5, v.6, v.7, v.8, v.9, v.10, v.11, v.12, &v.13, v.14, v.15,
            v.16, v.17, v.18, v.19,
        )?);
    }
    Ok(out)
}

// ---- helpers ----

fn require_kind(r: EntityRef, k: EntityKind) -> Result<(), EngineError> {
    if r.kind() != k {
        return Err(EngineError::Invalid(format!(
            "expected {k:?}, got {:?}",
            r.kind()
        )));
    }
    Ok(())
}

fn task_state_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Todo => "todo",
        TaskState::InProgress => "in_progress",
        TaskState::Done => "done",
        TaskState::Cancelled => "cancelled",
    }
}

/// Read a stored task state.
///
/// Lossy on purpose, and delegated to the domain so the storage projection and
/// the wire decoder degrade the same way. A row written by a newer binary with
/// a state this build has never heard of reads as `todo` — the task is still
/// there and still open — rather than failing the whole read and taking every
/// query that touches it down with it.
fn parse_task_state(s: &str) -> TaskState {
    TaskState::from_str_lossy(s)
}

fn energy_str(e: sunrise_domain::Energy) -> &'static str {
    match e {
        sunrise_domain::Energy::Low => "low",
        sunrise_domain::Energy::Med => "med",
        sunrise_domain::Energy::High => "high",
    }
}

fn parse_energy(s: &str) -> Option<sunrise_domain::Energy> {
    match s {
        "low" => Some(sunrise_domain::Energy::Low),
        "med" => Some(sunrise_domain::Energy::Med),
        "high" => Some(sunrise_domain::Energy::High),
        _ => None,
    }
}

/// Turn arbitrary user input into a safe FTS5 MATCH expression.
///
/// FTS5's query grammar treats characters like `"`, `*`, `:`, `-`, `(`, `)`,
/// and bare-word operators (`AND`/`OR`/`NOT`/`NEAR`) as syntax; hostile or
/// accidental input can otherwise produce a MATCH syntax error. We defuse it
/// completely: split on whitespace, strip embedded double quotes and control
/// characters (a NUL would truncate the C string SQLite receives) from each
/// token, wrap each token in double quotes (making it a literal phrase — no
/// operator or special character survives), and join with spaces (implicit
/// AND). An empty result means "no query".
fn sanitize_fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            let cleaned: String = tok
                .chars()
                .filter(|&c| c != '"' && !c.is_control())
                .collect();
            cleaned
        })
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{tok}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ms_to_ts(ms: i64) -> jiff::Timestamp {
    // Determinism: never read a wall clock on failure. Out-of-range epoch-ms
    // (corrupt row) clamps to the Unix epoch rather than `Timestamp::now()`.
    jiff::Timestamp::from_millisecond(ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// Widen a stored blob back to a 16-byte id.
fn to16(raw: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let take = raw.len().min(16);
    out[..take].copy_from_slice(&raw[..take]);
    out
}

fn hex_short(b: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for byte in b.iter().take(4) {
        use core::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Reviews and stats (`docs/08-features/reviews-and-stats.md`)
//
// Everything in this section follows the same shape: **the query fetches, a
// pure fold in `sunrise-domain` computes**. Nothing here does arithmetic on a
// review number, which is why the review, the trends, the timeline and the
// export can never disagree with one another — they are four renderings of
// two folds.
// ---------------------------------------------------------------------------

impl Engine {
    /// Record a completed weekly review as an append-only `rvw_` record.
    fn save_review_snapshot(
        &self,
        db: &mut Db,
        d: ReviewSnapshotDraft,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let id = self.fresh_id(EntityKind::ReviewSnapshot, now_ms);
        let snapshot = ReviewSnapshot {
            id,
            created_at: ms_to_ts(now_ms as i64),
            window_start: sunrise_domain::epoch_ms::from_u64(d.window_start_ms),
            window_end: sunrise_domain::epoch_ms::from_u64(d.window_end_ms.max(d.window_start_ms)),
            totals: d.totals,
            streams: d.streams,
            streaks: d.streaks,
            note: d.note,
            unknown: Unknowns::new(),
        };
        let inner = encode_inner_op(&InnerOp::ReviewSnapshotCreate(Box::new(snapshot.clone())))?;
        let op_id = self.fresh_op_id(now_ms);
        // A review spans the whole vault, so it routes to the meta log the way
        // Stream and Routine lifecycle ops do.
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_review_snapshot_row(tx, &snapshot, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner,
                "review.snapshot",
                "review_snapshot",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// The device signing keys this vault can open envelopes with: this device
    /// plus every trusted peer, **including revoked ones**.
    ///
    /// A revoked device's past ops still happened, and a history that silently
    /// loses them the moment a laptop is de-authorized would be worse than no
    /// history. Revocation stops *future* ops being accepted
    /// ([`Self::lookup_device_cert`] filters there); it does not rewrite the
    /// record.
    fn device_signing_keys(&self, db: &Db) -> Result<BTreeMap<[u8; 16], [u8; 32]>, EngineError> {
        let mut keys = BTreeMap::new();
        keys.insert(
            self.keychain.device_id(),
            self.keychain.device_signing_pub(),
        );
        let mut stmt = db
            .conn()
            .prepare("SELECT device_id, cert_blob FROM devices")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, blob) in rows {
            let Ok(cert) = DeviceCert::from_cbor(&blob) else {
                continue;
            };
            let mut a = [0u8; 16];
            let take = id.len().min(16);
            a[..take].copy_from_slice(&id[..take]);
            keys.insert(a, cert.body.d_s_pub);
        }
        Ok(keys)
    }

    /// Decode op-log rows into the domain's [`OpRecord`] form.
    ///
    /// A row that cannot be opened (an envelope from a device whose cert this
    /// replica never stored) is **skipped, not an error**: a partial history is
    /// still a useful history, and one unreadable op must not blank a review.
    fn decode_op_records(
        &self,
        keys: &BTreeMap<[u8; 16], [u8; 32]>,
        rows: Vec<OpRow>,
    ) -> Vec<OpRecord> {
        rows.into_iter()
            .filter_map(|row| {
                let inner = self.open_logged_op(keys, &row.envelope)?;
                Some(OpRecord {
                    op_id: row.op_id,
                    at_ms: row.ts_ms,
                    device: row.device,
                    target: inner.target_ref(),
                    payload: op_payload(inner),
                })
            })
            .collect()
    }

    /// Verify + decrypt one stored envelope back to its [`InnerOp`].
    ///
    /// Unlike [`Keychain::open_op`] this accepts envelopes signed by *any*
    /// known device, which is what makes a merged vault's timeline show the
    /// other device's edits instead of only this one's.
    fn open_logged_op(
        &self,
        keys: &BTreeMap<[u8; 16], [u8; 32]>,
        envelope: &[u8],
    ) -> Option<InnerOp> {
        let env = decode_envelope(envelope).ok()?;
        let d_s_pub = keys.get(&env.device_id)?;
        let inner = self
            .keychain
            .stream_keys_at(&env.stream_id, env.epoch)
            .iter()
            .find_map(|k| open_envelope(&env, d_s_pub, Some(k)).ok())?;
        decode_inner_op(&inner).ok()
    }

    /// Every task op for the tasks touched since `since_ms`, in log order.
    ///
    /// The subselect is the whole trick: a trend needs each task's **full**
    /// history (to tell a transition from a rewrite of the same state), but it
    /// only cares about tasks something happened to inside the window. So the
    /// window picks the tasks and the outer query takes their whole history.
    fn task_history_ops(&self, db: &Db, since_ms: u64) -> Result<Vec<OpRecord>, EngineError> {
        let keys = self.device_signing_keys(db)?;
        let since = i64::try_from(since_ms).unwrap_or(i64::MAX);
        let mut stmt = db.conn().prepare(
            // Filtered on `target_kind`, not on `inner_kind`: a defer is
            // logged locally as `task.defer` but arrives from a peer as
            // `task.update` (both are a full-state `TaskUpdate`), so an
            // inner-kind list would quietly lose one device's defers.
            //
            // Ordered by `(ts_ms, device_id, seq)`, **not** by `(ts_ms,
            // op_id)`. `fold_activity` is a state machine over successive
            // full-state snapshots, so the order it reads them in *is* the
            // history it reports. `op_id` is a ULID whose low bits are random,
            // so two ops a device wrote in the same millisecond used to sort
            // arbitrarily — and a pair read backwards does not just come out
            // reordered, it fabricates a transition that never happened
            // (defer-then-complete read backwards reports a *reopen*).
            // `(device_id, seq)` is the device's own causal order, which is
            // exactly what the fold needs; `op_id` stays as a final tiebreak
            // so the ordering is still total.
            "SELECT op_id, ts_ms, device_id, envelope FROM ops
             WHERE target_kind = 'task'
               AND target_id IN (
                   SELECT DISTINCT target_id FROM ops
                   WHERE target_kind = 'task'
                     AND ts_ms >= ?1 AND target_id IS NOT NULL)
             ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC",
        )?;
        let rows = stmt
            .query_map(params![since], op_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(self.decode_op_records(&keys, rows))
    }

    /// The ops that make up one entity's activity feed.
    ///
    /// For a Task: its own ops plus the ops of every focus session recorded
    /// against it. For a Stream: its own lifecycle ops plus the ops of the
    /// Tasks that currently live in it, so the Stream's feed reads as "what
    /// happened in this stream".
    fn entity_timeline_ops(
        &self,
        db: &Db,
        entity: EntityRef,
    ) -> Result<Vec<OpRecord>, EngineError> {
        let keys = self.device_signing_keys(db)?;
        let id: Vec<u8> = entity.bytes().to_vec();
        let sql = match entity.kind() {
            EntityKind::Task => {
                "SELECT op_id, ts_ms, device_id, envelope FROM ops
                 WHERE target_id = ?1
                    OR target_id IN (SELECT id FROM focus_sessions WHERE task_id = ?1)
                 ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC"
            }
            EntityKind::Stream => {
                "SELECT op_id, ts_ms, device_id, envelope FROM ops
                 WHERE target_id = ?1
                    OR target_id IN (SELECT id FROM tasks WHERE stream_id = ?1)
                    OR target_id IN (SELECT id FROM focus_sessions WHERE stream_id = ?1)
                 ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC"
            }
            _ => {
                return Err(EngineError::Invalid(
                    "activity timelines exist for tasks and streams only".into(),
                ))
            }
        };
        let mut stmt = db.conn().prepare(sql)?;
        let rows = stmt
            .query_map(params![id], op_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(self.decode_op_records(&keys, rows))
    }

    /// The device-local week grid, anchored on Monday.
    ///
    /// The timezone comes from the injected clock — the same seam scheduling
    /// constraints use — so a week boundary is never read from ambient state.
    fn week_grid(&self, now_ms: u64, weeks: u32) -> Result<WeekGrid, EngineError> {
        let tz = jiff::tz::TimeZone::get(&self.clock.timezone()).unwrap_or(jiff::tz::TimeZone::UTC);
        WeekGrid::trailing(now_ms, weeks.clamp(1, MAX_TREND_WEEKS), &tz, Weekday::Mo)
            .map_err(|e| EngineError::Invalid(format!("week grid: {e}")))
    }

    fn query_stream_trends(
        &self,
        db: &Db,
        weeks: u32,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Trends(Box::new(
            self.trends(db, weeks, now_ms)?,
        )))
    }

    fn trends(&self, db: &Db, weeks: u32, now_ms: u64) -> Result<Trends, EngineError> {
        let grid = self.week_grid(now_ms, weeks)?;
        let ops = self.task_history_ops(db, grid.starts().first().copied().unwrap_or(0))?;
        Ok(fold_trends(&ops, &grid))
    }

    fn query_activity_timeline(
        &self,
        db: &Db,
        entity: EntityRef,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        let ops = self.entity_timeline_ops(db, entity)?;
        let events = fold_activity(&ops);
        // A Stream's feed is its own events plus its members'; a Task's feed is
        // just its own (its focus sessions are already attributed to it by the
        // fold).
        let mut subjects = BTreeSet::from([entity]);
        if entity.kind() == EntityKind::Stream {
            subjects.extend(stream_member_tasks(db.conn(), entity.bytes())?);
        }
        Ok(QueryResult::Activity(activity_for_entities(
            &events,
            &subjects,
            limit as usize,
        )))
    }

    /// Assemble the weekly review.
    fn query_weekly_review(
        &self,
        db: &Db,
        week_start_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::WeeklyReview(Box::new(self.weekly_review(
            db,
            week_start_ms,
            now_ms,
        )?)))
    }

    fn weekly_review(
        &self,
        db: &Db,
        week_start_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<WeeklyReview, EngineError> {
        let grid = self.week_grid(now_ms, TREND_WEEKS)?;
        let start = week_start_ms.unwrap_or_else(|| grid.last_start_ms());
        let end = grid
            .starts()
            .iter()
            .find(|s| **s > start)
            .copied()
            .unwrap_or_else(|| grid.end_ms());
        let window = ReviewWindow::new(start, end);

        // One op scan feeds both folds: the activity feed the review's counts
        // come from, and the trend line each Stream shows.
        let ops = self.task_history_ops(db, grid.starts().first().copied().unwrap_or(0))?;
        let mut activity = fold_activity(&ops);
        activity.extend(self.focus_activity(db, window)?);
        activity.sort_by_key(|e| (e.at_ms, e.op_id));
        let trends = fold_trends(&ops, &grid);

        let QueryResult::FocusStats(focus) =
            self.query_focus_stats(db, None, Some(window.start_ms), now_ms)?
        else {
            return Err(EngineError::Invalid("focus stats shape".into()));
        };

        let QueryResult::Streams(stream_rows) = self.query_stream_list(db)? else {
            return Err(EngineError::Invalid("stream list shape".into()));
        };
        // `StreamRow` carries no `paused` flag and is constructed in several
        // crates, so the pause state is read straight from the projection here
        // rather than widened onto a shared struct.
        let paused = paused_streams(db.conn())?;
        let streams: Vec<ReviewStream> = stream_rows
            .into_iter()
            .map(|r| ReviewStream {
                paused: paused.contains(&r.id),
                id: r.id,
                name: r.name,
                archived: r.archived,
            })
            .collect();

        let tasks = read_live_tasks(db.conn())?;
        let routines = read_routines(db.conn())?;
        let drift_window = drift_window(window, DRIFT_WINDOW_WEEKS)?;
        let drift: Vec<RoutineDrift> = routines
            .iter()
            .filter(|r| !r.deleted && !r.archived)
            .filter_map(|r| routine_drift(r, drift_window, DEFAULT_DRIFT_THRESHOLD).ok())
            .collect();

        Ok(build_weekly_review(WeeklyReviewInput {
            window,
            streams,
            tasks,
            activity,
            routines,
            drift,
            focus: *focus,
            trends,
        }))
    }

    /// Focus start/end events inside `window`, folded from the op log.
    ///
    /// Kept separate from [`Self::task_history_ops`] because the trend fold has
    /// no use for them and scanning the whole session log for a 12-week trend
    /// would be waste.
    fn focus_activity(
        &self,
        db: &Db,
        window: ReviewWindow,
    ) -> Result<Vec<ActivityEvent>, EngineError> {
        let keys = self.device_signing_keys(db)?;
        let since = i64::try_from(window.start_ms).unwrap_or(i64::MAX);
        let until = i64::try_from(window.end_ms).unwrap_or(i64::MAX);
        let mut stmt = db.conn().prepare(
            "SELECT op_id, ts_ms, device_id, envelope FROM ops
             WHERE target_kind = 'focus_session'
               AND ts_ms >= ?1 AND ts_ms < ?2
             ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC",
        )?;
        let rows = stmt
            .query_map(params![since, until], op_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(fold_activity(&self.decode_op_records(&keys, rows)))
    }

    fn query_daily_review(
        &self,
        db: &Db,
        since_ms: u64,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let QueryResult::StreamTasks(inbox) = self.query_stream_tasks(db, &inbox_stream_ref())?
        else {
            return Err(EngineError::Invalid("inbox shape".into()));
        };
        let QueryResult::Tasks(today) = self.query_today(db, now_ms, &[])? else {
            return Err(EngineError::Invalid("today shape".into()));
        };
        // The derived-blocked set, straight off the dependency index — the same
        // walk `Query::Actionable` uses, so "blocked" means one thing.
        let blocked: BTreeSet<EntityRef> = actionable_scan(db.conn(), None, u32::MAX, false)?
            .into_iter()
            .filter(|(_, open, _)| *open > 0)
            .map(|(bytes, _, _)| EntityRef::new(EntityKind::Task, bytes))
            .collect();
        Ok(QueryResult::DailyReview(Box::new(build_daily_review(
            ReviewWindow::new(since_ms, now_ms.max(since_ms)),
            inbox,
            today,
            &blocked,
        ))))
    }

    fn query_review_history(&self, db: &Db, limit: u32) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT body FROM review_snapshots
             ORDER BY window_start_ms DESC, created_at_ms DESC, id ASC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let out = rows
            .into_iter()
            .filter_map(|blob| ciborium::de::from_reader::<ReviewSnapshot, _>(&blob[..]).ok())
            .collect();
        Ok(QueryResult::ReviewSnapshots(out))
    }

    fn query_export(
        &self,
        db: &Db,
        dataset: ExportDataset,
        format: ExportFormat,
        weeks: u32,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let QueryResult::Streams(rows) = self.query_stream_list(db)? else {
            return Err(EngineError::Invalid("stream list shape".into()));
        };
        let names: BTreeMap<EntityRef, String> = rows.into_iter().map(|r| (r.id, r.name)).collect();
        let table = match dataset {
            ExportDataset::Trends => trends_table(&self.trends(db, weeks, now_ms)?, &names),
            ExportDataset::Activity => {
                let grid = self.week_grid(now_ms, weeks)?;
                let start = grid.starts().first().copied().unwrap_or(0);
                let ops = self.task_history_ops(db, start)?;
                let mut events = fold_activity(&ops);
                events.extend(self.focus_activity(db, ReviewWindow::new(start, grid.end_ms()))?);
                events.retain(|e| e.at_ms >= start);
                events.sort_by_key(|e| (e.at_ms, e.op_id));
                activity_table(&events)
            }
            ExportDataset::Focus => {
                let grid = self.week_grid(now_ms, weeks)?;
                let QueryResult::FocusStats(stats) =
                    self.query_focus_stats(db, None, grid.starts().first().copied(), now_ms)?
                else {
                    return Err(EngineError::Invalid("focus stats shape".into()));
                };
                focus_table(&stats, &names)
            }
            ExportDataset::Streaks => {
                let rows: Vec<StreakRow> = read_routines(db.conn())?
                    .into_iter()
                    .filter(|r| !r.deleted && !r.archived)
                    .map(|r| StreakRow {
                        routine: r.id,
                        title: r.template.title,
                        streak: r.streak_counter,
                        last_completed_at_ms: r
                            .last_completed_at
                            .and_then(|t| u64::try_from(t.as_millisecond()).ok()),
                    })
                    .collect();
                streaks_table(&rows)
            }
        };
        Ok(QueryResult::Export(table.render(format)))
    }
}

/// One op-log row as the history readers see it.
#[derive(Debug)]
struct OpRow {
    op_id: [u8; 16],
    ts_ms: u64,
    device: [u8; 16],
    envelope: Vec<u8>,
}

/// `rusqlite` row mapper for [`OpRow`].
fn op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OpRow> {
    Ok(OpRow {
        op_id: blob16(&r.get::<_, Vec<u8>>(0)?),
        ts_ms: u64::try_from(r.get::<_, i64>(1)?.max(0)).unwrap_or(0),
        device: blob16(&r.get::<_, Vec<u8>>(2)?),
        envelope: r.get::<_, Vec<u8>>(3)?,
    })
}

/// Left-pad / truncate a stored blob into a 16-byte id.
fn blob16(raw: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    let take = raw.len().min(16);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

/// Project an [`InnerOp`] onto the timeline's payload vocabulary.
///
/// Everything the spec's exclusion list names — routine bookkeeping, context
/// edits, interruption logs, snapshot ops — lands on `Ignored`, so the filter
/// lives in one exhaustive `match` that a new op variant cannot slip past.
fn op_payload(inner: InnerOp) -> OpPayload {
    match inner {
        InnerOp::TaskCreate(t) => OpPayload::TaskCreated(Box::new(t)),
        InnerOp::TaskUpdate(t) => OpPayload::TaskUpdated(Box::new(t)),
        InnerOp::TaskDelete(_) => OpPayload::TaskDeleted,
        InnerOp::StreamCreate(s) => OpPayload::StreamCreated(Box::new(s)),
        InnerOp::StreamDelete(_) => OpPayload::StreamDeleted,
        InnerOp::FocusStart(f) => OpPayload::FocusStarted(f),
        InnerOp::FocusEnd(f) => OpPayload::FocusEnded(f),
        InnerOp::StreamUpdate(_)
        | InnerOp::ContextCreate(_)
        | InnerOp::ContextUpdate(_)
        | InnerOp::ContextDelete(_)
        | InnerOp::RoutineCreate(_)
        | InnerOp::RoutineUpdate(_)
        | InnerOp::RoutineDelete(_)
        // Calendar bookkeeping. A Block records WHEN work is planned, never
        // that any happened, so it does not belong in a Task's or a Stream's
        // activity feed.
        | InnerOp::BlockCreate(_)
        | InnerOp::BlockUpdate(_)
        | InnerOp::BlockDelete(_)
        // Attachment metadata. The timeline is about what happened to the
        // work, and `docs/08-features/reviews-and-stats.md` keeps bookkeeping
        // ops out of it.
        | InnerOp::AttachmentCreate(_)
        | InnerOp::AttachmentDelete(_)
        | InnerOp::FocusInterrupt(_)
        // Control ops carry key material and trust, not work. A rotation is
        // not something that happened to a Task.
        | InnerOp::KeyEnvelope(_)
        | InnerOp::DeviceRevoke(_)
        | InnerOp::DeviceCertPublish(_)
        | InnerOp::ReviewSnapshotCreate(_) => OpPayload::Ignored,
    }
}

/// The routine-tuning window: the `weeks` civil weeks ending with the reviewed
/// one. Approximated in whole weeks from the review window, which is exact
/// because a review window *is* one grid week.
fn drift_window(
    window: ReviewWindow,
    weeks: u32,
) -> Result<(jiff::Timestamp, jiff::Timestamp), EngineError> {
    let span = window.end_ms.saturating_sub(window.start_ms);
    let start = window
        .start_ms
        .saturating_sub(span.saturating_mul(u64::from(weeks.saturating_sub(1))));
    let to_ts = |ms: u64| {
        i64::try_from(ms)
            .ok()
            .and_then(|v| jiff::Timestamp::from_millisecond(v).ok())
            .ok_or_else(|| EngineError::Invalid(format!("timestamp out of range: {ms}")))
    };
    Ok((to_ts(start)?, to_ts(window.end_ms)?))
}

/// Streams currently paused — the review skips them
/// (`docs/08-features/reviews-and-stats.md` §Weekly review step 1).
fn paused_streams(conn: &rusqlite::Connection) -> Result<BTreeSet<EntityRef>, EngineError> {
    let mut stmt = conn.prepare("SELECT stream_id FROM streams WHERE paused != 0")?;
    let out = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Stream, blob16(&raw)))
        .collect();
    Ok(out)
}

/// The Tasks currently filed under one Stream, for its activity feed.
fn stream_member_tasks(
    conn: &rusqlite::Connection,
    stream: &[u8; 16],
) -> Result<Vec<EntityRef>, EngineError> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE stream_id = ?")?;
    let out = stmt
        .query_map(params![&stream[..]], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Task, blob16(&raw)))
        .collect();
    Ok(out)
}

/// Every live Task, for the review's per-Stream lists.
fn read_live_tasks(conn: &rusqlite::Connection) -> Result<Vec<Task>, EngineError> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE deleted = 0 ORDER BY id ASC")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(t) = read_task(conn, &blob16(&raw))? {
            out.push(t);
        }
    }
    Ok(out)
}

/// Write the snapshot record. Idempotent on the snapshot id, which is what
/// makes re-delivery a no-op and lets two devices' reviews of one week coexist.
fn insert_review_snapshot_row(
    tx: &Transaction<'_>,
    s: &ReviewSnapshot,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let mut body = Vec::new();
    ciborium::ser::into_writer(s, &mut body)
        .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
    tx.execute(
        "INSERT OR IGNORE INTO review_snapshots
         (id, created_at_ms, window_start_ms, window_end_ms,
          completed, deferred, dropped, created, reopened, body,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &s.id.bytes()[..],
            s.created_at.as_millisecond(),
            s.window_start_ms() as i64,
            s.window_end_ms() as i64,
            i64::from(s.totals.completed),
            i64::from(s.totals.deferred),
            i64::from(s.totals.dropped),
            i64::from(s.totals.created),
            i64::from(s.totals.reopened),
            body,
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Time blocks (docs/02-domain/time-blocks.md)
// ---------------------------------------------------------------------------

/// The Block command path.
///
/// Block ops route under the Block's **own Stream**, like task ops and unlike
/// routine ops: a calendar is per-Stream in the UI, so routing there keeps a
/// Block's `seq` independent of the meta stream's.
///
/// `Task.blocks` is never written. It is derived from `block_tasks` on read
/// (see [`read_task_blocks`]), which is what makes the spec's "Bound Task's
/// `blocks` field updates symmetrically" hold by construction: one writer, one
/// op, and nothing for a concurrent edit of the Task to overwrite.
impl Engine {
    fn create_block(&self, db: &mut Db, d: BlockDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        require_kind(d.stream_id, EntityKind::Stream)?;
        for t in &d.tasks {
            require_kind(*t, EntityKind::Task)?;
        }
        let now_ms = self.clock.now_ms();
        let block_id = self.fresh_id(EntityKind::Block, now_ms);
        let op_id = self.fresh_op_id(now_ms);
        let tasks: BTreeSet<EntityRef> = d.tasks.iter().copied().collect();
        // Shadow copy: a Block created around exactly one Task and given no
        // title of its own takes that Task's title AS IT IS NOW. Later renames
        // of the Task do not reach the Block unless `title_track_task` is set.
        let title = match d.title {
            Some(t) => Some(t.trim().to_string()),
            None => self.shadow_title(db, &tasks)?,
        };
        let block = Block {
            id: block_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            stream_id: d.stream_id,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            title,
            title_track_task: d.title_track_task,
            tasks,
            deleted: false,
            unknown: Unknowns::new(),
        };
        block.validate_invariants()?;
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.create")?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    /// Write the Block one external calendar item maps onto, at the id
    /// [`imported_block_id`] derives from `(source, uid)`.
    ///
    /// Create and update are the same code path because the row write already
    /// is one (`upsert_block_row`): re-importing an unchanged file rewrites
    /// identical column values, which is what makes the whole operation
    /// idempotent without a "have I seen this UID" table to keep in step.
    ///
    /// Two fields are read back off the existing Block rather than taken from
    /// the draft, because they are not the importing calendar's to state — see
    /// [`Command::ImportBlock`].
    fn import_block(
        &self,
        db: &mut Db,
        source: &str,
        uid: &str,
        d: BlockDraft,
    ) -> Result<CommandResult, EngineError> {
        if uid.trim().is_empty() {
            return Err(EngineError::Invalid(
                "import: an external item needs a UID".into(),
            ));
        }
        d.validate()?;
        require_kind(d.stream_id, EntityKind::Stream)?;
        for t in &d.tasks {
            require_kind(*t, EntityKind::Task)?;
        }
        let now_ms = self.clock.now_ms();
        let block_id = imported_block_id(source, uid);
        let existing = read_block(db.conn(), block_id.bytes())?;
        let op_id = self.fresh_op_id(now_ms);

        // Union, not replace: a task the user bound to an imported Block is
        // theirs, and a re-import must not quietly unbind it.
        let mut tasks: BTreeSet<EntityRef> = d.tasks.iter().copied().collect();
        if let Some(e) = &existing {
            tasks.extend(e.tasks.iter().copied());
        }
        let title = match d.title {
            Some(t) => Some(t.trim().to_string()),
            None => self.shadow_title(db, &tasks)?,
        };
        let block = Block {
            id: block_id,
            // The Block was created when it was first imported; a re-import is
            // not a second creation of it.
            created_at: existing
                .as_ref()
                .map_or_else(|| ms_to_ts(now_ms as i64), |e| e.created_at),
            updated_at: ms_to_ts(now_ms as i64),
            stream_id: d.stream_id,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            title,
            title_track_task: d.title_track_task,
            tasks,
            // Re-importing an event the user deleted locally brings it back.
            // The alternative — honouring the tombstone — would make the
            // import silently incomplete, and the file is the statement of
            // what the external calendar holds.
            deleted: false,
            // Forward-compat fields a newer build wrote are the Block's, not
            // the importer's, so they survive the rewrite.
            unknown: existing
                .as_ref()
                .map_or_else(Unknowns::new, |e| e.unknown.clone()),
        };
        block.validate_invariants()?;
        // The op kind is what the activity feed reads, so a first import is a
        // create and a re-import is an update. Both apply identically on a
        // remote replica — every Block op is full-state.
        let inner_kind = if existing.is_some() {
            "block.update"
        } else {
            "block.create"
        };
        let seq = self.emit_block(db, &block, now_ms, &op_id, inner_kind)?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    fn update_block(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: BlockPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Block)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
        if let Some(s) = patch.starts_at {
            block.starts_at = s;
        }
        if let Some(e) = patch.ends_at {
            block.ends_at = e;
        }
        if let Some(t) = patch.title {
            block.title = t.map(|s| s.trim().to_string());
        }
        if let Some(track) = patch.title_track_task {
            block.title_track_task = track;
        }
        if let Some(s) = patch.stream_id {
            require_kind(s, EntityKind::Stream)?;
            block.stream_id = s;
        }
        block.updated_at = ms_to_ts(now_ms as i64);
        block.validate_invariants()?;
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.update")?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Tombstone a Block. The op carries the Block's **whole state** with
    /// `deleted` set and goes down [`Self::emit_block`] — the same path an
    /// update takes — so a delete that wins LWW replaces the row rather than
    /// flipping one column on top of whatever the receiving replica held.
    fn delete_block(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Block)?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {id}")))?;
        block.deleted = true;
        block.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.delete")?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Bind a Task to a Block. Idempotent on the membership set: re-binding an
    /// already-bound Task writes the same set, so every replica converges to
    /// the same membership whichever order the ops land in.
    fn bind_task(
        &self,
        db: &mut Db,
        block_id: EntityRef,
        task: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(block_id, EntityKind::Block)?;
        require_kind(task, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), block_id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {block_id}")))?;
        block.tasks.insert(task);
        // The first Task bound to an untitled Block lends it the shadow copy,
        // which is what makes drag-a-task-onto-the-grid produce a labelled
        // block instead of a blank one.
        if block.title.is_none() {
            block.title = self.shadow_title(db, &block.tasks)?;
        }
        block.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.update")?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    /// Unbind a Task from a Block. The Block survives with no tasks bound: an
    /// empty Block is a legitimate calendar entry ("gym", "lunch").
    fn unbind_task(
        &self,
        db: &mut Db,
        block_id: EntityRef,
        task: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(block_id, EntityKind::Block)?;
        require_kind(task, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut block = read_block(db.conn(), block_id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("block {block_id}")))?;
        block.tasks.remove(&task);
        block.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let seq = self.emit_block(db, &block, now_ms, &op_id, "block.update")?;
        Ok(CommandResult::new(block_id, None, op_id, seq))
    }

    /// Seal a full-state Block op, materialize the row and its bindings, and
    /// append the op — all in one transaction. Returns the op's `seq`.
    fn emit_block(
        &self,
        db: &mut Db,
        block: &Block,
        now_ms: u64,
        op_id: &[u8; 16],
        inner_kind: &str,
    ) -> Result<u64, EngineError> {
        let inner = match inner_kind {
            "block.create" => InnerOp::BlockCreate(Box::new(block.clone())),
            "block.delete" => InnerOp::BlockDelete(Box::new(block.clone())),
            _ => InnerOp::BlockUpdate(Box::new(block.clone())),
        };
        let inner_op = encode_inner_op(&inner)?;
        let seq = self.next_seq(db, block.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let stream_bytes = *block.stream_id.bytes();
        let block = block.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &block.stream_id, now_ms, None, false, false, false)?;
            upsert_block_row(tx, &block, &lww)?;
            replace_block_tasks(tx, &block)?;
            self.ops_insert(
                tx,
                op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner_op,
                inner_kind,
                "block",
                Some(block.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(seq)
    }

    /// The title a single bound Task lends a Block. `None` when the Block binds
    /// zero or two-plus Tasks (nothing unambiguous to shadow) or when the one
    /// Task has not been materialized on this replica yet.
    fn shadow_title(
        &self,
        db: &Db,
        tasks: &BTreeSet<EntityRef>,
    ) -> Result<Option<String>, EngineError> {
        let mut it = tasks.iter();
        let (Some(only), None) = (it.next(), it.next()) else {
            return Ok(None);
        };
        Ok(read_task(db.conn(), only.bytes())?.map(|t| t.title))
    }

    /// Blocks overlapping the civil day containing `day_ms`, in the device
    /// zone.
    fn query_day_blocks(&self, db: &Db, day_ms: u64) -> Result<QueryResult, EngineError> {
        let (from, to) = self.civil_span(day_ms as i64, 1)?;
        self.query_blocks_between(db, from, to)
    }

    /// Blocks overlapping the seven civil days beginning at the Monday of the
    /// week containing `week_ms`, in the device zone.
    fn query_week_blocks(&self, db: &Db, week_ms: u64) -> Result<QueryResult, EngineError> {
        let tz = self.device_zone();
        let anchor = ms_to_ts(week_ms as i64).to_zoned(tz).date();
        // Monday-first, matching `WeekGrid` and every other weekly fold here.
        let back = i64::from(anchor.weekday().to_monday_zero_offset());
        let monday = anchor
            .checked_sub(jiff::Span::new().days(back))
            .unwrap_or(anchor);
        let monday_ms = self.civil_day_start_ms(monday)?;
        let (from, to) = self.civil_span(monday_ms, 7)?;
        self.query_blocks_between(db, from, to)
    }

    /// `[start_of_day(at_ms), start_of_day(at_ms) + days)` in the device zone,
    /// as epoch-millisecond bounds.
    ///
    /// Computed over civil dates rather than by adding `86_400_000` so a day
    /// that is 23 or 25 hours long across a DST transition is still exactly one
    /// day on the grid.
    fn civil_span(&self, at_ms: i64, days: i64) -> Result<(i64, i64), EngineError> {
        let tz = self.device_zone();
        let date = ms_to_ts(at_ms).to_zoned(tz).date();
        let end_date = date
            .checked_add(jiff::Span::new().days(days))
            .map_err(|e| EngineError::Invalid(format!("calendar span: {e}")))?;
        Ok((
            self.civil_day_start_ms(date)?,
            self.civil_day_start_ms(end_date)?,
        ))
    }

    fn civil_day_start_ms(&self, date: jiff::civil::Date) -> Result<i64, EngineError> {
        let tz = self.device_zone();
        let zoned = tz
            .to_zoned(date.to_datetime(jiff::civil::Time::midnight()))
            .map_err(|e| EngineError::Invalid(format!("calendar day start: {e}")))?;
        Ok(zoned.timestamp().as_millisecond())
    }

    /// The device-local zone, from the injected clock. An unknown IANA name
    /// degrades to UTC rather than failing a read.
    fn device_zone(&self) -> jiff::tz::TimeZone {
        jiff::tz::TimeZone::get(&self.clock.timezone()).unwrap_or(jiff::tz::TimeZone::UTC)
    }

    /// Every live Block whose `[starts_at, ends_at)` overlaps `[from, to)`.
    ///
    /// Overlap, not containment: a two-hour block that started before the
    /// window still belongs on the grid, which is why the `blocks_by_end`
    /// index exists.
    fn query_blocks_between(
        &self,
        db: &Db,
        from: i64,
        to: i64,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM blocks
             WHERE deleted = 0 AND starts_at_ms < ? AND ends_at_ms > ?
             ORDER BY starts_at_ms ASC, id ASC",
        )?;
        let ids = stmt
            .query_map(params![to, from], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(ids.len());
        for raw in ids {
            let Some(block) = read_block(db.conn(), &blob16(&raw))? else {
                continue;
            };
            rows.push(block_row(db.conn(), block)?);
        }
        Ok(QueryResult::Blocks(rows))
    }
}

/// Assemble one [`BlockRow`]: the Block, the titles of the bound Tasks this
/// replica knows about, and the title the spec's shadow-copy rules resolve to.
fn block_row(conn: &rusqlite::Connection, block: Block) -> Result<BlockRow, EngineError> {
    let mut task_titles = Vec::with_capacity(block.tasks.len());
    for t in &block.tasks {
        if let Some(task) = read_task(conn, t.bytes())? {
            if !task.deleted {
                task_titles.push(task.title);
            }
        }
    }
    let title = block.resolve_title(&task_titles).map(ToOwned::to_owned);
    Ok(BlockRow {
        block,
        title,
        task_titles,
    })
}

/// INSERT-or-REPLACE a Block row, stamping the LWW columns.
fn upsert_block_row(tx: &Transaction<'_>, b: &Block, lww: &LwwStamp) -> rusqlite::Result<()> {
    let (start_ms, start_kind, start_tz) = b.starts_at.to_parts();
    let (end_ms, end_kind, end_tz) = b.ends_at.to_parts();
    tx.execute(
        "INSERT INTO blocks
         (id, stream_id, starts_at_ms, starts_at_kind, starts_at_tz,
          ends_at_ms, ends_at_kind, ends_at_tz, title, title_track_task,
          deleted, extra, created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            stream_id = excluded.stream_id,
            starts_at_ms = excluded.starts_at_ms,
            starts_at_kind = excluded.starts_at_kind,
            starts_at_tz = excluded.starts_at_tz,
            ends_at_ms = excluded.ends_at_ms,
            ends_at_kind = excluded.ends_at_kind,
            ends_at_tz = excluded.ends_at_tz,
            title = excluded.title,
            title_track_task = excluded.title_track_task,
            deleted = excluded.deleted,
            extra = excluded.extra,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device",
        params![
            &b.id.bytes()[..],
            &b.stream_id.bytes()[..],
            start_ms,
            start_kind,
            start_tz,
            end_ms,
            end_kind,
            end_tz,
            b.title.as_deref(),
            b.title_track_task as i64,
            b.deleted as i64,
            encode_unknowns(&b.unknown)?,
            b.created_at.as_millisecond(),
            b.updated_at.as_millisecond(),
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// Replace a Block's bindings with the op's set. Full-state, like every other
/// v1 op: the set on the winning op is the set.
fn replace_block_tasks(tx: &Transaction<'_>, b: &Block) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM block_tasks WHERE block_id = ?",
        params![&b.id.bytes()[..]],
    )?;
    for t in &b.tasks {
        tx.execute(
            "INSERT OR IGNORE INTO block_tasks (block_id, task_id) VALUES (?, ?)",
            params![&b.id.bytes()[..], &t.bytes()[..]],
        )?;
    }
    Ok(())
}

/// Read one Block, with its bindings.
fn read_block(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<Option<Block>, EngineError> {
    let row = conn
        .query_row(
            "SELECT stream_id, starts_at_ms, starts_at_kind, starts_at_tz,
                    ends_at_ms, ends_at_kind, ends_at_tz, title, title_track_task,
                    deleted, extra, created_at_ms, updated_at_ms
             FROM blocks WHERE id = ?",
            params![&id[..]],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<Vec<u8>>>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            },
        )
        .optional()?;
    let Some(b) = row else {
        return Ok(None);
    };
    Ok(Some(Block {
        id: EntityRef::new(EntityKind::Block, *id),
        created_at: ms_to_ts(b.11.max(0)),
        updated_at: ms_to_ts(b.12.max(0)),
        stream_id: EntityRef::new(EntityKind::Stream, blob16(&b.0)),
        starts_at: SunriseTime::from_parts(b.1, &b.2, b.3.as_deref()),
        ends_at: SunriseTime::from_parts(b.4, &b.5, b.6.as_deref()),
        title: b.7,
        title_track_task: b.8 != 0,
        tasks: read_block_tasks(conn, id)?,
        deleted: b.9 != 0,
        unknown: decode_unknowns(b.10),
    }))
}

/// The Tasks bound to one Block.
fn read_block_tasks(
    conn: &rusqlite::Connection,
    block: &[u8; 16],
) -> Result<BTreeSet<EntityRef>, EngineError> {
    let mut stmt = conn.prepare("SELECT task_id FROM block_tasks WHERE block_id = ?")?;
    let out = stmt
        .query_map(params![&block[..]], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Task, blob16(&raw)))
        .collect();
    Ok(out)
}

/// The live Blocks scheduling one Task — the derived other half of the
/// binding, and the only source `Task.blocks` is ever read from.
///
/// Deleted Blocks are filtered here rather than at bind time: a Block
/// tombstoned on another device stops appearing on its Tasks the moment the
/// tombstone merges, with no repair pass over `block_tasks`.
fn read_task_blocks(
    conn: &rusqlite::Connection,
    task: &[u8; 16],
) -> Result<BTreeSet<EntityRef>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT bt.block_id FROM block_tasks bt
         JOIN blocks b ON b.id = bt.block_id
         WHERE bt.task_id = ? AND b.deleted = 0",
    )?;
    let out = stmt
        .query_map(params![&task[..]], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Block, blob16(&raw)))
        .collect();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Attachments (docs/02-domain/attachments.md)
// ---------------------------------------------------------------------------

/// The Attachment command path.
///
/// Attachment ops route under the parent Task's Stream, so an attachment
/// travels with the work it belongs to and inherits that Stream's key.
///
/// Metadata is **write-once**: created, then only ever tombstoned. Every field
/// but `deleted` describes one specific run of ciphertext identified by
/// `content_hash`, so there is no update op — re-attaching an edited file is a
/// new attachment.
impl Engine {
    fn attach_file(&self, db: &mut Db, d: AttachmentDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        // The parent has to exist locally: its Stream is the op's routing
        // stream, and an attachment on a task this replica has never seen is a
        // client bug rather than an out-of-order merge.
        let parent = read_task(db.conn(), d.parent.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {}", d.parent)))?;
        let att = Attachment {
            id: self.fresh_id(EntityKind::Attachment, now_ms),
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            parent: d.parent,
            filename: d.filename.trim().to_string(),
            mime_type: d.mime_type.trim().to_string(),
            size_bytes: d.size_bytes,
            blob_key: d.blob_key,
            blob_id: d.blob_id,
            chunk_count: d.chunk_count,
            content_hash: d.content_hash,
            deleted: false,
            unknown: Unknowns::new(),
        };
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::AttachmentCreate(Box::new(att.clone())))?;
        let seq = self.next_seq(db, parent.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let stream_bytes = *parent.stream_id.bytes();
        let id = att.id;
        db.with_tx(|tx| -> rusqlite::Result<()> {
            upsert_attachment_row(tx, &att, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner_op,
                "attachment.create",
                "attachment",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Tombstone attachment metadata.
    ///
    /// This is step 1 of `docs/02-domain/attachments.md` §Deletion and all of
    /// it that belongs in the core: reclaiming the blob needs the device-cursor
    /// quorum and the 30-day grace period, which are the relay's job.
    fn detach_file(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Attachment)?;
        let now_ms = self.clock.now_ms();
        let mut att = read_attachment(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("attachment {id}")))?;
        let stream = read_task(db.conn(), att.parent.bytes())?
            .map_or_else(inbox_stream_ref, |t| t.stream_id);
        // The op carries the attachment's whole state with `deleted` set, so a
        // delete that wins LWW replaces the row on every replica rather than
        // flipping one column on top of whatever that replica held.
        att.deleted = true;
        att.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::AttachmentDelete(Box::new(att.clone())))?;
        let seq = self.next_seq(db, stream.bytes())?;
        let lww = self.lww_stamp(seq);
        let stream_bytes = *stream.bytes();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            upsert_attachment_row(tx, &att, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner_op,
                "attachment.delete",
                "attachment",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Live attachments on one Task, oldest first — the order they were added
    /// in, which is the order a preview pane shows them.
    fn query_task_attachments(&self, db: &Db, task: EntityRef) -> Result<QueryResult, EngineError> {
        require_kind(task, EntityKind::Task)?;
        let mut stmt = db.conn().prepare(
            "SELECT id FROM attachments
             WHERE parent_kind = 'task' AND parent_id = ? AND deleted = 0
             ORDER BY created_at_ms ASC, id ASC",
        )?;
        let ids = stmt
            .query_map(params![&task.bytes()[..]], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for raw in ids {
            if let Some(a) = read_attachment(db.conn(), &blob16(&raw))? {
                out.push(a);
            }
        }
        Ok(QueryResult::Attachments(out))
    }
}

/// INSERT-or-REPLACE an attachment row, stamping the LWW columns.
fn upsert_attachment_row(
    tx: &Transaction<'_>,
    a: &Attachment,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO attachments
         (id, parent_kind, parent_id, filename, mime_type, size_bytes,
          blob_key, blob_id, chunk_count, content_hash, deleted, extra,
          created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            filename = excluded.filename,
            mime_type = excluded.mime_type,
            size_bytes = excluded.size_bytes,
            blob_key = excluded.blob_key,
            blob_id = excluded.blob_id,
            chunk_count = excluded.chunk_count,
            content_hash = excluded.content_hash,
            deleted = excluded.deleted,
            extra = excluded.extra,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device",
        params![
            &a.id.bytes()[..],
            attachment_parent_kind(a.parent),
            &a.parent.bytes()[..],
            a.filename,
            a.mime_type,
            a.size_bytes as i64,
            &a.blob_key[..],
            &a.blob_id[..],
            a.chunk_count,
            &a.content_hash[..],
            a.deleted as i64,
            encode_unknowns(&a.unknown)?,
            a.created_at.as_millisecond(),
            a.updated_at.as_millisecond(),
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// The `parent_kind` discriminator. v1 validates Task-only parents on the
/// command path; a remote op from a build that widened it still stores its own
/// kind rather than being coerced to `task`.
fn attachment_parent_kind(parent: EntityRef) -> &'static str {
    match parent.kind() {
        EntityKind::Stream => "stream",
        EntityKind::Note => "note",
        _ => "task",
    }
}

fn read_attachment(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Attachment>, EngineError> {
    let row = conn
        .query_row(
            "SELECT parent_kind, parent_id, filename, mime_type, size_bytes,
                    blob_key, blob_id, chunk_count, content_hash, deleted, extra,
                    created_at_ms, updated_at_ms
             FROM attachments WHERE id = ?",
            params![&id[..]],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Vec<u8>>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<Vec<u8>>>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            },
        )
        .optional()?;
    let Some(a) = row else {
        return Ok(None);
    };
    let parent_kind = match a.0.as_str() {
        "stream" => EntityKind::Stream,
        "note" => EntityKind::Note,
        _ => EntityKind::Task,
    };
    Ok(Some(Attachment {
        id: EntityRef::new(EntityKind::Attachment, *id),
        created_at: ms_to_ts(a.11.max(0)),
        updated_at: ms_to_ts(a.12.max(0)),
        parent: EntityRef::new(parent_kind, blob16(&a.1)),
        filename: a.2,
        mime_type: a.3,
        size_bytes: u64::try_from(a.4.max(0)).unwrap_or(0),
        blob_key: blob32(&a.5),
        blob_id: blob16(&a.6),
        chunk_count: u32::try_from(a.7.max(0)).unwrap_or(0),
        content_hash: blob32(&a.8),
        deleted: a.9 != 0,
        unknown: decode_unknowns(a.10),
    }))
}

/// Left-pad / truncate a stored blob into a 32-byte key or digest.
fn blob32(raw: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    let take = raw.len().min(32);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

// ---------------------------------------------------------------------------
// Notification reads (docs/08-features/notifications.md, issue #9)
// ---------------------------------------------------------------------------

/// The read half of scheduled notifications.
///
/// Everything a notification says is computed here, on-device, after
/// decryption. The relay only ever sends a content-less wake-up, so a count or
/// a title reaching the server would be the whole privacy model failing at
/// once. The rules themselves are the pure `sunrise_domain::notify` functions;
/// the engine's job is to read the rows and resolve the civil bounds.
impl Engine {
    fn query_morning_summary(&self, db: &Db, now_ms: u64) -> Result<QueryResult, EngineError> {
        let (today_start, today_end) = self.civil_span(now_ms as i64, 1)?;
        // "Since the previous calendar date" — the start of yesterday, in the
        // device's zone, computed over civil dates so a DST day is still a day.
        let (prev_start, _) = self.civil_span(today_start - 1, 1)?;
        let tasks = read_live_tasks(db.conn())?;
        Ok(QueryResult::MorningSummary(Box::new(
            build_morning_summary(
                &tasks,
                ms_to_ts(prev_start),
                ms_to_ts(today_start),
                ms_to_ts(today_end),
                inbox_stream_ref(),
            ),
        )))
    }

    fn query_end_of_day_plan(&self, db: &Db, now_ms: u64) -> Result<QueryResult, EngineError> {
        let (day_start, day_end) = self.civil_span(now_ms as i64, 1)?;
        // "The week ahead": the seven civil days that follow today.
        let (_, week_end) = self.civil_span(now_ms as i64, 8)?;
        let tasks = read_live_tasks(db.conn())?;
        Ok(QueryResult::EndOfDayPlan(Box::new(build_end_of_day_plan(
            &tasks,
            ms_to_ts(day_start),
            ms_to_ts(day_end),
            ms_to_ts(week_end),
        ))))
    }

    /// Everything worth scheduling with the OS between `now` and the horizon.
    ///
    /// Two sources in v1: a Task's `scheduled_at` and a Block's start. A
    /// routine occurrence is already a Task by the time it is due — that is
    /// what materialization produces — so it needs no separate source, and
    /// having one would double every routine reminder.
    fn query_reminder_intents(
        &self,
        db: &Db,
        now_ms: u64,
        horizon_ms: u64,
        settings: &ReminderSettings,
    ) -> Result<QueryResult, EngineError> {
        // A non-primary device is silent, so do not even read the rows.
        if !settings.is_primary_device {
            return Ok(QueryResult::Reminders(Vec::new()));
        }
        let mut stream_leads: BTreeMap<[u8; 16], Option<u32>> = BTreeMap::new();
        let mut candidates = Vec::new();

        for t in read_live_tasks(db.conn())? {
            if !matches!(t.state, TaskState::Todo | TaskState::InProgress) {
                continue;
            }
            let Some(at) = t.scheduled_at.clone() else {
                continue;
            };
            let stream_lead = match stream_leads.entry(*t.stream_id.bytes()) {
                std::collections::btree_map::Entry::Occupied(e) => *e.get(),
                std::collections::btree_map::Entry::Vacant(e) => *e.insert(
                    read_stream(db.conn(), t.stream_id.bytes())?.and_then(|s| s.reminder_lead_s),
                ),
            };
            candidates.push(ReminderCandidate {
                entity: t.id,
                kind: ReminderKind::Task,
                at,
                title: t.title.clone(),
                own_lead_s: t.reminder_lead_s,
                stream_lead_s: stream_lead,
            });
        }

        // Blocks in the window, plus a day either side: the lead time can pull
        // a block's reminder back before the window's start, and the horizon
        // filter in `plan_reminders` is what actually decides.
        let pad = 86_400_000i64;
        let from = (now_ms as i64).saturating_sub(pad);
        let to = (horizon_ms as i64).saturating_add(pad);
        let QueryResult::Blocks(rows) = self.query_blocks_between(db, from, to)? else {
            return Err(EngineError::Invalid(
                "block read returned non-blocks".into(),
            ));
        };
        for row in rows {
            let stream_lead = match stream_leads.entry(*row.block.stream_id.bytes()) {
                std::collections::btree_map::Entry::Occupied(e) => *e.get(),
                std::collections::btree_map::Entry::Vacant(e) => *e.insert(
                    read_stream(db.conn(), row.block.stream_id.bytes())?
                        .and_then(|s| s.reminder_lead_s),
                ),
            };
            candidates.push(ReminderCandidate {
                entity: row.block.id,
                kind: ReminderKind::Block,
                at: row.block.starts_at.clone(),
                // The resolved title, so a notification says what the calendar
                // says rather than re-deriving the shadow-copy rules.
                title: row.title.unwrap_or_default(),
                own_lead_s: None,
                stream_lead_s: stream_lead,
            });
        }

        Ok(QueryResult::Reminders(plan_reminders(
            &candidates,
            ms_to_ts(now_ms as i64),
            ms_to_ts(horizon_ms as i64),
            &self.device_zone(),
            settings,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemRng;
    use parking_lot::Mutex as PLMutex;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::ActivityKind;
    use sunrise_storage::Db;

    #[derive(Debug)]
    struct FakeClock(PLMutex<u64>);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock()
        }
    }

    fn engine() -> Engine {
        let keychain = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
        Engine::from_clock(
            Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
            Arc::new(SystemRng),
            keychain,
        )
    }

    fn db() -> Db {
        Db::open_memory(&VaultRootKey::from_bytes([0xab; 32])).unwrap()
    }

    #[test]
    fn create_task_seals_a_valid_envelope() {
        let mut db = db();
        let e = engine();
        let res = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "sealed task".into(),
                    ..Default::default()
                }),
            )
            .unwrap();

        // The stored ops row is a real magic-prefixed OpEnvelope.
        let env_bytes = OpLog::get_envelope(&db, &res.op_id).unwrap().unwrap();
        assert_eq!(&env_bytes[..2], b"SR", "envelope carries the magic prefix");
        let env = sunrise_crypto::decode_envelope(&env_bytes).unwrap();
        // Inbox tasks route to the Inbox's own stream id, which since ADR-0024
        // is no longer the vault-meta id.
        assert_eq!(env.stream_id, INBOX_STREAM_BYTES);
        assert_eq!(env.device_id, e.keychain.device_id());
        assert_eq!(env.seq, res.seq);

        // Signature verifies against the keychain's device signing key.
        sunrise_crypto::verify_envelope(&env, &e.keychain.device_signing_pub()).unwrap();

        // open_op_row proves the full seal→unseal cycle: it yields inner-op CBOR
        // that parses as the externally-tagged `TaskCreate` variant.
        let inner = e.open_op_row(&db, &res.op_id).unwrap();
        let value: ciborium::value::Value = ciborium::de::from_reader(inner.as_slice()).unwrap();
        let ciborium::value::Value::Map(map) = value else {
            panic!("inner op must be a map");
        };
        let (k, _) = &map[0];
        assert_eq!(k, &ciborium::value::Value::Text("TaskCreate".to_string()));
    }

    #[test]
    fn per_device_seq_is_independent() {
        // Two devices (distinct keychains) writing to the same DB keep separate
        // per-(stream, device) seq sequences.
        let mut db = db();
        let clock = || Arc::new(FakeClock(PLMutex::new(1_700_000_000_000)));
        let ea = Engine::from_clock(
            clock(),
            Arc::new(SystemRng),
            Arc::new(Keychain::for_test_seeded(
                VaultRootKey::from_bytes([0xab; 32]),
                [1u8; 32],
            )),
        );
        let eb = Engine::from_clock(
            clock(),
            Arc::new(SystemRng),
            Arc::new(Keychain::for_test_seeded(
                VaultRootKey::from_bytes([0xab; 32]),
                [2u8; 32],
            )),
        );
        assert_ne!(ea.keychain.device_id(), eb.keychain.device_id());

        let mk = |title: &str| {
            Command::CreateTask(TaskDraft {
                title: title.into(),
                ..Default::default()
            })
        };
        let a1 = ea.apply(&mut db, mk("a1")).unwrap();
        let b1 = eb.apply(&mut db, mk("b1")).unwrap();
        let a2 = ea.apply(&mut db, mk("a2")).unwrap();
        assert_eq!(a1.seq, 1);
        assert_eq!(b1.seq, 1, "device B starts its own seq at 1");
        assert_eq!(a2.seq, 2, "device A continues its own sequence");
    }

    #[test]
    fn submit_enqueues_outbox_row() {
        let mut db = db();
        let e = engine();
        let res = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "outboxed".into(),
                    ..Default::default()
                }),
            )
            .unwrap();

        let pending = Outbox::list_unacked(&db).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, res.op_id);
        assert_eq!(Outbox::pending_count(&db).unwrap(), 1);

        db.with_tx(|tx| {
            Outbox::mark_acked(tx, &res.op_id, 999)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(Outbox::pending_count(&db).unwrap(), 0);
    }

    #[test]
    fn create_task_round_trips() {
        let mut db = db();
        let e = engine();
        let cmd = Command::CreateTask(TaskDraft {
            title: "draft a memo".into(),
            ..Default::default()
        });
        let res = e.apply(&mut db, cmd).unwrap();
        assert_eq!(res.entity.kind(), EntityKind::Task);
        assert_eq!(res.state, Some(TaskState::Todo));
        let t = match e.query(&db, Query::EntityById(res.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!("wrong query result variant"),
        };
        assert_eq!(t.title, "draft a memo");
        assert_eq!(t.stream_id, inbox_stream_ref());
    }

    #[test]
    fn complete_task_sets_done() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        e.apply(&mut db, Command::CompleteTask(r.entity)).unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.state, TaskState::Done);
        assert!(t.completed_at.is_some());
    }

    #[test]
    fn defer_increments_count() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        e.apply(
            &mut db,
            Command::DeferTask {
                id: r.entity,
                to_ms: 1_700_000_000_000 + 86_400_000,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.deferred_count, 1);
        assert!(t.scheduled_at.is_some());
    }

    #[test]
    fn delete_task_marks_deleted_and_hides_from_inbox() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        e.apply(&mut db, Command::DeleteTask(r.entity)).unwrap();
        let res = e.query(&db, Query::Inbox).unwrap();
        let tasks = match res {
            QueryResult::StreamTasks(v) => v,
            _ => panic!(),
        };
        assert!(tasks.is_empty());
    }

    #[test]
    fn create_stream_round_trips() {
        let mut db = db();
        let e = engine();
        let s = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    color: Some(StreamColor::Sky),
                    ..Default::default()
                }),
            )
            .unwrap();
        let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
            QueryResult::Stream(s) => *s,
            _ => panic!(),
        };
        assert_eq!(st.archived, false);
        // Name and color must round-trip through storage (regression: these
        // used to come back as "" / Slate).
        assert_eq!(st.name, "Work");
        assert_eq!(st.color, StreamColor::Sky);
    }

    // ---- contexts ----

    fn context_rows(e: &Engine, db: &Db) -> Vec<ContextRow> {
        match e.query(db, Query::Contexts).unwrap() {
            QueryResult::Contexts(rows) => rows,
            other => panic!("expected Contexts, got {other:?}"),
        }
    }

    fn new_context(e: &Engine, db: &mut Db, name: &str) -> EntityRef {
        e.apply(
            db,
            Command::CreateContext(ContextDraft {
                name: name.into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity
    }

    fn task_context_ids(db: &Db, task: EntityRef) -> BTreeSet<[u8; 16]> {
        let mut stmt = db
            .conn()
            .prepare("SELECT context_id FROM task_contexts WHERE task_id = ?")
            .unwrap();
        let rows = stmt
            .query_map(params![task.bytes().to_vec()], |r| r.get::<_, Vec<u8>>(0))
            .unwrap();
        rows.map(|r| {
            let raw = r.unwrap();
            let mut a = [0u8; 16];
            a.copy_from_slice(&raw[..16]);
            a
        })
        .collect()
    }

    #[test]
    fn created_context_is_listed_and_readable_by_id() {
        let mut db = db();
        let e = engine();
        let id = e
            .apply(
                &mut db,
                Command::CreateContext(ContextDraft {
                    name: "  Errands ".into(),
                    description: Some("out of the house".into()),
                }),
            )
            .unwrap()
            .entity;
        assert_eq!(id.kind(), EntityKind::Context);

        let rows = context_rows(&e, &db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].name, "Errands", "the name is stored trimmed");
        assert_eq!(rows[0].description.as_deref(), Some("out of the house"));
        assert_eq!(rows[0].task_count, 0);
        assert!(!rows[0].archived);

        match e.query(&db, Query::EntityById(id)).unwrap() {
            QueryResult::Context(c) => assert_eq!(c.name, "Errands"),
            other => panic!("expected Context, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_and_invalid_context_names_are_rejected() {
        let mut db = db();
        let e = engine();
        new_context(&e, &mut db, "errands");

        // Same name, different case + padding: still the same handle to a user
        // typing `@errands`, so it must not create a second context.
        for dup in ["errands", "  ERRANDS  ", "Errands"] {
            let err = e
                .apply(
                    &mut db,
                    Command::CreateContext(ContextDraft {
                        name: dup.into(),
                        ..Default::default()
                    }),
                )
                .unwrap_err();
            assert!(matches!(err, EngineError::Invalid(_)), "{dup}: {err:?}");
        }

        // Blank and over-length names never reach the store.
        for bad in [String::from("   "), "x".repeat(65)] {
            let err = e
                .apply(
                    &mut db,
                    Command::CreateContext(ContextDraft {
                        name: bad.clone(),
                        ..Default::default()
                    }),
                )
                .unwrap_err();
            assert!(matches!(err, EngineError::Validation(_)), "{err:?}");
        }

        // A rename onto an existing name is refused the same way...
        let other = new_context(&e, &mut db, "home");
        let err = e
            .apply(
                &mut db,
                Command::UpdateContext {
                    id: other,
                    patch: ContextPatch {
                        name: Some("ERRANDS".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
        // ...but renaming a context to its own name is not a clash.
        e.apply(
            &mut db,
            Command::UpdateContext {
                id: other,
                patch: ContextPatch {
                    name: Some("Home".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        assert_eq!(context_rows(&e, &db).len(), 2, "exactly two survived");
    }

    #[test]
    fn reserved_prefix_names_are_accepted_verbatim() {
        // The spec calls `waiting-on:` / `energy:` conventions, not enforced
        // types — the store must not police them.
        let mut db = db();
        let e = engine();
        for n in ["waiting-on:carlos", "energy:high", "energy:whatever"] {
            new_context(&e, &mut db, n);
        }
        let names: Vec<String> = context_rows(&e, &db).into_iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            vec!["energy:high", "energy:whatever", "waiting-on:carlos"]
        );
    }

    #[test]
    fn deleting_a_context_removes_it_from_every_task_that_carries_it() {
        let mut db = db();
        let e = engine();
        let errands = new_context(&e, &mut db, "errands");
        let home = new_context(&e, &mut db, "home");

        let mk = |e: &Engine, db: &mut Db, title: &str, ctxs: Vec<EntityRef>| {
            e.apply(
                db,
                Command::CreateTask(TaskDraft {
                    title: title.into(),
                    contexts: ctxs,
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity
        };
        let both = mk(&e, &mut db, "buy milk", vec![errands, home]);
        let only_errands = mk(&e, &mut db, "post parcel", vec![errands]);
        let untagged = mk(&e, &mut db, "think", vec![]);

        let by_name = |rows: Vec<ContextRow>, n: &str| {
            rows.into_iter().find(|r| r.name == n).expect("row present")
        };
        assert_eq!(by_name(context_rows(&e, &db), "errands").task_count, 2);

        e.apply(&mut db, Command::DeleteContext(errands)).unwrap();

        // Gone from the list...
        let rows = context_rows(&e, &db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "home");
        // ...and gone from every task that carried it, while the *other*
        // context on the same task is untouched.
        assert_eq!(task_context_ids(&db, both), BTreeSet::from([*home.bytes()]));
        assert!(task_context_ids(&db, only_errands).is_empty());
        assert!(task_context_ids(&db, untagged).is_empty());
        assert_eq!(
            read_task_t(&e, &db, both).contexts,
            BTreeSet::from([home]),
            "the task projection agrees"
        );
        assert_eq!(by_name(context_rows(&e, &db), "home").task_count, 1);

        // The Today context filter no longer matches the purged context.
        match e
            .query(
                &db,
                Query::Today {
                    now_ms: 1_700_000_000_000,
                    contexts: vec![errands],
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(t) => assert!(t.is_empty(), "no task still carries it"),
            other => panic!("expected Tasks, got {other:?}"),
        }
    }

    #[test]
    fn an_activity_feed_reports_the_order_things_actually_happened_in() {
        // `fold_activity` is a state machine over successive full-state
        // snapshots, so the order it reads ops in *is* the history it reports.
        // Ordering by `op_id` sorted same-millisecond ops by the random low
        // bits of a ULID — and a defer-then-complete pair read backwards does
        // not come out merely reordered, it fabricates a **reopen** that never
        // happened.
        //
        // The clock is fixed, so every op here shares `ts_ms` and the tie is
        // always taken. Twelve independent tasks make the old behaviour's
        // chance of passing (1/6)^12.
        let mut db = db();
        let e = engine();
        for i in 0..12 {
            let id = e
                .apply(
                    &mut db,
                    Command::CreateTask(TaskDraft {
                        title: format!("task {i}"),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .entity;
            e.apply(
                &mut db,
                Command::DeferTask {
                    id,
                    to_ms: 1_700_000_000_000 + 86_400_000,
                },
            )
            .unwrap();
            e.apply(&mut db, Command::CompleteTask(id)).unwrap();

            let events = match e
                .query(
                    &db,
                    Query::ActivityTimeline {
                        entity: id,
                        limit: 50,
                    },
                )
                .unwrap()
            {
                QueryResult::Activity(rows) => rows,
                other => panic!("expected Activity, got {other:?}"),
            };
            // Newest first.
            let verbs: Vec<&str> = events.iter().map(|ev| ev.kind.verb()).collect();
            assert_eq!(
                verbs,
                vec!["task.completed", "task.deferred", "task.created"],
                "task {i}: the feed must not invent a transition"
            );
        }
    }

    #[test]
    fn context_tasks_lists_across_streams_and_survives_a_purge() {
        // A Context cuts across Streams, so its listing must too — that is
        // the whole difference between a Context and a Stream, and a client
        // cannot offer "stand in @errands" without it.
        let mut db = db();
        let e = engine();
        let errands = new_context(&e, &mut db, "errands");
        let work = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let inbox_task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "post a letter".into(),
                    contexts: vec![errands],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let work_task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "collect the parcel".into(),
                    stream_id: Some(work),
                    contexts: vec![errands],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "untagged".into(),
                ..Default::default()
            }),
        )
        .unwrap();

        let listed = |e: &Engine, db: &Db| match e.query(db, Query::ContextTasks(errands)).unwrap()
        {
            QueryResult::StreamTasks(t) => t.into_iter().map(|t| t.id).collect::<BTreeSet<_>>(),
            other => panic!("expected StreamTasks, got {other:?}"),
        };
        assert_eq!(listed(&e, &db), BTreeSet::from([inbox_task, work_task]));

        // Completed work stays listed: hiding it would make the picker's
        // count disagree with the list it opens.
        e.apply(&mut db, Command::CompleteTask(inbox_task)).unwrap();
        assert_eq!(listed(&e, &db), BTreeSet::from([inbox_task, work_task]));

        // Deleting a task drops it; deleting the Context empties the listing,
        // because the purge removes the tag from every task.
        e.apply(&mut db, Command::DeleteTask(inbox_task)).unwrap();
        assert_eq!(listed(&e, &db), BTreeSet::from([work_task]));
        e.apply(&mut db, Command::DeleteContext(errands)).unwrap();
        assert!(listed(&e, &db).is_empty());
    }

    #[test]
    fn archiving_a_context_keeps_it_on_its_tasks() {
        // Per the spec, archiving only hides a Context from the picker.
        let mut db = db();
        let e = engine();
        let ctx = new_context(&e, &mut db, "errands");
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "buy milk".into(),
                    contexts: vec![ctx],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;

        e.apply(
            &mut db,
            Command::UpdateContext {
                id: ctx,
                patch: ContextPatch {
                    archived: Some(true),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        let rows = context_rows(&e, &db);
        assert_eq!(rows.len(), 1, "archived contexts are still listed");
        assert!(rows[0].archived);
        assert_eq!(rows[0].task_count, 1);
        assert_eq!(read_task_t(&e, &db, task).contexts, BTreeSet::from([ctx]));
    }

    #[test]
    fn context_commands_reject_a_non_context_id() {
        let mut db = db();
        let e = engine();
        let err = e
            .apply(&mut db, Command::DeleteContext(stream_ref(3)))
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    }

    /// The externally-tagged [`InnerOp`] variant name carried by `op_id`'s
    /// sealed envelope.
    fn inner_op_variant(e: &Engine, db: &Db, op_id: &[u8; 16]) -> String {
        let inner = e.open_op_row(db, op_id).unwrap();
        let ciborium::value::Value::Map(map) =
            ciborium::de::from_reader::<ciborium::value::Value, _>(inner.as_slice()).unwrap()
        else {
            panic!("inner op must be a map");
        };
        match &map[0].0 {
            ciborium::value::Value::Text(t) => t.clone(),
            other => panic!("unexpected key {other:?}"),
        }
    }

    #[test]
    fn context_ops_seal_their_own_inner_op_variants() {
        let mut db = db();
        let e = engine();
        let create = e
            .apply(
                &mut db,
                Command::CreateContext(ContextDraft {
                    name: "errands".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(inner_op_variant(&e, &db, &create.op_id), "ContextCreate");

        let upd = e
            .apply(
                &mut db,
                Command::UpdateContext {
                    id: create.entity,
                    patch: ContextPatch {
                        description: Some(Some("errand list".into())),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        assert_eq!(inner_op_variant(&e, &db, &upd.op_id), "ContextUpdate");

        let del = e
            .apply(&mut db, Command::DeleteContext(create.entity))
            .unwrap();
        assert_eq!(inner_op_variant(&e, &db, &del.op_id), "ContextDelete");
    }

    #[test]
    fn update_stream_persists_name_and_color() {
        let mut db = db();
        let e = engine();
        let s = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    color: Some(StreamColor::Sky),
                    ..Default::default()
                }),
            )
            .unwrap();
        let patch = StreamPatch {
            name: Some("Personal".into()),
            color: Some(StreamColor::Emerald),
            ..Default::default()
        };
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: s.entity,
                patch,
            },
        )
        .unwrap();
        let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
            QueryResult::Stream(s) => *s,
            _ => panic!(),
        };
        assert_eq!(st.name, "Personal");
        assert_eq!(st.color, StreamColor::Emerald);
    }

    /// A new stream lands after the last existing one, and moving one rewrites
    /// exactly that one row.
    #[test]
    fn a_reorder_moves_one_stream_and_touches_no_sibling() {
        let mut db = db();
        let e = engine();
        let mut ids = Vec::new();
        for name in ["one", "two", "three"] {
            ids.push(
                e.apply(
                    &mut db,
                    Command::CreateStream(StreamDraft {
                        name: name.into(),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .entity,
            );
        }
        let listed = |e: &Engine, db: &Db| -> Vec<(String, String)> {
            match e.query(db, Query::StreamList).unwrap() {
                QueryResult::Streams(rows) => rows[1..]
                    .iter()
                    .map(|r| (r.name.clone(), r.sort_order.clone()))
                    .collect(),
                other => panic!("expected Streams, got {other:?}"),
            }
        };
        let before = listed(&e, &db);
        assert_eq!(
            before.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["one", "two", "three"],
            "a new stream defaults to the end of the list"
        );

        // Drag "three" to the front: its new key is the one between the start
        // of the list and "one".
        let key = sort_order::between(None, Some(&before[0].1)).unwrap();
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: ids[2],
                patch: StreamPatch {
                    sort_order: Some(key.clone()),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        let after = listed(&e, &db);
        assert_eq!(
            after.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["three", "one", "two"]
        );
        // The whole point of the fractional index: the two rows that did not
        // move still hold the keys they held before the drag.
        assert_eq!(after[1].1, before[0].1, "\"one\" was rewritten");
        assert_eq!(after[2].1, before[1].1, "\"two\" was rewritten");
        assert_eq!(after[0].1, key);
    }

    /// A key the core cannot place is refused rather than stored. Without this
    /// one bad write would be permanent: nothing sorts after `"a0"`, so every
    /// later append would land above it forever.
    #[test]
    fn a_sort_key_outside_the_alphabet_is_rejected() {
        let mut db = db();
        let e = engine();
        let s = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        for bad in ["a0", "", "NA"] {
            let res = e.apply(
                &mut db,
                Command::UpdateStream {
                    id: s,
                    patch: StreamPatch {
                        sort_order: Some(bad.into()),
                        ..Default::default()
                    },
                },
            );
            assert!(
                matches!(res, Err(EngineError::Validation(_))),
                "{bad:?} was accepted"
            );
        }
        let st = match e.query(&db, Query::EntityById(s)).unwrap() {
            QueryResult::Stream(s) => *s,
            other => panic!("expected Stream, got {other:?}"),
        };
        assert!(sort_order::is_valid(&st.sort_order));
    }

    #[test]
    fn cannot_delete_inbox_stream() {
        let mut db = db();
        let e = engine();
        let res = e.apply(&mut db, Command::DeleteStream(inbox_stream_ref()));
        assert!(matches!(res, Err(EngineError::Invalid(_))));
    }

    #[test]
    fn update_task_changes_state_and_records_op() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let mut p = TaskPatch::default();
        p.state = Some(TaskState::InProgress);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch: p,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.state, TaskState::InProgress);
        let count: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap();
        assert!(count >= 2);
    }

    #[test]
    fn today_query_returns_tasks_due_in_window() {
        let mut db = db();
        let e = engine();
        let now = 1_700_000_000_000u64;
        let due_soon = ms_to_ts((now + 3_600_000) as i64);
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "soon".into(),
                due_at: Some(due_soon.into()),
                ..Default::default()
            }),
        )
        .unwrap();
        let far = ms_to_ts((now + 7 * 86_400_000) as i64);
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "far".into(),
                due_at: Some(far.into()),
                ..Default::default()
            }),
        )
        .unwrap();
        let r = e
            .query(
                &db,
                Query::Today {
                    now_ms: now,
                    contexts: vec![],
                },
            )
            .unwrap();
        let tasks = match r {
            QueryResult::Tasks(v) => v,
            _ => panic!(),
        };
        let titles: Vec<_> = tasks.iter().map(|t| t.title.clone()).collect();
        assert!(titles.contains(&"soon".to_string()));
        assert!(!titles.contains(&"far".to_string()));
    }

    #[test]
    fn today_query_filters_by_context() {
        // `Query::Today`'s `contexts` field used to be bound to `_contexts` and
        // dropped on the floor, so callers got an unfiltered list back with no
        // error. Assert the filter is honoured in all three modes: empty slice
        // means no filter, a matching context narrows, and a non-matching one
        // excludes.
        let mut db = db();
        let e = engine();
        let now = 1_700_000_000_000u64;
        let due_soon = ms_to_ts((now + 3_600_000) as i64);

        let home = EntityRef::new(EntityKind::Context, [0xA1; 16]);
        let work = EntityRef::new(EntityKind::Context, [0xB2; 16]);

        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "at home".into(),
                due_at: Some(due_soon.into()),
                contexts: vec![home],
                ..Default::default()
            }),
        )
        .unwrap();
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "at work".into(),
                due_at: Some(due_soon.into()),
                contexts: vec![work],
                ..Default::default()
            }),
        )
        .unwrap();
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "anywhere".into(),
                due_at: Some(due_soon.into()),
                ..Default::default()
            }),
        )
        .unwrap();

        let titles = |contexts: Vec<EntityRef>| -> Vec<String> {
            match e
                .query(
                    &db,
                    Query::Today {
                        now_ms: now,
                        contexts,
                    },
                )
                .unwrap()
            {
                QueryResult::Tasks(v) => v.iter().map(|t| t.title.clone()).collect(),
                _ => panic!("expected Tasks"),
            }
        };

        // No filter: everything in the window.
        let all = titles(vec![]);
        assert_eq!(
            all.len(),
            3,
            "unfiltered Today should return all 3: {all:?}"
        );

        // Single context: only the task carrying it.
        let only_home = titles(vec![home]);
        assert_eq!(only_home, vec!["at home".to_string()]);

        // Multiple contexts are an OR-set, and a context-less task is excluded
        // by any non-empty filter.
        let mut both = titles(vec![home, work]);
        both.sort();
        assert_eq!(both, vec!["at home".to_string(), "at work".to_string()]);

        // A context nothing carries yields nothing — not everything.
        let unused = EntityRef::new(EntityKind::Context, [0xC3; 16]);
        assert!(
            titles(vec![unused]).is_empty(),
            "an unmatched context must exclude, not fall through to unfiltered"
        );
    }

    #[test]
    fn invalid_state_transition_rejected() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        // todo can transition to in_progress, done, cancelled all directly,
        // so we pick a forbidden non-self transition.
        // todo → todo is allowed. Force a sequence: todo → done → in_progress
        // is allowed; then in_progress → done allowed. Try patching done →
        // explicitly back to todo (allowed). What's NOT allowed? Per the
        // current `can_transition_to`, every non-equal pair across the four
        // states is allowed. The matrix is essentially fully connected. So
        // invalidity is exposed by patching wrong_kind; test that path.
        let bogus = EntityRef::new(EntityKind::Stream, *r.entity.bytes());
        let res = e.apply(
            &mut db,
            Command::UpdateTask {
                id: bogus,
                patch: TaskPatch::default(),
            },
        );
        assert!(matches!(res, Err(EngineError::Invalid(_))));
    }

    #[test]
    fn stream_list_counts_open_tasks_inbox_first_and_ordering() {
        let mut db = db();
        let e = engine();

        // Two real streams. "Beta" is created first, and stays first: a new
        // stream is appended after the last existing key, so an untouched
        // vault lists streams in the order they were made. Name order was the
        // rule only while `sort_order` was hardcoded and every row tied.
        let beta = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Beta".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let alpha = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "alpha".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let gone = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Deleted".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteStream(gone)).unwrap();

        // Inbox: 1 open + 1 done (done not counted).
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "inbox open".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        let inbox_done = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "inbox done".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::CompleteTask(inbox_done)).unwrap();

        // alpha: 2 open (one todo, one in-progress), 1 cancelled, 1 deleted.
        for _ in 0..2 {
            e.apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "a open".into(),
                    stream_id: Some(alpha),
                    ..Default::default()
                }),
            )
            .unwrap();
        }
        let a_ip = db_last_task(&db, alpha);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: a_ip,
                patch: TaskPatch {
                    state: Some(TaskState::InProgress),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let a_cancel = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "a cancel".into(),
                    stream_id: Some(alpha),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: a_cancel,
                patch: TaskPatch {
                    state: Some(TaskState::Cancelled),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let a_del = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "a del".into(),
                    stream_id: Some(alpha),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteTask(a_del)).unwrap();

        // beta: 0 open tasks.
        let rows = match e.query(&db, Query::StreamList).unwrap() {
            QueryResult::Streams(v) => v,
            _ => panic!("wrong variant"),
        };

        // Inbox is always first.
        assert_eq!(rows[0].id, inbox_stream_ref());
        assert_eq!(rows[0].name, "Inbox");
        assert_eq!(rows[0].open_task_count, 1);

        // Then real (non-deleted) streams in fractional-index order, which for
        // a vault nobody has reordered is creation order. Deleted stream
        // excluded entirely.
        let names: Vec<_> = rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["Inbox", "Beta", "alpha"]);
        assert!(!names.contains(&"Deleted".to_string()));

        // The keys really are ascending and really are keys — the sidebar
        // computes new positions from these, so a row arriving without one
        // would break the very next drag.
        let keys: Vec<_> = rows[1..].iter().map(|r| r.sort_order.clone()).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
        assert!(keys.iter().all(|k| sort_order::is_valid(k)));
        // The Inbox is not a stream and holds no position.
        assert_eq!(rows[0].sort_order, "");

        let alpha_row = rows.iter().find(|r| r.id == alpha).unwrap();
        assert_eq!(alpha_row.open_task_count, 2);
        let beta_row = rows.iter().find(|r| r.id == beta).unwrap();
        assert_eq!(beta_row.open_task_count, 0);
    }

    #[test]
    fn search_matches_title_and_body_excludes_deleted_and_respects_limit() {
        let mut db = db();
        let e = engine();

        // Title hit.
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "quarterly kangaroo report".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        // Body hit (unique term only in the body).
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "misc notes".into(),
                body: Some(NoteBody(b"remember the kangaroo".to_vec())),
                ..Default::default()
            }),
        )
        .unwrap();
        // Deleted task that would otherwise match.
        let del = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "kangaroo obsolete".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteTask(del)).unwrap();

        let hits = match e
            .query(
                &db,
                Query::Search {
                    text: "kangaroo".into(),
                    limit: 10,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v,
            _ => panic!("wrong variant"),
        };
        let titles: Vec<_> = hits.iter().map(|t| t.title.clone()).collect();
        assert_eq!(
            hits.len(),
            2,
            "title + body hits, deleted excluded: {titles:?}"
        );
        assert!(titles.contains(&"quarterly kangaroo report".to_string()));
        assert!(titles.contains(&"misc notes".to_string()));
        assert!(!titles.contains(&"kangaroo obsolete".to_string()));

        // Limit is honored.
        let limited = match e
            .query(
                &db,
                Query::Search {
                    text: "kangaroo".into(),
                    limit: 1,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v,
            _ => panic!(),
        };
        assert_eq!(limited.len(), 1);

        // Empty / whitespace-only input returns nothing without touching FTS.
        let empty = match e
            .query(
                &db,
                Query::Search {
                    text: "   ".into(),
                    limit: 10,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v,
            _ => panic!(),
        };
        assert!(empty.is_empty());
    }

    // Fetch the most-recently-created task id in a stream (highest rowid
    // proxy via id ordering is unstable, so use created ordering by title
    // being unavailable — instead read the last inserted via the ops table
    // is overkill; query tasks table directly).
    fn db_last_task(db: &Db, stream: EntityRef) -> EntityRef {
        let blob: Vec<u8> = stream.bytes().to_vec();
        let raw: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT id FROM tasks WHERE stream_id = ? ORDER BY id DESC LIMIT 1",
                params![blob],
                |r| r.get(0),
            )
            .unwrap();
        let mut a = [0u8; 16];
        let take = raw.len().min(16);
        a[..take].copy_from_slice(&raw[..take]);
        EntityRef::new(EntityKind::Task, a)
    }

    fn sample_constraint() -> ScheduleConstraint {
        ScheduleConstraint {
            time_of_day: Some(sunrise_domain::TimeOfDayRange {
                start: jiff::civil::time(9, 0, 0, 0),
                end: jiff::civil::time(17, 0, 0, 0),
            }),
            days_of_week: sunrise_domain::WeekdaySet::new(),
            date_range: None,
            severity: sunrise_domain::ConstraintSeverity::Hard,
        }
    }

    #[test]
    fn create_task_with_constraints_round_trips() {
        let mut db = db();
        let e = engine();
        let c = sample_constraint();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "with constraint".into(),
                    scheduling_constraints: vec![c],
                    ..Default::default()
                }),
            )
            .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.scheduling_constraints, vec![c]);
    }

    #[test]
    fn update_task_replaces_whole_constraint_list() {
        let mut db = db();
        let e = engine();
        let c1 = sample_constraint();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    scheduling_constraints: vec![c1],
                    ..Default::default()
                }),
            )
            .unwrap();
        // Replace with a different, single-element list.
        let c2 = ScheduleConstraint {
            time_of_day: None,
            days_of_week: sunrise_domain::WeekdaySet::from_days([sunrise_domain::Weekday::Sa]),
            date_range: None,
            severity: sunrise_domain::ConstraintSeverity::Soft,
        };
        let patch = TaskPatch {
            scheduling_constraints: Some(vec![c2]),
            ..Default::default()
        };
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.scheduling_constraints, vec![c2]);

        // Clearing to empty persists as NULL / empty list.
        let clear = TaskPatch {
            scheduling_constraints: Some(vec![]),
            ..Default::default()
        };
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch: clear,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert!(t.scheduling_constraints.is_empty());
    }

    #[test]
    fn patch_due_before_scheduled_is_rejected() {
        let mut db = db();
        let e = engine();
        let now = 1_700_000_000_000i64;
        let scheduled = ms_to_ts(now + 2 * 3_600_000);
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    scheduled_at: Some(scheduled.into()),
                    ..Default::default()
                }),
            )
            .unwrap();
        // Patch sets ONLY due_at, earlier than the existing scheduled_at. This
        // used to slip through silently; the invariant re-check must reject it.
        let earlier = ms_to_ts(now + 3_600_000);
        let patch = TaskPatch {
            due_at: Some(Some(earlier.into())),
            ..Default::default()
        };
        let res = e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        );
        assert!(matches!(
            res,
            Err(EngineError::Validation(
                sunrise_domain::ValidationError::DueBeforeScheduled
            ))
        ));
    }

    #[test]
    fn invalid_constraint_list_rejected_on_create_and_update() {
        let mut db = db();
        let e = engine();
        // An empty (no-dimension) constraint is invalid.
        let bad = ScheduleConstraint {
            time_of_day: None,
            days_of_week: sunrise_domain::WeekdaySet::new(),
            date_range: None,
            severity: sunrise_domain::ConstraintSeverity::Hard,
        };
        let create = e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                scheduling_constraints: vec![bad],
                ..Default::default()
            }),
        );
        assert!(matches!(create, Err(EngineError::Validation(_))));

        // A valid task, then an update that introduces the invalid list.
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "ok".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let patch = TaskPatch {
            scheduling_constraints: Some(vec![bad]),
            ..Default::default()
        };
        let update = e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        );
        assert!(matches!(update, Err(EngineError::Validation(_))));
    }

    proptest::proptest! {
        /// `Query::Search` must never surface an FTS5 syntax error, no matter
        /// how hostile the input (quotes, colons, stars, unbalanced quotes,
        /// `NEAR`, hyphens, parentheses).
        #[test]
        fn search_never_errors_on_hostile_input(s in ".*") {
            let mut db = db();
            let e = engine();
            e.apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "haystack".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
            let res = e.query(
                &db,
                Query::Search {
                    text: s,
                    limit: 20,
                },
            );
            proptest::prop_assert!(res.is_ok());
        }

        /// Any epoch-ms in a sane range survives the `ms_to_ts` ->
        /// `as_millisecond` round-trip that bridges SQLite INTEGER storage
        /// and `jiff::Timestamp`.
        #[test]
        fn ms_timestamp_round_trip(ms in -10_000_000_000_000i64..=10_000_000_000_000i64) {
            let ts = ms_to_ts(ms);
            proptest::prop_assert_eq!(ts.as_millisecond(), ms);
        }
    }

    // ---- routine materialization tests ----

    use sunrise_domain::{RRule, Routine, RoutineCatchupPolicy, RoutineDraft, TaskTemplate};

    const NOW: i64 = 1_700_000_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn stream_ref(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [b; 16])
    }

    fn routine_draft(
        stream: EntityRef,
        rrule: &str,
        starts_ms: i64,
        policy: RoutineCatchupPolicy,
        constraints: Vec<ScheduleConstraint>,
    ) -> RoutineDraft {
        RoutineDraft {
            template: TaskTemplate {
                title: "Water plants".into(),
                stream_id: stream,
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse(rrule).unwrap(),
            timezone: "UTC".into(),
            starts_at: ms_to_ts(starts_ms),
            ends_at: None,
            scheduling_constraints: constraints,
            catchup_policy: policy,
        }
    }

    fn op_count(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap()
    }

    fn live_task_ids(db: &Db) -> BTreeSet<[u8; 16]> {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM tasks WHERE deleted = 0")
            .unwrap();
        let out = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|raw| {
                let raw = raw.unwrap();
                let mut a = [0u8; 16];
                let take = raw.len().min(16);
                a[..take].copy_from_slice(&raw[..take]);
                a
            })
            .collect();
        out
    }

    fn past_task_count(db: &Db, rid: EntityRef) -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM tasks
                 WHERE routine_id = ? AND deleted = 0 AND scheduled_at_ms <= ?",
                params![rid.bytes().to_vec(), NOW],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn create_routine_materializes_future_tasks_with_linkage() {
        let mut db = db();
        let e = engine();
        let stream = stream_ref(5);
        let c = sample_constraint();
        let draft = routine_draft(
            stream,
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            vec![c],
        );
        let res = e.apply(&mut db, Command::CreateRoutine(draft)).unwrap();
        let rid = res.entity;
        assert_eq!(rid.kind(), EntityKind::Routine);

        // Routine is queryable.
        let routines = match e.query(&db, Query::Routines).unwrap() {
            QueryResult::Routines(v) => v,
            _ => panic!(),
        };
        assert_eq!(routines.len(), 1);
        let routine = routines.into_iter().next().unwrap();
        assert_eq!(routine.id, rid);

        // Daily occurrences within the 14-day horizon are all materialized.
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 14 * DAY_MS));
        let occ = routine.occurrences_in(window).unwrap();
        assert!(occ.len() >= 13, "daily horizon materializes ~14 days");

        // Each occurrence has a deterministic task with linkage + copied
        // constraints + scheduled_at == occurrence instant.
        for o in &occ {
            let tid = occurrence_task_id(&rid, &o.key);
            let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
                QueryResult::Task(t) => *t,
                _ => panic!("task {tid} missing for occurrence {}", o.key),
            };
            assert_eq!(t.routine_id, Some(rid));
            assert_eq!(t.routine_occurrence, Some(o.at));
            assert_eq!(t.scheduled_at, Some(o.at.into()));
            assert_eq!(t.scheduling_constraints, vec![c]);
        }
    }

    #[test]
    fn rematerialize_is_idempotent() {
        let mut db = db();
        let e = engine();
        let draft = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        e.apply(&mut db, Command::CreateRoutine(draft)).unwrap();
        let ids_before = live_task_ids(&db);
        let ops_before = op_count(&db);

        // Re-run materialization at the same clock: no new tasks, no new ops.
        e.apply(&mut db, Command::MaterializeRoutines { now_ms: NOW as u64 })
            .unwrap();
        assert_eq!(live_task_ids(&db), ids_before, "no duplicate tasks");
        assert_eq!(op_count(&db), ops_before, "no new ops on re-materialize");
    }

    #[test]
    fn two_engines_produce_identical_task_ids() {
        // Same routine content (same id) on two independent DBs => identical
        // deterministic task-id sets, even though op-ids differ (rng).
        fn fixed_routine() -> Routine {
            Routine {
                id: EntityRef::new(EntityKind::Routine, [0x33; 16]),
                created_at: ms_to_ts(NOW),
                updated_at: ms_to_ts(NOW),
                template: TaskTemplate {
                    title: "R".into(),
                    stream_id: stream_ref(7),
                    contexts: Vec::new(),
                    energy: None,
                    priority: None,
                    estimated_duration_s: None,
                    body: None,
                },
                rrule: RRule::parse("FREQ=DAILY").unwrap(),
                timezone: "UTC".into(),
                starts_at: ms_to_ts(NOW + 3_600_000),
                ends_at: None,
                scheduling_constraints: Vec::new(),
                skip_dates: Vec::new(),
                skipped_keys: Vec::new(),
                catchup_policy: RoutineCatchupPolicy::Skip,
                streak_counter: 0,
                last_completed_at: None,
                grace_window_s: None,
                forgiveness_enabled: true,
                streak_started_at: None,
                forgivenesses_in_window: 0,
                streak_keys: Vec::new(),
                paused: false,
                paused_until: None,
                archived: false,
                deleted: false,
                unknown: Unknowns::new(),
            }
        }

        fn run() -> BTreeSet<[u8; 16]> {
            let mut db = db();
            let e = engine();
            let r = fixed_routine();
            db.with_tx(|tx| {
                ensure_stream_row(
                    tx,
                    &r.template.stream_id,
                    NOW as u64,
                    None,
                    false,
                    false,
                    false,
                )?;
                insert_routine_row(tx, &r, NOW as u64, &e.lww_stamp(1))
            })
            .unwrap();
            e.materialize_one_routine(&mut db, &r, NOW as u64).unwrap();
            live_task_ids(&db)
        }

        let a = run();
        let b = run();
        assert!(!a.is_empty());
        assert_eq!(a, b, "task-id sets converge across devices");
    }

    #[test]
    fn catchup_policies_skip_queue_merge() {
        let start = NOW - (5 * DAY_MS + 3_600_000); // 6 missed daily occurrences
                                                    // Skip: no past tasks.
        {
            let mut db = db();
            let e = engine();
            let d = routine_draft(
                stream_ref(5),
                "FREQ=DAILY",
                start,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            );
            let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
            assert_eq!(past_task_count(&db, rid), 0, "skip drops missed");
        }
        // Queue: one task per missed occurrence.
        {
            let mut db = db();
            let e = engine();
            let d = routine_draft(
                stream_ref(5),
                "FREQ=DAILY",
                start,
                RoutineCatchupPolicy::Queue,
                Vec::new(),
            );
            let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
            assert_eq!(past_task_count(&db, rid), 6, "queue keeps every missed");
        }
        // Merge: exactly one past task, titled with the catch-up suffix.
        {
            let mut db = db();
            let e = engine();
            let d = routine_draft(
                stream_ref(5),
                "FREQ=DAILY",
                start,
                RoutineCatchupPolicy::Merge,
                Vec::new(),
            );
            let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
            assert_eq!(
                past_task_count(&db, rid),
                1,
                "merge collapses missed to one"
            );
            let title: String = db
                .conn()
                .query_row(
                    "SELECT title FROM tasks
                     WHERE routine_id = ? AND deleted = 0 AND scheduled_at_ms <= ?",
                    params![rid.bytes().to_vec(), NOW],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(title.contains("(x6 catch-up)"), "merge title: {title}");
        }
    }

    #[test]
    fn skip_occurrence_prevents_task() {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        let routine = read_routine(db.conn(), rid.bytes()).unwrap().unwrap();
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
        let occ = routine.occurrences_in(window).unwrap();
        let target = occ[2].key.clone();
        let tid = occurrence_task_id(&rid, &target);

        // Task exists pre-skip.
        assert!(matches!(
            e.query(&db, Query::EntityById(tid)).unwrap(),
            QueryResult::Task(_)
        ));

        e.apply(
            &mut db,
            Command::SkipRoutineOccurrence {
                id: rid,
                occurrence_key: target.clone(),
            },
        )
        .unwrap();

        // The existing task is tombstoned...
        let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert!(t.deleted, "skipped occurrence's task is tombstoned");

        // ...and re-materialization does not resurrect it.
        e.apply(&mut db, Command::MaterializeRoutines { now_ms: NOW as u64 })
            .unwrap();
        let t2 = match e.query(&db, Query::EntityById(tid)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert!(t2.deleted, "skip persists across materialize");
    }

    #[test]
    fn update_routine_regenerates_future_but_keeps_started() {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        let routine = read_routine(db.conn(), rid.bytes()).unwrap().unwrap();
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
        let daily = routine.occurrences_in(window).unwrap();
        // occ[0] is the anchor weekday (kept by a weekly rule); occ[2] is a
        // different weekday (dropped); mark occ[3] in-progress so it survives.
        let keep_anchor = occurrence_task_id(&rid, &daily[0].key);
        let drop_id = occurrence_task_id(&rid, &daily[2].key);
        let started_id = occurrence_task_id(&rid, &daily[3].key);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: started_id,
                patch: TaskPatch {
                    state: Some(TaskState::InProgress),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // Change to weekly (only the anchor weekday recurs).
        let patch = RoutinePatch {
            rrule: Some(RRule::parse("FREQ=WEEKLY").unwrap()),
            ..Default::default()
        };
        e.apply(&mut db, Command::UpdateRoutine { id: rid, patch })
            .unwrap();

        let deleted = |id: EntityRef| -> bool {
            match e.query(&db, Query::EntityById(id)).unwrap() {
                QueryResult::Task(t) => t.deleted,
                _ => panic!(),
            }
        };
        assert!(!deleted(keep_anchor), "anchor-weekday occurrence stays");
        assert!(deleted(drop_id), "off-cadence future task regenerated away");
        assert!(!deleted(started_id), "started task is preserved");
    }

    // ---- reminder lead time (docs/08-features/notifications.md) ----

    /// A per-task lead time survives the materialized row. It used to be the
    /// case for `Stream.icon` that it did not, and the field read back `None`
    /// forever; this is the same shape of bug and the same shape of test.
    #[test]
    fn a_tasks_reminder_lead_time_survives_a_reread() {
        let mut db = db();
        let e = engine();
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Standup".into(),
                    reminder_lead_s: Some(900),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        assert_eq!(
            read_task(db.conn(), task.bytes())
                .unwrap()
                .unwrap()
                .reminder_lead_s,
            Some(900)
        );

        // `Some(0)` is a real answer -- "fire at the scheduled time" -- and
        // must not read back as "not set".
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    reminder_lead_s: Some(Some(0)),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            read_task(db.conn(), task.bytes())
                .unwrap()
                .unwrap()
                .reminder_lead_s,
            Some(0)
        );

        // Clearing falls back to the Stream's.
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    reminder_lead_s: Some(None),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            read_task(db.conn(), task.bytes())
                .unwrap()
                .unwrap()
                .reminder_lead_s,
            None
        );
    }

    #[test]
    fn a_streams_reminder_lead_time_survives_a_reread() {
        let mut db = db();
        let e = engine();
        let stream = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    reminder_lead_s: Some(1_800),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        assert_eq!(
            read_stream(db.conn(), stream.bytes())
                .unwrap()
                .unwrap()
                .reminder_lead_s,
            Some(1_800)
        );
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: stream,
                patch: StreamPatch {
                    reminder_lead_s: Some(None),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            read_stream(db.conn(), stream.bytes())
                .unwrap()
                .unwrap()
                .reminder_lead_s,
            None
        );
    }

    // ---- time blocks (docs/02-domain/time-blocks.md) ----

    /// A block on 2026-03-04, `hour..hour + len` floating (no zone), which is
    /// what a calendar grid draws when the user has not pinned a zone.
    fn block_at(hour: i8, len: i8) -> (SunriseTime, SunriseTime) {
        (
            SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(hour, 0, 0, 0)),
            SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(hour + len, 0, 0, 0)),
        )
    }

    fn block_draft(hour: i8, len: i8, title: Option<&str>) -> BlockDraft {
        let (starts_at, ends_at) = block_at(hour, len);
        BlockDraft {
            stream_id: inbox_stream_ref(),
            starts_at,
            ends_at,
            title: title.map(ToOwned::to_owned),
            title_track_task: false,
            tasks: Vec::new(),
        }
    }

    fn day_rows(e: &Engine, db: &Db, at_ms: u64) -> Vec<BlockRow> {
        match e.query(db, Query::DayBlocks { day_ms: at_ms }).unwrap() {
            QueryResult::Blocks(rows) => rows,
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    /// The instant a floating civil time on the test date resolves to under the
    /// engine's device zone (UTC in tests).
    fn day_ms() -> u64 {
        SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(12, 0, 0, 0)).index_ms() as u64
    }

    #[test]
    fn create_block_round_trips_and_lands_on_the_day_grid() {
        let mut db = db();
        let e = engine();
        let res = e
            .apply(
                &mut db,
                Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
            )
            .unwrap();
        assert_eq!(res.entity.kind(), EntityKind::Block);

        let rows = day_rows(&e, &db, day_ms());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].block.id, res.entity);
        assert_eq!(rows[0].title.as_deref(), Some("Gym"));
        assert!(rows[0].block.tasks.is_empty());

        // The op is a real sealed `BlockCreate`.
        assert_eq!(inner_op_variant(&e, &db, &res.op_id), "BlockCreate");
    }

    #[test]
    fn an_inverted_block_is_rejected() {
        let mut db = db();
        let e = engine();
        let (starts_at, ends_at) = block_at(10, 1);
        let bad = BlockDraft {
            starts_at: ends_at,
            ends_at: starts_at,
            ..block_draft(9, 1, Some("backwards"))
        };
        assert!(matches!(
            e.apply(&mut db, Command::CreateBlock(bad)),
            Err(EngineError::Validation(ValidationError::Field {
                field: "block.ends_at",
                ..
            }))
        ));
    }

    /// The grid asks "what overlaps this day", so a block that started
    /// yesterday and runs into today is on today's grid — and one that ends
    /// exactly at midnight is not.
    #[test]
    fn the_day_grid_returns_overlaps_not_containments() {
        let mut db = db();
        let e = engine();
        let midnight = jiff::civil::date(2026, 3, 4).at(0, 0, 0, 0);
        let spans_into_today = BlockDraft {
            starts_at: SunriseTime::floating(jiff::civil::date(2026, 3, 3).at(23, 0, 0, 0)),
            ends_at: SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(1, 0, 0, 0)),
            ..block_draft(9, 1, Some("overnight"))
        };
        let ends_at_midnight = BlockDraft {
            starts_at: SunriseTime::floating(jiff::civil::date(2026, 3, 3).at(22, 0, 0, 0)),
            ends_at: SunriseTime::floating(midnight),
            ..block_draft(9, 1, Some("yesterday only"))
        };
        e.apply(&mut db, Command::CreateBlock(spans_into_today))
            .unwrap();
        e.apply(&mut db, Command::CreateBlock(ends_at_midnight))
            .unwrap();

        let titles: Vec<_> = day_rows(&e, &db, day_ms())
            .into_iter()
            .filter_map(|r| r.title)
            .collect();
        assert_eq!(titles, vec!["overnight".to_string()]);
    }

    /// A week window is built from civil dates, so it is exactly seven days and
    /// Monday-first. 2026-03-04 is a Wednesday.
    #[test]
    fn the_week_grid_runs_monday_to_sunday() {
        let mut db = db();
        let e = engine();
        for (date, title) in [
            ((2026, 3, 2), "monday"),        // in
            ((2026, 3, 8), "sunday"),        // in
            ((2026, 3, 1), "sunday_before"), // out
            ((2026, 3, 9), "next_monday"),   // out
        ] {
            let d = jiff::civil::date(date.0, date.1, date.2);
            e.apply(
                &mut db,
                Command::CreateBlock(BlockDraft {
                    starts_at: SunriseTime::floating(d.at(9, 0, 0, 0)),
                    ends_at: SunriseTime::floating(d.at(10, 0, 0, 0)),
                    ..block_draft(9, 1, Some(title))
                }),
            )
            .unwrap();
        }
        let rows = match e
            .query(
                &db,
                Query::WeekBlocks {
                    week_ms: day_ms(), // Wednesday of that week
                },
            )
            .unwrap()
        {
            QueryResult::Blocks(rows) => rows,
            other => panic!("expected blocks, got {other:?}"),
        };
        let titles: Vec<_> = rows.into_iter().filter_map(|r| r.title).collect();
        assert_eq!(titles, vec!["monday".to_string(), "sunday".to_string()]);
    }

    #[test]
    fn binding_a_task_updates_task_blocks_symmetrically() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(block_draft(9, 2, Some("Deep work"))),
            )
            .unwrap()
            .entity;

        e.apply(&mut db, Command::BindTask { block, task }).unwrap();

        // Forward: the block carries the task.
        let rows = day_rows(&e, &db, day_ms());
        assert_eq!(
            rows[0].block.tasks.iter().copied().collect::<Vec<_>>(),
            vec![task]
        );
        assert_eq!(rows[0].task_titles, vec!["Write the report".to_string()]);
        // Reverse: the task carries the block, with no second op.
        let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
        assert_eq!(t.blocks.iter().copied().collect::<Vec<_>>(), vec![block]);

        // Unbinding takes both halves back down.
        e.apply(&mut db, Command::UnbindTask { block, task })
            .unwrap();
        let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
        assert!(t.blocks.is_empty());
        assert!(day_rows(&e, &db, day_ms())[0].block.tasks.is_empty());
    }

    /// The spec's shadow copy: the title is taken at bind time and does NOT
    /// follow a later rename of the task.
    #[test]
    fn an_untitled_block_shadow_copies_the_bound_task_title() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Original title");
        let block = e
            .apply(&mut db, Command::CreateBlock(block_draft(9, 1, None)))
            .unwrap()
            .entity;
        e.apply(&mut db, Command::BindTask { block, task }).unwrap();
        assert_eq!(
            day_rows(&e, &db, day_ms())[0].title.as_deref(),
            Some("Original title")
        );

        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("Renamed later".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            day_rows(&e, &db, day_ms())[0].title.as_deref(),
            Some("Original title"),
            "a shadow copy does not follow the task"
        );
    }

    #[test]
    fn title_track_task_follows_the_rename() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Original title");
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(BlockDraft {
                    title_track_task: true,
                    tasks: vec![task],
                    ..block_draft(9, 1, None)
                }),
            )
            .unwrap()
            .entity;
        assert_eq!(
            day_rows(&e, &db, day_ms())[0].title.as_deref(),
            Some("Original title")
        );

        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("Renamed later".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            day_rows(&e, &db, day_ms())[0].title.as_deref(),
            Some("Renamed later")
        );

        // A second bound task takes the flag out of play: there is no single
        // task title left to shadow, so the STORED title stands — which is the
        // shadow copy taken when the block was created, not the rename.
        let other = new_task(&e, &mut db, "Something else");
        e.apply(&mut db, Command::BindTask { block, task: other })
            .unwrap();
        assert_eq!(
            day_rows(&e, &db, day_ms())[0].title.as_deref(),
            Some("Original title"),
            "the stored title stands once two tasks are bound"
        );
    }

    #[test]
    fn updating_a_block_moves_it_on_the_grid() {
        let mut db = db();
        let e = engine();
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
            )
            .unwrap()
            .entity;
        let (starts_at, ends_at) = block_at(18, 1);
        e.apply(
            &mut db,
            Command::UpdateBlock {
                id: block,
                patch: BlockPatch {
                    starts_at: Some(starts_at.clone()),
                    ends_at: Some(ends_at),
                    title: Some(Some("Evening gym".into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let rows = day_rows(&e, &db, day_ms());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("Evening gym"));
        assert_eq!(rows[0].block.starts_at, starts_at);
    }

    #[test]
    fn a_patch_that_inverts_the_range_is_rejected() {
        let mut db = db();
        let e = engine();
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
            )
            .unwrap()
            .entity;
        let (before_start, _) = block_at(8, 1);
        assert!(e
            .apply(
                &mut db,
                Command::UpdateBlock {
                    id: block,
                    patch: BlockPatch {
                        ends_at: Some(before_start),
                        ..Default::default()
                    },
                },
            )
            .is_err());
    }

    #[test]
    fn deleting_a_block_clears_it_from_the_grid_and_from_its_tasks() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(BlockDraft {
                    tasks: vec![task],
                    ..block_draft(9, 1, Some("Deep work"))
                }),
            )
            .unwrap()
            .entity;
        assert_eq!(day_rows(&e, &db, day_ms()).len(), 1);

        let res = e.apply(&mut db, Command::DeleteBlock(block)).unwrap();
        assert_eq!(inner_op_variant(&e, &db, &res.op_id), "BlockDelete");
        assert!(day_rows(&e, &db, day_ms()).is_empty());
        // The Task survives; only the binding stops being visible.
        let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
        assert!(!t.deleted);
        assert!(t.blocks.is_empty());
    }

    /// The delete-wins case, forced rather than raced.
    ///
    /// A `BlockDelete` that beats a concurrent `BlockUpdate` has to replace the
    /// **whole** row on the receiving replica, not just flip its tombstone. The
    /// old id-only shape left B holding its own title under A's winning stamp,
    /// and because both sides then carried that same stamp neither would ever
    /// accept a correction (ADR-0014).
    ///
    /// The e2e probes reproduce this only when the delete happens to land last.
    /// Here the two stamps are chosen so it always does, which is what makes a
    /// single run conclusive — and makes reverting the fix fail every time
    /// rather than one run in four.
    #[test]
    fn a_block_delete_that_wins_lww_replaces_the_whole_row() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        let block = ea
            .apply(
                &mut dba,
                Command::CreateBlock(block_draft(9, 1, Some("contested"))),
            )
            .unwrap()
            .entity;
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.create"))
            .unwrap();

        // B renames it...
        set_clock(&cb, T0 + 1_000);
        eb.apply(
            &mut dbb,
            Command::UpdateBlock {
                id: block,
                patch: BlockPatch {
                    title: Some(Some("renamed".into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // ...and A deletes it strictly later, so the delete wins on both sides.
        set_clock(&ca, T0 + 2_000);
        ea.apply(&mut dba, Command::DeleteBlock(block)).unwrap();

        eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.delete"))
            .unwrap();
        ea.apply_remote(&mut dba, &env_for_kind(&dbb, block.bytes(), "block.update"))
            .unwrap();

        let ra = read_block(dba.conn(), block.bytes()).unwrap().unwrap();
        let rb = read_block(dbb.conn(), block.bytes()).unwrap().unwrap();
        assert!(ra.deleted && rb.deleted, "both replicas tombstoned it");
        assert_eq!(
            ra.title, rb.title,
            "and agree on the rest of the row, not just the tombstone"
        );
        assert_eq!(
            ra.title.as_deref(),
            Some("contested"),
            "the deleting replica's state is what replaced it"
        );
        assert_eq!(
            row_stamp(&dba, "blocks", "id", &block),
            row_stamp(&dbb, "blocks", "id", &block),
        );
    }

    /// A `BlockDelete` that overtakes its own `BlockCreate` materializes the
    /// tombstone anyway. Under the old shape there was no row to tombstone, so
    /// the delete was dropped, the create then landed live, and the replica sat
    /// permanently out of step with the one that had deleted.
    #[test]
    fn a_block_delete_that_overtakes_its_create_still_lands_as_a_tombstone() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let block = ea
            .apply(
                &mut dba,
                Command::CreateBlock(block_draft(9, 1, Some("Deep work"))),
            )
            .unwrap()
            .entity;
        set_clock(&ca, T0 + 1_000);
        ea.apply(&mut dba, Command::DeleteBlock(block)).unwrap();

        // Delete first, create second.
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.delete"))
            .unwrap();
        let rb = read_block(dbb.conn(), block.bytes())
            .unwrap()
            .expect("the delete materialized a row of its own");
        assert!(rb.deleted);
        assert_eq!(rb.title.as_deref(), Some("Deep work"));

        eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.create"))
            .unwrap();
        assert!(
            read_block(dbb.conn(), block.bytes())
                .unwrap()
                .unwrap()
                .deleted,
            "the older create loses LWW and does not resurrect it"
        );
        assert_eq!(
            row_stamp(&dba, "blocks", "id", &block),
            row_stamp(&dbb, "blocks", "id", &block),
        );
    }

    #[test]
    fn block_commands_reject_a_non_block_id() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "not a block");
        assert!(e.apply(&mut db, Command::DeleteBlock(task)).is_err());
        assert!(e
            .apply(
                &mut db,
                Command::UpdateBlock {
                    id: task,
                    patch: BlockPatch::default()
                }
            )
            .is_err());
        assert!(e
            .apply(&mut db, Command::BindTask { block: task, task })
            .is_err());
    }

    #[test]
    fn entity_by_id_reads_one_block_as_a_grid_row() {
        let mut db = db();
        let e = engine();
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
            )
            .unwrap()
            .entity;
        match e.query(&db, Query::EntityById(block)).unwrap() {
            QueryResult::Blocks(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].block.id, block);
            }
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    /// A block created on one replica materializes identically on another from
    /// the sealed op alone — bindings included.
    #[test]
    fn a_remote_block_op_materializes_with_its_bindings() {
        let clock_a = Arc::new(FakeClock(PLMutex::new(T0)));
        let clock_b = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], clock_a);
        let eb = engine_seeded(ROOT, [2u8; 32], clock_b);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let task = new_task(&ea, &mut dba, "Write the report");
        let created = ea
            .apply(
                &mut dba,
                Command::CreateBlock(BlockDraft {
                    tasks: vec![task],
                    ..block_draft(9, 1, Some("Deep work"))
                }),
            )
            .unwrap();

        let event = eb
            .apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
            .unwrap();
        assert!(matches!(event, Some(DomainEvent::Created(id)) if id == created.entity));

        let rows = day_rows(&eb, &dbb, day_ms());
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].block.tasks.iter().copied().collect::<Vec<_>>(),
            vec![task],
            "the binding rides along with the full-state op"
        );
        // The bound task's own op has not arrived on this replica, so there is
        // no title to report for it — a valid state, not a repair case.
        assert!(rows[0].task_titles.is_empty());
        assert_eq!(rows[0].title.as_deref(), Some("Deep work"));
    }

    // ---- imported blocks (docs/09-integrations/icalendar.md §Import) ----

    fn import(source: &str, uid: &str, draft: BlockDraft) -> Command {
        Command::ImportBlock {
            source: source.into(),
            uid: uid.into(),
            draft,
        }
    }

    /// The rule the whole importer rests on: the same `(source, uid)` is the
    /// same Block, not a second one.
    #[test]
    fn importing_the_same_uid_twice_writes_one_block() {
        let mut db = db();
        let e = engine();
        let first = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
            )
            .unwrap();
        let second = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(10, 1, Some("Gym, moved"))),
            )
            .unwrap();
        assert_eq!(first.entity, second.entity);
        assert_eq!(first.entity, imported_block_id("ics", "ev1"));

        let rows = day_rows(&e, &db, day_ms());
        assert_eq!(rows.len(), 1, "one block, not two: {rows:?}");
        assert_eq!(rows[0].title.as_deref(), Some("Gym, moved"));
        assert_eq!(rows[0].block.starts_at, block_at(10, 1).0);
    }

    #[test]
    fn the_same_uid_from_two_sources_is_two_blocks() {
        let mut db = db();
        let e = engine();
        let a = e
            .apply(&mut db, import("ics", "ev1", block_draft(9, 1, Some("A"))))
            .unwrap();
        let b = e
            .apply(
                &mut db,
                import("other", "ev1", block_draft(11, 1, Some("B"))),
            )
            .unwrap();
        assert_ne!(a.entity, b.entity);
        assert_eq!(day_rows(&e, &db, day_ms()).len(), 2);
    }

    /// A re-import is not a second creation, so the Block keeps the moment it
    /// first appeared.
    #[test]
    fn a_re_import_preserves_created_at_and_moves_updated_at() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock.clone());
        let mut db = db_root(ROOT);
        let id = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
            )
            .unwrap()
            .entity;
        set_clock(&clock, T0 + 60_000);
        e.apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();

        let b = read_block(db.conn(), id.bytes()).unwrap().unwrap();
        assert_eq!(b.created_at.as_millisecond(), T0 as i64);
        assert_eq!(b.updated_at.as_millisecond(), (T0 + 60_000) as i64);
    }

    /// A task the user bound to an imported Block is theirs. A re-import
    /// carries no bindings and must not take it away.
    #[test]
    fn a_re_import_keeps_a_task_the_user_bound() {
        let mut db = db();
        let e = engine();
        let block = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
            )
            .unwrap()
            .entity;
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Pack kit".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::BindTask { block, task }).unwrap();

        e.apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();
        let rows = day_rows(&e, &db, day_ms());
        assert_eq!(
            rows[0].block.tasks.iter().copied().collect::<Vec<_>>(),
            vec![task],
            "the user's binding survives a re-import"
        );
    }

    /// The op kind is what the activity feed reads: a first import is a
    /// creation and a re-import is not.
    #[test]
    fn the_first_import_emits_a_create_and_the_second_an_update() {
        let mut db = db();
        let e = engine();
        let first = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
            )
            .unwrap();
        assert_eq!(inner_op_variant(&e, &db, &first.op_id), "BlockCreate");
        let second = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
            )
            .unwrap();
        assert_eq!(inner_op_variant(&e, &db, &second.op_id), "BlockUpdate");
    }

    /// Re-importing an event the user deleted locally brings it back: the file
    /// is the statement of what the external calendar holds, and honouring the
    /// tombstone would make the import silently incomplete.
    #[test]
    fn a_re_import_resurrects_a_locally_deleted_block() {
        let mut db = db();
        let e = engine();
        let id = e
            .apply(
                &mut db,
                import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteBlock(id)).unwrap();
        assert!(day_rows(&e, &db, day_ms()).is_empty());

        e.apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();
        assert_eq!(day_rows(&e, &db, day_ms()).len(), 1);
    }

    #[test]
    fn an_import_with_no_uid_is_refused() {
        let mut db = db();
        let e = engine();
        assert!(e
            .apply(&mut db, import("ics", "  ", block_draft(9, 1, Some("Gym"))))
            .is_err());
    }

    #[test]
    fn an_import_validates_its_draft_like_any_other_write() {
        let mut db = db();
        let e = engine();
        let inverted = BlockDraft {
            starts_at: block_at(10, 1).0,
            ends_at: block_at(9, 1).0,
            ..block_draft(9, 1, Some("Gym"))
        };
        assert!(e.apply(&mut db, import("ics", "ev1", inverted)).is_err());
    }

    // ---- attachments (docs/02-domain/attachments.md) ----

    fn attachment_draft(parent: EntityRef) -> AttachmentDraft {
        AttachmentDraft {
            parent,
            filename: "receipt.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 4096,
            blob_key: [7u8; 32],
            blob_id: [9u8; 16],
            chunk_count: 2,
            content_hash: [11u8; 32],
        }
    }

    fn attachments_of(e: &Engine, db: &Db, task: EntityRef) -> Vec<Attachment> {
        match e.query(db, Query::TaskAttachments(task)).unwrap() {
            QueryResult::Attachments(a) => a,
            other => panic!("expected attachments, got {other:?}"),
        }
    }

    #[test]
    fn an_attached_file_round_trips_with_its_key() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "File the expenses");
        let res = e
            .apply(&mut db, Command::AttachFile(attachment_draft(task)))
            .unwrap();
        assert_eq!(res.entity.kind(), EntityKind::Attachment);
        assert_eq!(inner_op_variant(&e, &db, &res.op_id), "AttachmentCreate");

        let rows = attachments_of(&e, &db, task);
        assert_eq!(rows.len(), 1);
        let a = &rows[0];
        assert_eq!(a.id, res.entity);
        assert_eq!(a.parent, task);
        assert_eq!(a.filename, "receipt.pdf");
        assert_eq!(a.mime_type, "application/pdf");
        assert_eq!(a.size_bytes, 4096);
        assert_eq!(a.chunk_count, 2);
        // The per-blob key is what makes the ciphertext readable anywhere else;
        // losing it on the way through storage would orphan the blob.
        assert_eq!(a.blob_key, [7u8; 32]);
        assert_eq!(a.blob_id, [9u8; 16]);
        assert_eq!(a.content_hash, [11u8; 32]);
        assert!(!a.deleted);
    }

    /// The op routes under the parent task's Stream, not the meta stream, so
    /// an attachment travels with the work it belongs to.
    #[test]
    fn an_attachment_routes_under_its_tasks_stream() {
        let mut db = db();
        let e = engine();
        let stream = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "File the expenses".into(),
                    stream_id: Some(stream),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let res = e
            .apply(&mut db, Command::AttachFile(attachment_draft(task)))
            .unwrap();
        let env = sunrise_crypto::decode_envelope(&env_bytes(&db, &res.op_id)).unwrap();
        assert_eq!(&env.stream_id, stream.bytes());
    }

    #[test]
    fn attaching_to_an_unknown_task_is_rejected() {
        let mut db = db();
        let e = engine();
        let ghost = EntityRef::new(EntityKind::Task, [0xee; 16]);
        assert!(matches!(
            e.apply(&mut db, Command::AttachFile(attachment_draft(ghost))),
            Err(EngineError::NotFound(_))
        ));
    }

    #[test]
    fn an_invalid_draft_is_rejected_before_any_op_is_written() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "File the expenses");
        let before = op_count(&db);
        let bad = AttachmentDraft {
            size_bytes: 0,
            ..attachment_draft(task)
        };
        assert!(e.apply(&mut db, Command::AttachFile(bad)).is_err());
        assert_eq!(op_count(&db), before, "a rejected draft writes no op");
    }

    #[test]
    fn detaching_tombstones_the_metadata_and_hides_it_from_the_task() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "File the expenses");
        let att = e
            .apply(&mut db, Command::AttachFile(attachment_draft(task)))
            .unwrap()
            .entity;
        assert_eq!(attachments_of(&e, &db, task).len(), 1);

        let res = e.apply(&mut db, Command::DetachFile(att)).unwrap();
        assert_eq!(inner_op_variant(&e, &db, &res.op_id), "AttachmentDelete");
        assert!(attachments_of(&e, &db, task).is_empty());
    }

    /// Attachment metadata is write-once, so there is no `AttachmentUpdate` to
    /// race a delete against — but the *ordering* half of the same defect is
    /// live. A delete that overtakes its create had no row to tombstone and was
    /// dropped; the create then landed live and the replica stayed permanently
    /// out of step with the one that had deleted.
    ///
    /// Carrying the attachment's full state makes the delete self-sufficient.
    #[test]
    fn an_attachment_delete_that_overtakes_its_create_still_lands_as_a_tombstone() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let task = new_task(&ea, &mut dba, "File the expenses");
        let att = ea
            .apply(&mut dba, Command::AttachFile(attachment_draft(task)))
            .unwrap()
            .entity;
        set_clock(&ca, T0 + 1_000);
        ea.apply(&mut dba, Command::DetachFile(att)).unwrap();

        // Delete first, create second.
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, att.bytes(), "attachment.delete"),
        )
        .unwrap();
        let rb = read_attachment(dbb.conn(), att.bytes())
            .unwrap()
            .expect("the delete materialized a row of its own");
        assert!(rb.deleted);
        // The delete carries the metadata, so the tombstone is a real record
        // rather than a bare id: the blob GC can still identify what it names.
        assert_eq!(rb.filename, "receipt.pdf");
        assert_eq!(rb.content_hash, [11u8; 32]);
        assert_eq!(rb.parent, task);

        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, att.bytes(), "attachment.create"),
        )
        .unwrap();
        assert!(
            read_attachment(dbb.conn(), att.bytes())
                .unwrap()
                .unwrap()
                .deleted,
            "the older create loses LWW and does not resurrect it"
        );
        assert_eq!(
            row_stamp(&dba, "attachments", "id", &att),
            row_stamp(&dbb, "attachments", "id", &att),
        );
    }

    #[test]
    fn attachments_list_oldest_first_and_only_for_their_own_task() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], Arc::clone(&clock));
        let mut db = db_root(ROOT);
        let a_task = new_task(&e, &mut db, "One");
        let b_task = new_task(&e, &mut db, "Two");
        for (i, (task, name)) in [
            (a_task, "first.pdf"),
            (a_task, "second.pdf"),
            (b_task, "other.pdf"),
        ]
        .into_iter()
        .enumerate()
        {
            // Distinct instants: "oldest first" is an ordering on
            // `created_at_ms`, and three attachments in one millisecond have
            // no oldest.
            set_clock(&clock, T0 + i as u64 * 1_000);
            e.apply(
                &mut db,
                Command::AttachFile(AttachmentDraft {
                    filename: name.into(),
                    ..attachment_draft(task)
                }),
            )
            .unwrap();
        }
        let names: Vec<_> = attachments_of(&e, &db, a_task)
            .into_iter()
            .map(|a| a.filename)
            .collect();
        assert_eq!(
            names,
            vec!["first.pdf".to_string(), "second.pdf".to_string()]
        );
        assert_eq!(attachments_of(&e, &db, b_task).len(), 1);
    }

    #[test]
    fn task_attachments_rejects_a_non_task_id() {
        let db = db();
        let e = engine();
        assert!(e
            .query(
                &db,
                Query::TaskAttachments(EntityRef::new(EntityKind::Stream, [1u8; 16]))
            )
            .is_err());
    }

    // ---- auto-completion (issue #10) ----

    fn open_session(e: &Engine, db: &mut Db, task: EntityRef) -> EntityRef {
        e.apply(
            db,
            Command::StartFocus(FocusStartDraft {
                task_id: task,
                kind: FocusKind::Work,
                length: SessionLength::OnePomodoro,
                energy: None,
            }),
        )
        .unwrap()
        .entity
    }

    fn end_session(e: &Engine, db: &mut Db, session: EntityRef, done: bool) -> CommandResult {
        e.apply(
            db,
            Command::EndFocus {
                session,
                actual_focused_ms: None,
                completed_task: done,
            },
        )
        .unwrap()
    }

    fn task_of(e: &Engine, db: &Db, id: EntityRef) -> Task {
        match e.query(db, Query::EntityById(id)).unwrap() {
            QueryResult::Task(t) => *t,
            other => panic!("expected task, got {other:?}"),
        }
    }

    /// The signal: a session closed with `completed_task: true` completes the
    /// task it was opened on, and stamps `completed_at`.
    #[test]
    fn a_session_that_says_it_finished_the_task_completes_it() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let session = open_session(&e, &mut db, task);
        let res = end_session(&e, &mut db, session, true);

        let t = task_of(&e, &db, task);
        assert_eq!(t.state, TaskState::Done);
        assert!(t.completed_at.is_some(), "completed_at is stamped");
        assert_eq!(
            res.state,
            Some(TaskState::Done),
            "the caller learns the task moved"
        );
    }

    /// A session closed WITHOUT the flag changes nothing about the task. The
    /// signal is the user's statement, not the session's existence.
    #[test]
    fn ending_a_session_without_the_flag_leaves_the_task_open() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let session = open_session(&e, &mut db, task);
        let res = end_session(&e, &mut db, session, false);

        let t = task_of(&e, &db, task);
        assert_eq!(t.state, TaskState::Todo);
        assert!(t.completed_at.is_none());
        assert_eq!(res.state, None);
    }

    /// A task the user cancelled is left alone: `cancelled -> done` is not a
    /// legal transition, and a session must not overrule the user.
    #[test]
    fn a_cancelled_task_is_not_auto_completed() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let session = open_session(&e, &mut db, task);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    state: Some(TaskState::Cancelled),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        end_session(&e, &mut db, session, true);
        assert_eq!(task_of(&e, &db, task).state, TaskState::Cancelled);
    }

    /// An already-done task is a no-op: no second `task.update` op, so a client
    /// that both completes the task and closes the session with the flag does
    /// not write the completion twice.
    #[test]
    fn an_already_done_task_produces_no_second_op() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let session = open_session(&e, &mut db, task);
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();
        let before = op_count(&db);
        end_session(&e, &mut db, session, true);
        assert_eq!(
            op_count(&db) - before,
            1,
            "only the focus.end op; the task was already done"
        );
    }

    /// A deleted task is a no-op rather than an error: a session closing over
    /// a task that was tombstoned elsewhere must still close.
    #[test]
    fn a_deleted_task_is_not_resurrected() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let session = open_session(&e, &mut db, task);
        e.apply(&mut db, Command::DeleteTask(task)).unwrap();
        end_session(&e, &mut db, session, true);
        let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
        assert!(t.deleted);
        assert_eq!(t.state, TaskState::Todo);
    }

    /// The op the completion emits is an ordinary full-state `task.update`,
    /// which is what makes it converge like any hand-edit.
    #[test]
    fn the_completion_is_an_ordinary_task_update_op() {
        let mut db = db();
        let e = engine();
        let task = new_task(&e, &mut db, "Write the report");
        let session = open_session(&e, &mut db, task);
        end_session(&e, &mut db, session, true);
        let env = env_for_kind(&db, task.bytes(), "task.update");
        assert!(!env.is_empty(), "a task.update op was emitted");
    }

    // ---- notification reads (issue #9) ----

    use sunrise_domain::{EndOfDayPlan, MorningSummary};

    /// 2026-03-04 12:00 UTC, and the civil midnights around it.
    const NOTIFY_NOON: i64 = 1_772_625_600_000;

    fn engine_at(now_ms: i64) -> (Engine, Db) {
        let e = engine_seeded(
            ROOT,
            [1u8; 32],
            Arc::new(FakeClock(PLMutex::new(now_ms as u64))),
        );
        (e, db_root(ROOT))
    }

    fn morning(e: &Engine, db: &Db, now_ms: i64) -> MorningSummary {
        match e
            .query(
                db,
                Query::MorningSummary {
                    now_ms: now_ms as u64,
                },
            )
            .unwrap()
        {
            QueryResult::MorningSummary(s) => *s,
            other => panic!("expected a morning summary, got {other:?}"),
        }
    }

    fn evening(e: &Engine, db: &Db, now_ms: i64) -> EndOfDayPlan {
        match e
            .query(
                db,
                Query::EndOfDayPlan {
                    now_ms: now_ms as u64,
                },
            )
            .unwrap()
        {
            QueryResult::EndOfDayPlan(p) => *p,
            other => panic!("expected an end-of-day plan, got {other:?}"),
        }
    }

    fn reminders(
        e: &Engine,
        db: &Db,
        now_ms: i64,
        horizon_ms: i64,
        settings: ReminderSettings,
    ) -> Vec<sunrise_domain::ReminderIntent> {
        match e
            .query(
                db,
                Query::ReminderIntents {
                    now_ms: now_ms as u64,
                    horizon_ms: horizon_ms as u64,
                    settings,
                },
            )
            .unwrap()
        {
            QueryResult::Reminders(r) => r,
            other => panic!("expected reminders, got {other:?}"),
        }
    }

    #[test]
    fn the_morning_summary_reads_the_previous_calendar_date() {
        let (e, mut db) = engine_at(NOTIFY_NOON);
        // Completed yesterday afternoon.
        let done = new_task(&e, &mut db, "Ship the thing");
        e.apply(&mut db, Command::CompleteTask(done)).unwrap();
        // Sitting in the Inbox, awaiting a decision.
        let inbox = new_task(&e, &mut db, "Read this later");
        // Filed and scheduled for next month, so on neither list.
        let later = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Renew passport".into(),
                    stream_id: Some(stream_ref(3)),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(
                        NOTIFY_NOON + 30 * 86_400_000,
                    ))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;

        let s = morning(&e, &db, NOTIFY_NOON);
        assert_eq!(
            s.completed.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![done]
        );
        assert_eq!(
            s.to_triage.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![inbox],
            "an Inbox task is the triage queue; a filed one is not"
        );
        assert!(s.due_today.iter().all(|t| t.id != later));
        // Yesterday's midnight, and today's, in the device zone (UTC here).
        assert_eq!(s.today_start.as_millisecond(), 1_772_582_400_000);
        assert_eq!(s.since.as_millisecond(), 1_772_496_000_000);
    }

    #[test]
    fn the_end_of_day_plan_covers_today_then_the_week() {
        let (e, mut db) = engine_at(NOTIFY_NOON);
        let day = 86_400_000i64;
        let today = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "This evening".into(),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 6 * 3_600_000))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let midweek = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Thursday".into(),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 3 * day))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let far = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Next month".into(),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 30 * day))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let floating = new_task(&e, &mut db, "Someday");

        let p = evening(&e, &db, NOTIFY_NOON);
        assert_eq!(
            p.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![today]
        );
        assert_eq!(
            p.week_ahead.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![midweek],
            "the week ahead is the seven civil days after today"
        );
        assert!(p.week_ahead.iter().all(|t| t.id != far));
        assert_eq!(
            p.unscheduled.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![floating]
        );
    }

    #[test]
    fn a_scheduled_task_and_a_block_both_produce_reminders() {
        let (e, mut db) = engine_at(NOTIFY_NOON);
        let at = NOTIFY_NOON + 2 * 3_600_000;
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Call the bank".into(),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(at))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(BlockDraft {
                    stream_id: inbox_stream_ref(),
                    starts_at: SunriseTime::instant(ms_to_ts(at)),
                    ends_at: SunriseTime::instant(ms_to_ts(at + 3_600_000)),
                    title: Some("Deep work".into()),
                    title_track_task: false,
                    tasks: Vec::new(),
                }),
            )
            .unwrap()
            .entity;

        let out = reminders(
            &e,
            &db,
            NOTIFY_NOON,
            NOTIFY_NOON + 86_400_000,
            ReminderSettings::default(),
        );
        assert_eq!(out.len(), 2);
        // The block leads by 15 minutes, so it sorts first.
        assert_eq!(out[0].entity, block);
        assert_eq!(out[0].fire_at.as_millisecond(), at - 900_000);
        assert_eq!(out[0].title, "Deep work");
        assert_eq!(out[1].entity, task);
        assert_eq!(out[1].fire_at.as_millisecond(), at);
    }

    /// The lead-time hierarchy end to end: the Task's own value beats the
    /// Stream's, which beats the device default.
    #[test]
    fn the_lead_time_hierarchy_resolves_against_real_rows() {
        let (e, mut db) = engine_at(NOTIFY_NOON);
        let at = NOTIFY_NOON + 6 * 3_600_000;
        let stream = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    reminder_lead_s: Some(1_800),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let inherits = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Inherits the stream's".into(),
                    stream_id: Some(stream),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(at))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let overrides = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Sets its own".into(),
                    stream_id: Some(stream),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(at))),
                    reminder_lead_s: Some(60),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let global = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Falls all the way through".into(),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(at))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;

        let settings = ReminderSettings {
            default_lead_s: 300,
            ..ReminderSettings::default()
        };
        let out = reminders(&e, &db, NOTIFY_NOON, NOTIFY_NOON + 86_400_000, settings);
        let fired = |id: EntityRef| {
            out.iter()
                .find(|r| r.entity == id)
                .unwrap_or_else(|| panic!("no reminder for {id}"))
                .fire_at
                .as_millisecond()
        };
        assert_eq!(fired(overrides), at - 60_000, "per-task wins");
        assert_eq!(fired(inherits), at - 1_800_000, "per-stream next");
        assert_eq!(fired(global), at - 300_000, "device default last");
    }

    #[test]
    fn a_completed_task_stops_producing_reminders() {
        let (e, mut db) = engine_at(NOTIFY_NOON);
        let at = NOTIFY_NOON + 3_600_000;
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "Call the bank".into(),
                    scheduled_at: Some(SunriseTime::instant(ms_to_ts(at))),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        assert_eq!(
            reminders(
                &e,
                &db,
                NOTIFY_NOON,
                NOTIFY_NOON + 86_400_000,
                ReminderSettings::default()
            )
            .len(),
            1
        );
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();
        assert!(reminders(
            &e,
            &db,
            NOTIFY_NOON,
            NOTIFY_NOON + 86_400_000,
            ReminderSettings::default()
        )
        .is_empty());
    }

    /// The spec's dedup rule, enforced before a single row is read.
    #[test]
    fn a_non_primary_device_is_handed_nothing_to_schedule() {
        let (e, mut db) = engine_at(NOTIFY_NOON);
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "Call the bank".into(),
                scheduled_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 3_600_000))),
                ..Default::default()
            }),
        )
        .unwrap();
        let settings = ReminderSettings {
            is_primary_device: false,
            ..ReminderSettings::default()
        };
        assert!(reminders(&e, &db, NOTIFY_NOON, NOTIFY_NOON + 86_400_000, settings).is_empty());
    }

    // ---- apply_remote (receive half of sync) tests ----
    /// Convergence: the completion is derived ONCE, on the originating device.
    /// A replica that applies only the `focus.end` op derives nothing — so the
    /// task moves exactly when the `task.update` arrives, and never twice.
    #[test]
    fn a_replica_does_not_re_derive_the_completion_from_the_focus_op() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let task = new_task(&ea, &mut dba, "Write the report");
        let session = open_session(&ea, &mut dba, task);
        eb.apply_remote(&mut dbb, &create_env_for(&dba, task.bytes()))
            .unwrap();
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, session.bytes(), "focus.start"),
        )
        .unwrap();

        end_session(&ea, &mut dba, session, true);

        // Only the focus.end crosses. B sees the session close and the task
        // stay open: the derivation is not re-run here.
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, session.bytes(), "focus.end"))
            .unwrap();
        assert_eq!(
            read_task(dbb.conn(), task.bytes()).unwrap().unwrap().state,
            TaskState::Todo,
            "a focus.end op is not itself a completion"
        );

        // The derived task op is what moves it, through the ordinary LWW path.
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.update"))
            .unwrap();
        let a = read_task(dba.conn(), task.bytes()).unwrap().unwrap();
        let b = read_task(dbb.conn(), task.bytes()).unwrap().unwrap();
        assert_eq!(b.state, TaskState::Done);
        assert_eq!(a.completed_at, b.completed_at, "one instant, not two");
        assert_eq!(
            row_stamp(&dba, "tasks", "id", &task),
            row_stamp(&dbb, "tasks", "id", &task),
        );
    }

    /// `docs/08-features/focus-mode.md`: "a focus session that completes a
    /// routine occurrence increments the routine streak" — and the advance
    /// converges, because it rides in the same transaction as its completion.
    #[test]
    fn completing_a_routine_occurrence_through_focus_advances_the_streak() {
        let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let (rid, occ) = seed_routine(&ea, &mut dba);
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
            .unwrap();
        eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
            .unwrap();

        // Both replicas move together: an hour of real skew between peers is
        // outside the HLC drift window on purpose, and this test is about the
        // streak, not about clock abuse.
        set_clock(&ca, (occ[0].1 + 3_600_000) as u64);
        set_clock(&cb, (occ[0].1 + 3_600_000) as u64);
        let session = open_session(&ea, &mut dba, occ[0].0);
        end_session(&ea, &mut dba, session, true);
        assert_eq!(routine_of(&ea, &dba, rid).streak_counter, 1);

        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, occ[0].0.bytes(), "task.update"),
        )
        .unwrap();
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
            .unwrap();
        assert_eq!(routine_of(&eb, &dbb, rid).streak_counter, 1);
        assert_eq!(
            row_stamp(&dba, "routines", "id", &rid),
            row_stamp(&dbb, "routines", "id", &rid),
        );
    }

    /// Attachment metadata that arrives before its parent task still lands:
    /// nothing about the row depends on the parent existing, and the two join
    /// up when the task's own op turns up.
    #[test]
    fn an_attachment_op_that_overtakes_its_task_still_materializes() {
        let clock_a = Arc::new(FakeClock(PLMutex::new(T0)));
        let clock_b = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], clock_a);
        let eb = engine_seeded(ROOT, [2u8; 32], clock_b);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let task = new_task(&ea, &mut dba, "File the expenses");
        let att = ea
            .apply(&mut dba, Command::AttachFile(attachment_draft(task)))
            .unwrap();

        // Only the attachment op crosses; the task's create stays behind.
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &att.op_id))
            .unwrap();
        assert!(
            attachments_of(&eb, &dbb, task).len() == 1,
            "the attachment materializes without its parent"
        );

        // The task arrives afterwards and the two agree.
        eb.apply_remote(&mut dbb, &create_env_for(&dba, task.bytes()))
            .unwrap();
        let rows = attachments_of(&eb, &dbb, task);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].blob_key, [7u8; 32]);
        assert_eq!(rows[0].content_hash, [11u8; 32]);
    }

    use crate::events::DomainEvent;

    fn db_root(root: [u8; 32]) -> Db {
        Db::open_memory(&VaultRootKey::from_bytes(root)).unwrap()
    }

    fn engine_seeded(root: [u8; 32], seed: [u8; 32], clock: Arc<FakeClock>) -> Engine {
        let kc = Arc::new(Keychain::for_test_seeded(
            VaultRootKey::from_bytes(root),
            seed,
        ));
        Engine::from_clock(clock, Arc::new(SystemRng), kc)
    }

    /// Like [`engine_seeded`] but with **real random** Stream keys, so a key
    /// this device does not hold is genuinely one it cannot open.
    ///
    /// Every other engine unit test derives its keys from the shared vault root
    /// (see [`Keychain::for_test_seeded`]) precisely so two in-memory engines
    /// can read each other with no relay to carry `key_envelope` ops. That is
    /// the wrong world for the deferral path, whose whole subject is an op
    /// arriving before the key that opens it.
    fn engine_random_keys(root: [u8; 32], seed: [u8; 32], clock: Arc<FakeClock>) -> Engine {
        let kc = Arc::new(Keychain::for_test_random_keys(
            VaultRootKey::from_bytes(root),
            seed,
        ));
        Engine::from_clock(clock, Arc::new(SystemRng), kc)
    }

    /// Hand `receiver` the `(epoch, key)` `sender` holds for `stream_id`.
    ///
    /// This is what pairing does for real — the payload carries every Stream
    /// key the inviting device holds — compressed into one call, because these
    /// engines have no relay and no Noise channel between them.
    fn hand_over_key(
        receiver: &Engine,
        rdb: &mut Db,
        sender: &Engine,
        sdb: &mut Db,
        stream: &[u8; 16],
    ) {
        let (epoch, key) = sdb
            .with_tx(|tx| sender.keychain.current_stream_key_tx(tx, stream))
            .unwrap()
            .expect("the sender holds a key for this stream");
        rdb.with_tx(|tx| {
            receiver.keychain.absorb_stream_key(
                tx,
                stream,
                epoch,
                &key,
                KeySource::Pairing,
                receiver.rng.as_ref(),
                T0,
            )?;
            Ok(())
        })
        .unwrap();
    }

    /// Every sealed `key.envelope` op `db` holds, oldest first.
    ///
    /// Not narrowed by stream: a control op is logged with no `target_id` (it
    /// names no entity of its own), so which stream's key one carries is
    /// visible only inside the sealed payload. The caller applies them all,
    /// which is what a replica does anyway.
    fn key_envelope_envs(db: &Db) -> Vec<Vec<u8>> {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT op_id FROM ops
                 WHERE inner_kind = 'key.envelope'
                 ORDER BY rowid ASC",
            )
            .unwrap();
        let ids: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        ids.iter().map(|id| env_bytes(db, &to16(id))).collect()
    }

    fn deferred_rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM deferred_ops", [], |r| r.get(0))
            .unwrap()
    }

    /// Apply a `device_revoke` for `target`, effective at `effective_at_ms`,
    /// through the same code path the op takes when it arrives over sync.
    ///
    /// The declaring op's HLC is `effective_at_ms` too, which is what an
    /// honest emitter produces and what the cut's bounds are measured against.
    fn revoke(
        receiver: &Engine,
        db: &mut Db,
        sender: &Engine,
        target: [u8; 16],
        effective_at_ms: u64,
    ) {
        revoke_at(
            receiver,
            db,
            sender,
            target,
            effective_at_ms,
            effective_at_ms,
        );
    }

    /// [`revoke`] with the declaring op's HLC chosen separately from the cut.
    fn revoke_at(
        receiver: &Engine,
        db: &mut Db,
        sender: &Engine,
        target: [u8; 16],
        effective_at_ms: u64,
        hlc_ms: u64,
    ) {
        let sender_id = sender.keychain.device_id();
        let inner = InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: target,
            reason_code: RevokeReason::Lost,
            effective_at_ms,
        });
        db.with_tx(|tx| {
            receiver
                .apply_control_op(tx, &inner, &sender_id, hlc_ms, effective_at_ms)
                .map(|_| ())
        })
        .unwrap();
    }

    /// An op that arrives before the key that opens it is parked, not lost, and
    /// the `key_envelope` op carrying that key releases it.
    ///
    /// The two halves are both assertions. Parking must be **silent**: no
    /// event, no error, and no entity — a `RemoteOpInvalid` here would make the
    /// driver treat a perfectly good op as a damaged frame and ask for a
    /// resync it cannot be helped by. Draining must be **complete**: the
    /// envelope op itself changes nothing on screen, so if the delivery did not
    /// hand back the released op's event the screen would never learn about a
    /// task the database now holds.
    #[test]
    fn an_op_that_arrives_before_its_key_is_parked_and_the_envelope_releases_it() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
        let eb = engine_random_keys(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        // Each knows the other's identity-signed cert, so B knows A's key to
        // verify with and A knows B's `D_D_pub` to seal an envelope to.
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        // A captures a task. This mints the Inbox key and the vault-meta key,
        // and emits a `key_envelope` op per recipient for each.
        let task = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "buy milk".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;

        // B was paired, so it holds the vault-meta key and can read control
        // ops. It was never told the Inbox key.
        hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
        assert!(
            eb.keychain
                .stream_keys_at(&sunrise_domain::INBOX_STREAM_BYTES, 1)
                .is_empty(),
            "the premise: B holds no Inbox key"
        );

        // The task op arrives first.
        let create_env = create_env_for(&dba, task.bytes());
        let events = eb.apply_remote_all(&mut dbb, &create_env).unwrap();
        assert!(events.is_empty(), "parking is silent");
        assert_eq!(deferred_rows(&dbb), 1, "and it is parked, not dropped");
        assert!(
            read_task(dbb.conn(), task.bytes()).unwrap().is_none(),
            "nothing materialized from an op nobody could open"
        );

        // Re-delivery of the same op must not pile up a second row.
        assert!(eb
            .apply_remote_all(&mut dbb, &create_env)
            .unwrap()
            .is_empty());
        assert_eq!(deferred_rows(&dbb), 1, "parking is idempotent on op id");

        // Now A's `key_envelope` ops. One per (stream, recipient); the delivery
        // that lands B's copy of the Inbox key is the one that releases the
        // task, and the rest are silent.
        let mut released = Vec::new();
        let envs = key_envelope_envs(&dba);
        assert!(!envs.is_empty(), "A emitted envelopes to seal to B at all");
        for env in envs {
            released.extend(eb.apply_remote_all(&mut dbb, &env).unwrap());
        }
        assert!(
            matches!(released.as_slice(), [DomainEvent::Created(r)] if *r == task),
            "the envelope op is silent; the op it released is not, got {released:?}"
        );
        assert_eq!(deferred_rows(&dbb), 0, "the park is emptied by the drain");
        assert_eq!(
            read_task_t(&eb, &dbb, task).title,
            "buy milk",
            "and the task is really there"
        );
    }

    fn set_clock(c: &FakeClock, v: u64) {
        *c.0.lock() = v;
    }

    fn env_bytes(db: &Db, op_id: &[u8; 16]) -> Vec<u8> {
        OpLog::get_envelope(db, op_id).unwrap().unwrap()
    }

    /// Sealed envelope of the most recent `task.create` op targeting `target`.
    fn create_env_for(db: &Db, target: &[u8; 16]) -> Vec<u8> {
        let op_id: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT op_id FROM ops
                 WHERE target_id = ? AND inner_kind = 'task.create'
                 ORDER BY rowid DESC LIMIT 1",
                params![&target[..]],
                |r| r.get(0),
            )
            .unwrap();
        let mut a = [0u8; 16];
        a.copy_from_slice(&op_id[..16]);
        env_bytes(db, &a)
    }

    fn read_task_t(e: &Engine, db: &Db, id: EntityRef) -> Task {
        match e.query(db, Query::EntityById(id)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!("expected task"),
        }
    }

    /// Put `sender` in `receiver`'s device list, through the same code path a
    /// `device_cert` op takes when it arrives over sync.
    ///
    /// Replaces the old `Command::TrustDevice` submit. Trust is no longer
    /// something a caller hands the engine — a device publishes its
    /// identity-signed cert as an op — so a unit test with no relay applies
    /// that op's effect directly.
    ///
    /// It carries no Stream keys, and does not need to: engine unit tests run
    /// on [`Keychain::for_test_seeded`], whose keys are derived from the shared
    /// vault root precisely so two in-memory engines can read each other with
    /// no relay to carry `key_envelope` ops. See that constructor's docs.
    fn trust(receiver: &Engine, db: &mut Db, sender: &Engine) {
        let cert = sender.keychain.cert_blob().to_vec();
        let sender_id = sender.keychain.device_id();
        db.with_tx(|tx| {
            receiver
                .apply_control_op(tx, &InnerOp::DeviceCertPublish(cert), &sender_id, 0, 0)
                .map(|_| ())
        })
        .unwrap();
    }

    const ROOT: [u8; 32] = [0x5a; 32];
    const T0: u64 = 1_700_000_000_000;

    /// Sealed envelope of the most recent op of `kind` targeting `target`.
    fn env_for_kind(db: &Db, target: &[u8; 16], kind: &str) -> Vec<u8> {
        let op_id: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT op_id FROM ops
                 WHERE target_id = ? AND inner_kind = ?
                 ORDER BY rowid DESC LIMIT 1",
                params![&target[..], kind],
                |r| r.get(0),
            )
            .unwrap();
        let mut a = [0u8; 16];
        a.copy_from_slice(&op_id[..16]);
        env_bytes(db, &a)
    }

    #[test]
    fn remote_context_create_and_delete_converge() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        // A creates a context and tags a task with it; B receives both.
        let ctx = new_context(&ea, &mut dba, "errands");
        let task = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "buy milk".into(),
                    contexts: vec![ctx],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;

        let create_env = env_for_kind(&dba, ctx.bytes(), "context.create");
        assert!(matches!(
            eb.apply_remote(&mut dbb, &create_env).unwrap(),
            Some(DomainEvent::Created(r)) if r == ctx
        ));
        eb.apply_remote(&mut dbb, &create_env_for(&dba, task.bytes()))
            .unwrap();

        let rows = context_rows(&eb, &dbb);
        assert_eq!(rows.len(), 1, "the context reached B");
        assert_eq!(rows[0].name, "errands");
        assert_eq!(rows[0].task_count, 1, "and B sees the membership");
        // Re-delivery is a no-op.
        assert!(eb.apply_remote(&mut dbb, &create_env).unwrap().is_none());

        // A renames it; B converges under LWW.
        set_clock(&ca, T0 + 1_000);
        ea.apply(
            &mut dba,
            Command::UpdateContext {
                id: ctx,
                patch: ContextPatch {
                    name: Some("errands & chores".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let upd_env = env_for_kind(&dba, ctx.bytes(), "context.update");
        assert!(matches!(
            eb.apply_remote(&mut dbb, &upd_env).unwrap(),
            Some(DomainEvent::Updated(r)) if r == ctx
        ));
        assert_eq!(context_rows(&eb, &dbb)[0].name, "errands & chores");

        // A deletes it: B must both drop the context AND strip it from the task.
        set_clock(&ca, T0 + 2_000);
        ea.apply(&mut dba, Command::DeleteContext(ctx)).unwrap();
        let del_env = env_for_kind(&dba, ctx.bytes(), "context.delete");
        assert!(matches!(
            eb.apply_remote(&mut dbb, &del_env).unwrap(),
            Some(DomainEvent::Deleted(r)) if r == ctx
        ));
        assert!(context_rows(&eb, &dbb).is_empty(), "gone from B's list");
        assert!(
            task_context_ids(&dbb, task).is_empty(),
            "and stripped from B's copy of the task, without an extra task op"
        );
        assert_eq!(
            task_context_ids(&dba, task),
            task_context_ids(&dbb, task),
            "both replicas agree"
        );
    }

    /// A `ContextDelete` that overtakes its own `ContextCreate` materializes
    /// the tombstone anyway.
    ///
    /// This was the last delete op still missing its insert-when-absent branch.
    /// The failure was worse than a plain dropped delete: the membership purge
    /// is keyed on the id alone and ran regardless, so B stripped the context
    /// off its tasks and then let the older create land the Context *alive* —
    /// a live context nothing is tagged with, on a replica whose peer has no
    /// such context at all, and no later op to reconcile them.
    #[test]
    fn a_context_delete_that_overtakes_its_create_still_lands_as_a_tombstone() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let ctx = new_context(&ea, &mut dba, "errands");
        set_clock(&ca, T0 + 1_000);
        ea.apply(&mut dba, Command::DeleteContext(ctx)).unwrap();

        // Delete first, create second.
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, ctx.bytes(), "context.delete"))
            .unwrap();
        let rb = read_context(dbb.conn(), ctx.bytes())
            .unwrap()
            .expect("the delete materialized a row of its own");
        assert!(rb.deleted);
        assert_eq!(rb.name, "errands", "and it carries the full state");

        eb.apply_remote(&mut dbb, &env_for_kind(&dba, ctx.bytes(), "context.create"))
            .unwrap();
        assert!(
            read_context(dbb.conn(), ctx.bytes())
                .unwrap()
                .unwrap()
                .deleted,
            "the older create loses LWW and does not resurrect it"
        );
        assert!(
            context_rows(&eb, &dbb).is_empty(),
            "and it stays out of the context list"
        );
        assert_eq!(
            row_stamp(&dba, "contexts", "id", &ctx),
            row_stamp(&dbb, "contexts", "id", &ctx),
            "both replicas agree on the winning stamp"
        );
    }

    #[test]
    fn concurrent_context_renames_converge_to_one_name() {
        // Both replicas rename the same context at the same instant; the tie is
        // broken by device id, and both must land on the same winner.
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        let ctx = new_context(&ea, &mut dba, "errands");
        let create_env = env_for_kind(&dba, ctx.bytes(), "context.create");
        eb.apply_remote(&mut dbb, &create_env).unwrap();

        set_clock(&ca, T0 + 5_000);
        set_clock(&cb, T0 + 5_000);
        let rename = |name: &str| Command::UpdateContext {
            id: ctx,
            patch: ContextPatch {
                name: Some(name.into()),
                ..Default::default()
            },
        };
        ea.apply(&mut dba, rename("from A")).unwrap();
        eb.apply(&mut dbb, rename("from B")).unwrap();

        let a_env = env_for_kind(&dba, ctx.bytes(), "context.update");
        let b_env = env_for_kind(&dbb, ctx.bytes(), "context.update");
        eb.apply_remote(&mut dbb, &a_env).unwrap();
        ea.apply_remote(&mut dba, &b_env).unwrap();

        let name_a = context_rows(&ea, &dba)[0].name.clone();
        let name_b = context_rows(&eb, &dbb)[0].name.clone();
        assert_eq!(name_a, name_b, "both replicas picked the same winner");
        assert!(name_a == "from A" || name_a == "from B", "{name_a}");
    }

    /// Two devices reorder the same list while apart. They converge — and they
    /// converge on **one device's arrangement**, not on a merge of the two.
    ///
    /// This is `docs/02-domain/streams.md` §Merge mapping made executable.
    /// `sort_order` is one field on an entity-level LWW row (ADR-0014), so the
    /// losing device's drag is discarded whole. If per-field registers ever
    /// land, this test is where the change in behaviour shows up first.
    #[test]
    fn concurrent_stream_reorders_converge_on_one_arrangement_not_a_merge() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        // One stream, known to both.
        let s = ea
            .apply(
                &mut dba,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, s.bytes(), "stream.create"))
            .unwrap();

        // Both devices drag it somewhere, at the same instant, in ignorance of
        // each other. The tie breaks on device id.
        set_clock(&ca, T0 + 5_000);
        set_clock(&cb, T0 + 5_000);
        let move_to = |key: &str| Command::UpdateStream {
            id: s,
            patch: StreamPatch {
                sort_order: Some(key.into()),
                ..Default::default()
            },
        };
        ea.apply(&mut dba, move_to("B")).unwrap();
        eb.apply(&mut dbb, move_to("Y")).unwrap();

        let a_env = env_for_kind(&dba, s.bytes(), "stream.update");
        let b_env = env_for_kind(&dbb, s.bytes(), "stream.update");
        eb.apply_remote(&mut dbb, &a_env).unwrap();
        ea.apply_remote(&mut dba, &b_env).unwrap();

        let key_of = |e: &Engine, db: &Db| match e.query(db, Query::StreamList).unwrap() {
            QueryResult::Streams(rows) => rows
                .iter()
                .find(|r| r.id == s)
                .expect("the stream is still there")
                .sort_order
                .clone(),
            other => panic!("expected Streams, got {other:?}"),
        };
        let a = key_of(&ea, &dba);
        let b = key_of(&eb, &dbb);
        assert_eq!(a, b, "the two replicas disagree about the order");
        // Exactly one of the two drags survived. Not something between them:
        // there is no interleave to land on, which is the warning the spec
        // carries and the behaviour it describes.
        assert!(a == "B" || a == "Y", "invented a third position: {a}");
    }

    #[test]
    fn apply_remote_round_trip_materializes_identically() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "shared task".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);

        let ev = eb.apply_remote(&mut dbb, &env).unwrap();
        assert!(matches!(ev, Some(DomainEvent::Created(id)) if id == res.entity));

        // Field-by-field identical materialization on the receiver.
        let ta = read_task_t(&ea, &dba, res.entity);
        let tb = read_task_t(&eb, &dbb, res.entity);
        assert_eq!(ta, tb);
    }

    #[test]
    fn apply_remote_is_idempotent() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "once".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);

        assert!(eb.apply_remote(&mut dbb, &env).unwrap().is_some());
        let task_after_first = read_task_t(&eb, &dbb, res.entity);
        let ops_after_first = op_count(&dbb);

        // Second application is a no-op: Ok(None), row unchanged, no new ops row.
        assert!(eb.apply_remote(&mut dbb, &env).unwrap().is_none());
        assert_eq!(read_task_t(&eb, &dbb, res.entity), task_after_first);
        assert_eq!(op_count(&dbb), ops_after_first);
        assert_eq!(ops_after_first, 1, "exactly one remote op recorded");
    }

    /// The cut is a **time**, not a flag: an op a device signed before its
    /// revocation still applies on a replica that has already recorded the
    /// revocation.
    ///
    /// This is the scenario that made revocation retroactive. `A` writes at
    /// t=100 and is offline; `B` revokes it at t=200; the op reaches a third
    /// replica after the revocation does. `lookup_device_cert` used to filter
    /// `revoked_at_ms IS NULL`, so the row simply vanished, the op fell through
    /// to `self_authenticating_signer` — which knows only `DeviceCertPublish` —
    /// and came back `UnknownDevice` whatever its HLC said. Six months of
    /// honest work on a laptop the user retired yesterday, dropped, and
    /// `effective_at_ms` never consulted for any of the twenty domain families.
    #[test]
    fn an_op_signed_before_the_cut_applies_after_the_revocation_arrives() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        // A's op, written before anyone thought about revoking it.
        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "written before the cut".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);

        // B revokes A with a cut *after* that op's HLC, and only then receives
        // it — the ordering a device coming back from offline produces.
        revoke(&eb, &mut dbb, &eb, ea.keychain.device_id(), T0 + 1_000);
        let event = eb
            .apply_remote(&mut dbb, &env)
            .expect("a pre-cut op is not a revoked op");
        assert!(matches!(event, Some(DomainEvent::Created(r)) if r == res.entity));
        assert_eq!(
            read_task_t(&eb, &dbb, res.entity).title,
            "written before the cut"
        );
    }

    /// The other half of the same cut: an op signed at or after
    /// `effective_at_ms` is refused, and the cursor moves past it anyway.
    ///
    /// The refusal alone would be a second bug. A refused op never reaches
    /// `ops`, and the cursor is the contiguous *applied* prefix, so it would
    /// stop one seq short of that op forever — and the relay filters its replay
    /// by exactly that number, so it would re-send the op, and everything after
    /// it, on every reconnect, none of it ever advancing anything. Recording
    /// the refusal is what makes "do not send me this again" expressible.
    #[test]
    fn an_op_signed_after_the_cut_is_refused_and_the_cursor_passes_it() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let first = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "before".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let first_env = env_bytes(&dba, &first.op_id);
        let head = sunrise_cbor::decode_envelope_header(&first_env).unwrap();
        let (stream, device) = (head.stream_id, head.device_id);
        eb.apply_remote(&mut dbb, &first_env).unwrap();
        assert_eq!(cursor_for(&dbb, &stream, &device), 1);

        revoke(&eb, &mut dbb, &eb, device, T0 + 1);

        set_clock(&ca, T0 + 5_000);
        let after = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "after".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let after_env = env_bytes(&dba, &after.op_id);
        assert!(
            matches!(
                eb.apply_remote(&mut dbb, &after_env),
                Err(EngineError::DeviceRevoked)
            ),
            "an op past the cut must not apply"
        );
        assert!(
            read_task(dbb.conn(), after.entity.bytes())
                .unwrap()
                .is_none(),
            "and nothing of it materialized"
        );
        assert_eq!(
            cursor_for(&dbb, &stream, &device),
            2,
            "the refusal is a decision, so the cursor must pass it rather than \
             stall and have the relay replay it forever"
        );

        // Re-delivery is still a refusal, and still idempotent.
        assert!(matches!(
            eb.apply_remote(&mut dbb, &after_env),
            Err(EngineError::DeviceRevoked)
        ));
        assert_eq!(cursor_for(&dbb, &stream, &device), 2);
    }

    /// A member cannot republish a *sibling's* cert under its own signature.
    ///
    /// `ON CONFLICT(device_id) DO UPDATE` overwrites `cert_blob` — and with it
    /// the `d_s_pub` every one of that sibling's envelopes is verified against —
    /// and `d_d_pub`, which is where its `key_envelope`s are sealed. Publishing
    /// a cert for someone else is therefore a converging denial of service plus
    /// a redirect of their key material, from one well-formed op.
    #[test]
    fn a_device_cannot_publish_a_cert_naming_another_device() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&eb, &mut dbb, &ec);

        let stored = |db: &Db, id: &[u8; 16]| -> Vec<u8> {
            db.conn()
                .query_row(
                    "SELECT cert_blob FROM devices WHERE device_id = ?",
                    params![&id[..]],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let c_id = ec.keychain.device_id();
        let before = stored(&dbb, &c_id);

        // A issues a cert naming C, binding C's device id to **A's** DH key,
        // and publishes it. Pairing hands every device `ID_S_priv`, so this
        // cert verifies perfectly: the check that has to catch it is the one
        // about *who sent it*, not the one about whether it is well formed.
        let a_id = ea.keychain.device_id();
        let cert = ea.keychain.issue_cert_for(
            c_id,
            ec.keychain.device_signing_pub(),
            ea.keychain.device_dh_pub(),
            T0,
        );
        dbb.with_tx(|tx| {
            eb.apply_control_op(tx, &InnerOp::DeviceCertPublish(cert), &a_id, T0, T0)
                .map(|_| ())
        })
        .unwrap();
        assert_eq!(
            stored(&dbb, &c_id),
            before,
            "A must not be able to rewrite C's row"
        );
        let d_d: Vec<u8> = dbb
            .conn()
            .query_row(
                "SELECT d_d_pub FROM devices WHERE device_id = ?",
                params![&c_id[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            d_d,
            ec.keychain.device_dh_pub().to_vec(),
            "and C's future key envelopes still go to C"
        );

        // C publishing its own cert is of course fine, and is how the row is
        // written in the first place.
        let own = ec.keychain.cert_blob().to_vec();
        dbb.with_tx(|tx| {
            eb.apply_control_op(tx, &InnerOp::DeviceCertPublish(own.clone()), &c_id, T0, T0)
                .map(|_| ())
        })
        .unwrap();
        assert_eq!(stored(&dbb, &c_id), own);
    }

    /// An absorbed epoch far above this vault's own is refused.
    ///
    /// `MAX(epoch)` is what makes a key live, so one `key_envelope` at
    /// `u32::MAX` strands `mint_epoch` at its saturation point *and* redirects
    /// every op this device seals afterwards to a key only the sender holds.
    #[test]
    fn a_key_envelope_far_above_the_live_epoch_is_refused() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dbb = db_root(ROOT);
        let a_id = ea.keychain.device_id();
        let stream = [0x5c; 16];

        let mut absorb = |epoch: u32, key_byte: u8| {
            let key = StreamKey::from_bytes([key_byte; 32]);
            let sealed = eb
                .keychain
                .seal_key_envelope(
                    &eb.keychain.identity_dh_pub(),
                    &stream,
                    epoch,
                    &key,
                    eb.rng.as_ref(),
                )
                .unwrap();
            let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
                stream_id: stream,
                epoch,
                recipient: Recipient::Identity(eb.keychain.identity_id()),
                key_id: stream_key_id(&key),
                hpke_ciphertext: sealed,
            });
            dbb.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, T0, T0))
                .unwrap()
        };

        assert_eq!(
            absorb(1, 0xa1),
            vec![(stream, 1)],
            "an ordinary first epoch is absorbed"
        );
        assert_eq!(
            absorb(1 + MAX_EPOCH_LEAP, 0xa2),
            vec![(stream, 1 + MAX_EPOCH_LEAP)],
            "the far edge of the window is still inside it"
        );
        assert!(
            absorb(u32::MAX, 0xa3).is_empty(),
            "and an absurd epoch is refused"
        );
        assert!(
            eb.keychain.stream_keys_at(&stream, u32::MAX).is_empty(),
            "the refused key is not written, so it cannot become the live epoch"
        );
    }

    /// `deferred_ops` is bounded, and overflow evicts the oldest rows.
    ///
    /// Everything about a deferred op is unchecked — the key that would open it
    /// is exactly what is missing — and `epoch` is a free field of the signed
    /// envelope, so without a cap a member can park bytes of its choosing on
    /// every peer in the account at any `(stream, epoch)` it likes, forever.
    #[test]
    fn the_parked_op_buffer_is_bounded() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let eb = engine_random_keys(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        let over = usize::try_from(DEFERRED_PER_EPOCH_CAP).unwrap() + 16;
        let mut envs = Vec::with_capacity(over);
        for i in 0..over {
            set_clock(&ca, T0 + i as u64);
            let res = ea
                .apply(
                    &mut dba,
                    Command::CreateTask(TaskDraft {
                        title: format!("parked {i}"),
                        ..Default::default()
                    }),
                )
                .unwrap();
            envs.push(env_bytes(&dba, &res.op_id));
        }
        // B holds the vault-meta key but never the Inbox key, so every task op
        // A wrote parks on B. The hand-over is after the writes because the
        // first one is what mints the key at all.
        hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
        for (i, env) in envs.iter().enumerate() {
            // `received_at_ms` is the *receiver's* reading, so it is B's clock
            // that has to move for the rows to be orderable at all.
            set_clock(&cb, T0 + i as u64);
            assert!(eb.apply_remote_all(&mut dbb, env).unwrap().is_empty());
        }
        assert_eq!(
            deferred_rows(&dbb),
            DEFERRED_PER_EPOCH_CAP,
            "the bucket is capped, not unbounded"
        );

        // Oldest-first eviction: what survives is the newest window, because a
        // row that has waited longest is the one whose key is least likely to
        // still be in flight.
        let oldest: i64 = dbb
            .conn()
            .query_row("SELECT MIN(received_at_ms) FROM deferred_ops", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            oldest,
            i64::try_from(T0).unwrap() + 16,
            "the first 16 were the ones dropped"
        );
    }

    /// A pre-0017 op naming the Inbox by its old id — sixteen zero bytes, which
    /// are also the vault-meta stream — materializes into the Inbox, not into a
    /// `streams` row for the one stream that must never have one.
    ///
    /// Migration 0017 rewrites exactly these rows on a vault that upgrades. It
    /// cannot rewrite the signed envelopes, so a device paired *after* the
    /// upgrade replays that history and is the only other place the same rows
    /// can be born.
    #[test]
    fn a_pre_0017_inbox_op_lands_in_the_inbox_not_the_vault_meta_stream() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "captured before the split".into(),
                    ..Default::default()
                }),
            )
            .unwrap();

        // Rewrite the op's payload to name the Inbox the way a pre-0017 build
        // did, and re-seal it: the signature covers the payload, so a legacy op
        // has to be built rather than patched.
        let env = env_bytes(&dba, &res.op_id);
        let mut inner = decode_inner_op(&ea.keychain.open_op(&env).unwrap()).unwrap();
        let InnerOp::TaskCreate(task) = &mut inner else {
            panic!("expected a task.create");
        };
        task.stream_id = EntityRef::new(EntityKind::Stream, META_STREAM);
        let meta_key = dba
            .with_tx(|tx| ea.keychain.current_stream_key_tx(tx, &META_STREAM))
            .unwrap()
            .expect("A holds the vault-meta key");
        let legacy_env = ea
            .keychain
            .seal_op_at(
                META_STREAM,
                // A seq of its own in the vault-meta space, which is where a
                // pre-0017 Inbox op was logged: the two shared an id.
                900,
                ea.hlc.send(),
                &encode_inner_op(&inner).unwrap(),
                ea.rng.as_ref(),
                meta_key.0,
                &meta_key.1,
            )
            .unwrap();

        eb.apply_remote(&mut dbb, &legacy_env)
            .expect("a legacy Inbox op still applies");
        let stored: Vec<u8> = dbb
            .conn()
            .query_row(
                "SELECT stream_id FROM tasks WHERE id = ?",
                params![&res.entity.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            sunrise_domain::INBOX_STREAM_BYTES.to_vec(),
            "the task belongs to the Inbox, which is where 0017 put it locally"
        );
        let meta_rows: i64 = dbb
            .conn()
            .query_row(
                "SELECT count(*) FROM streams WHERE stream_id = ?",
                params![&META_STREAM[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            meta_rows, 0,
            "and the vault-meta stream still has no `streams` row"
        );
    }

    /// The `(revoked_at_ms, revoked_by, revoke_reason)` triple a replica holds
    /// for `device`. The whole triple, because convergence on the timestamp
    /// alone would still leave two replicas disagreeing about who did it.
    fn revocation_row(
        db: &Db,
        device: &[u8; 16],
    ) -> (Option<i64>, Option<Vec<u8>>, Option<String>) {
        db.conn()
            .query_row(
                "SELECT revoked_at_ms, revoked_by, revoke_reason FROM devices WHERE device_id = ?",
                params![&device[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// Two replicas, the same two revocation ops, opposite arrival orders, one
    /// final state — and it is the **earliest** cut, not the first to land.
    ///
    /// `WHERE revoked_at_ms IS NULL` made this first-writer-wins. That was
    /// harmless while any non-NULL value untrusted the device outright, and is
    /// not now that the column is compared against an envelope's HLC: the first
    /// revocation to arrive would be the only one that ever applied, so these
    /// two replicas would hold different cuts, refuse different ops, and one
    /// would write a permanent `refused_ops` row the other never would. That
    /// falsifies the property `record_refusal` is built on — that a refusal is
    /// a function of state every replica shares.
    #[test]
    fn concurrent_revocations_converge_on_the_earliest_cut_whatever_the_order() {
        let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
        let target = engine_seeded(ROOT, [1u8; 32], clock());
        let sooner = engine_seeded(ROOT, [2u8; 32], clock());
        let later = engine_seeded(ROOT, [3u8; 32], clock());
        let r1 = engine_seeded(ROOT, [4u8; 32], clock());
        let r2 = engine_seeded(ROOT, [5u8; 32], clock());
        let mut db1 = db_root(ROOT);
        let mut db2 = db_root(ROOT);
        trust(&r1, &mut db1, &target);
        trust(&r2, &mut db2, &target);
        let t = target.keychain.device_id();

        // The same two ops. r1 sees the later cut first; r2 sees it last.
        revoke(&r1, &mut db1, &later, t, T0 + 9_000);
        revoke(&r1, &mut db1, &sooner, t, T0 + 1_000);
        revoke(&r2, &mut db2, &sooner, t, T0 + 1_000);
        revoke(&r2, &mut db2, &later, t, T0 + 9_000);

        let row1 = revocation_row(&db1, &t);
        assert_eq!(
            row1,
            revocation_row(&db2, &t),
            "arrival order decided nothing"
        );
        assert_eq!(
            row1.0,
            Some(i64::try_from(T0).unwrap() + 1_000),
            "the earliest cut wins, not the one that landed first"
        );
        assert_eq!(
            row1.1.as_deref(),
            Some(&sooner.keychain.device_id()[..]),
            "and the columns that say who did it follow the winning cut"
        );

        // Which is the point: both replicas now make the same cut for the same
        // op, so neither can refuse what the other applies.
        for (e, db) in [(&r1, &db1), (&r2, &db2)] {
            assert!(e.is_revoked_at(db, &t, T0 + 5_000).unwrap());
            assert!(!e.is_revoked_at(db, &t, T0 + 500).unwrap());
        }
    }

    /// A cut far from the op that declares it is refused, so no revocation can
    /// immunise a device and none can reach backwards over its whole history.
    ///
    /// `u64::MAX` is the attack the `MIN` join would otherwise open: it sets a
    /// cut no realistic HLC reaches, and because the earliest cut wins every
    /// later genuine revocation converges onto it and refuses nothing. A cut of
    /// 0 is the mirror image — with `MIN`, one op retroactively invalidating
    /// everything a device ever wrote.
    #[test]
    fn a_revocation_cut_far_from_its_own_op_is_refused() {
        let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let sender = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let target = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut db = db_root(ROOT);
        trust(&er, &mut db, &target);
        let t = target.keychain.device_id();

        for cut in [u64::MAX, 0, T0 + REVOKE_CUT_AHEAD_MS + 1, T0 - 86_400_000] {
            revoke_at(&er, &mut db, &sender, t, cut, T0);
            assert_eq!(
                revocation_row(&db, &t).0,
                None,
                "a cut of {cut} against an op at {T0} must not land"
            );
        }

        // The documented forward case still works: `key-rotation.md` §Device
        // key rotation revokes the old key at `now + 24h`.
        revoke_at(&er, &mut db, &sender, t, T0 + 24 * 60 * 60 * 1000, T0);
        assert_eq!(
            revocation_row(&db, &t).0,
            Some(i64::try_from(T0).unwrap() + 24 * 60 * 60 * 1000)
        );
        // And a genuine, nearer cut still lowers it afterwards.
        revoke_at(&er, &mut db, &sender, t, T0 + 1_000, T0 + 1_000);
        assert!(er.is_revoked_at(&db, &t, T0 + 2_000).unwrap());
    }

    /// `Command::RevokeDevice` validates the cut too, against a tighter window
    /// than apply uses, so a device never emits a revocation its peers refuse.
    /// The field is `Option<u64>` and unchecked from `commands.rs` out to the
    /// Swift seam.
    #[test]
    fn a_revoke_command_refuses_a_cut_that_is_not_from_now_onwards() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        trust(&ea, &mut dba, &eb);
        let peer = EntityRef::new(EntityKind::Device, eb.keychain.device_id());

        for cut in [
            Some(0),
            Some(T0 - 1),
            Some(u64::MAX),
            Some(T0 + REVOKE_CUT_AHEAD_MS + 1),
        ] {
            let err = ea.apply(
                &mut dba,
                Command::RevokeDevice {
                    device_id: peer,
                    reason: RevokeReason::Lost,
                    effective_at_ms: cut,
                },
            );
            assert!(
                matches!(err, Err(EngineError::Invalid(_))),
                "cut {cut:?} should have been refused, got {err:?}"
            );
        }

        // The ordinary call — "now" — is accepted.
        ea.apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: peer,
                reason: RevokeReason::Lost,
                effective_at_ms: None,
            },
        )
        .expect("a revocation effective now is the whole point");
    }

    /// A revocation is the first control op a fresh vault emits, so resolving
    /// the vault-meta epoch mints that stream's own key mid-transaction and
    /// emits `key_envelope` ops into the very stream whose `seq` the revocation
    /// needs. A `seq` read before the transaction is already spent by then, and
    /// `ops` has a `UNIQUE(stream_id, device_id, seq)`.
    ///
    /// This is the only shape that fires it: a vault that has already written
    /// anything has a meta key, so nothing is minted and the stale read happens
    /// to be right.
    #[test]
    fn a_revocation_on_a_vault_with_no_meta_key_yet_gets_its_own_seq() {
        let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        // `trust` writes the peer's row directly, so the vault-meta stream
        // still holds no key and no op: exactly the premise.
        trust(&ea, &mut dba, &eb);
        assert_eq!(op_count(&dba), 0, "the premise: nothing written yet");

        ea.apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
                reason: RevokeReason::Stolen,
                effective_at_ms: None,
            },
        )
        .expect("revoking as the first act of a vault must work");

        let revokes: i64 = dba
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ops WHERE inner_kind = 'device.revoke'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(revokes, 1, "the revocation reached the op log");
        // Every op in the meta stream has a distinct seq. A stale read would
        // have collided with the `key_envelope` ops minting the epoch emitted.
        let (rows, distinct): (i64, i64) = dba
            .conn()
            .query_row(
                "SELECT COUNT(seq), COUNT(DISTINCT seq) FROM ops WHERE stream_id = ?",
                params![&META_STREAM[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(rows > 1, "the epoch mint emitted envelopes alongside it");
        assert_eq!(rows, distinct, "no two meta-stream ops share a seq");
    }

    fn refused_rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM refused_ops", [], |r| r.get(0))
            .unwrap()
    }

    /// A re-delivery of an op this replica already applied is silence, not a
    /// refusal — even once the sender is revoked with a cut below that op.
    ///
    /// The refusal sits at step c2, ahead of the idempotence gate at step e, so
    /// without an explicit check a replay would write a `refused_ops` entry for
    /// an op that is in `ops` and materialized, and answer `Err(DeviceRevoked)`
    /// where every other re-receive answers `Ok(None)`.
    #[test]
    fn a_replay_of_an_already_applied_op_is_not_a_refusal() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "applied before anyone revoked anything".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);
        eb.apply_remote(&mut dbb, &env).unwrap().expect("applied");

        // A revocation whose cut sits below that op's HLC — which the `MIN`
        // join makes reachable, since the cut only ever moves earlier.
        revoke_at(
            &eb,
            &mut dbb,
            &eb,
            ea.keychain.device_id(),
            T0 - 1_000,
            T0 + 1_000,
        );
        assert!(eb
            .is_revoked_at(&dbb, &ea.keychain.device_id(), T0)
            .unwrap());

        assert!(
            eb.apply_remote(&mut dbb, &env).unwrap().is_none(),
            "a re-receive is idempotent silence, not a refusal"
        );
        assert_eq!(refused_rows(&dbb), 0, "and nothing was recorded against it");
        assert_eq!(
            read_task_t(&eb, &dbb, res.entity).title,
            "applied before anyone revoked anything",
            "the op it already applied is untouched"
        );
    }

    /// Read the stored cursor for `(stream, device)`, or 0 if none.
    fn cursor_for(db: &Db, stream: &[u8; 16], device: &[u8; 16]) -> u64 {
        db.conn()
            .query_row(
                "SELECT last_applied_seq FROM sync_cursors
                 WHERE stream_id = ? AND device_id = ?",
                params![&stream[..], &device[..]],
                |row| row.get::<_, i64>(0),
            )
            .map(|v| u64::try_from(v).unwrap_or(0))
            .unwrap_or(0)
    }

    /// The relay filters its replay by this number, so it has to mean "I have
    /// everything through n". A hole must hold it back — otherwise the frame
    /// carrying the missing op is skipped on the next subscribe and the hole
    /// becomes permanent and invisible.
    #[test]
    fn a_cursor_stops_at_a_hole_rather_than_tracking_the_maximum() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        // Three ops from A, seqs 1..3 on the inbox stream.
        let mut envs = Vec::new();
        for title in ["one", "two", "three"] {
            let res = ea
                .apply(
                    &mut dba,
                    Command::CreateTask(TaskDraft {
                        title: title.into(),
                        ..Default::default()
                    }),
                )
                .unwrap();
            envs.push(env_bytes(&dba, &res.op_id));
        }
        let head = sunrise_cbor::decode_envelope_header(&envs[0]).unwrap();
        let (stream, device) = (head.stream_id, head.device_id);

        eb.apply_remote(&mut dbb, &envs[0]).unwrap();
        assert_eq!(cursor_for(&dbb, &stream, &device), 1);

        // Seq 2 is "lost"; seq 3 arrives. The cursor must stay at 1.
        eb.apply_remote(&mut dbb, &envs[2]).unwrap();
        assert_eq!(
            cursor_for(&dbb, &stream, &device),
            1,
            "a max-based cursor would claim 3 and strand seq 2 forever"
        );

        // The hole is filled: the prefix jumps to cover everything at once.
        eb.apply_remote(&mut dbb, &envs[1]).unwrap();
        assert_eq!(cursor_for(&dbb, &stream, &device), 3);
    }

    /// A device has certainly applied its own ops, so its own cursor belongs in
    /// `sync_cursors` — otherwise every Subscribe claims nothing about this
    /// device and the relay replays its whole history back to it.
    #[test]
    fn a_local_commit_advances_this_devices_own_cursor() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "mine".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let head = sunrise_cbor::decode_envelope_header(&env_bytes(&dba, &res.op_id)).unwrap();
        assert_eq!(cursor_for(&dba, &head.stream_id, &head.device_id), head.seq);
    }

    #[test]
    fn lww_earlier_remote_update_loses_to_later_local() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        // A creates; B receives the create.
        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "orig".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &res.op_id))
            .unwrap();

        // B updates locally at a LATER ts.
        set_clock(&cb, T0 + 10_000);
        eb.apply(
            &mut dbb,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    title: Some("B-late".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // A updates locally at an EARLIER ts, then that op arrives at B.
        set_clock(&ca, T0 + 5_000);
        let a_up = ea
            .apply(
                &mut dba,
                Command::UpdateTask {
                    id: res.entity,
                    patch: TaskPatch {
                        title: Some("A-early".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let ev = eb
            .apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
            .unwrap();
        // Op is applied (recorded + event emitted) but LWW keeps B's later value.
        assert!(matches!(ev, Some(DomainEvent::Updated(_))));
        assert_eq!(read_task_t(&eb, &dbb, res.entity).title, "B-late");
        // The losing op is still recorded in the op log.
        assert_eq!(op_count(&dbb), 3, "create + local update + remote update");
    }

    #[test]
    fn lww_later_remote_update_wins_over_earlier_local() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "orig".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &res.op_id))
            .unwrap();

        // B updates locally at an EARLIER ts.
        set_clock(&cb, T0 + 5_000);
        eb.apply(
            &mut dbb,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    title: Some("B-early".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // A updates at a LATER ts; that op wins on B.
        set_clock(&ca, T0 + 10_000);
        let a_up = ea
            .apply(
                &mut dba,
                Command::UpdateTask {
                    id: res.entity,
                    patch: TaskPatch {
                        title: Some("A-late".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
            .unwrap();
        assert_eq!(read_task_t(&eb, &dbb, res.entity).title, "A-late");
    }

    fn stamp(hlc_ms: u64, logical: u32, device: [u8; 16], seq: u64) -> LwwStamp {
        LwwStamp {
            hlc: Hlc {
                physical_ms: hlc_ms,
                logical,
            },
            device,
            seq,
        }
    }

    fn row_of(s: &LwwStamp) -> RowLww {
        RowLww {
            hlc: s.hlc,
            device: Some(s.device.to_vec()),
            seq: s.seq,
        }
    }

    /// Regression: a device's own successive ops must not lose to each other.
    ///
    /// The device-id memcmp breaks *cross-device* ties. Applied to one device's
    /// own ops it made the later op lose (`dev > dev` is false), so remote
    /// replicas silently dropped it while the originating replica kept it —
    /// permanent divergence with no error. Creating a task and patching it in
    /// the same millisecond is enough to trigger it, which is exactly what the
    /// blocker-convergence e2e was hitting about one run in six.
    ///
    /// The HLC makes the equal-stamp case unreachable while a device's clock
    /// state lives, but it is still reachable across a process restart (the
    /// logical counter resets to 0), so the guarantee is asserted directly.
    #[test]
    fn same_device_ops_at_one_hlc_do_not_lose_to_each_other() {
        let dev = [7u8; 16];
        assert!(
            lww_wins(&stamp(1000, 0, dev, 2), &row_of(&stamp(1000, 0, dev, 1))),
            "a device's later op must win over its own earlier op at an equal HLC"
        );
        // Cross-device ties are unaffected: still decided by memcmp.
        let lower = [1u8; 16];
        let higher = [9u8; 16];
        assert!(lww_wins(
            &stamp(1000, 0, higher, 1),
            &row_of(&stamp(1000, 0, lower, 1))
        ));
        assert!(!lww_wins(
            &stamp(1000, 0, lower, 1),
            &row_of(&stamp(1000, 0, higher, 1))
        ));
        // And a real HLC difference still dominates the device comparison.
        assert!(!lww_wins(
            &stamp(999, 0, higher, 9),
            &row_of(&stamp(1000, 0, lower, 1))
        ));
        assert!(lww_wins(
            &stamp(1001, 0, lower, 1),
            &row_of(&stamp(1000, 0, higher, 9))
        ));
        // The logical component orders inside one millisecond, which a bare
        // wall clock could not do at all.
        assert!(lww_wins(
            &stamp(1000, 1, lower, 1),
            &row_of(&stamp(1000, 0, higher, 1))
        ));
    }

    /// `seq` is the LAST term, not the first: a lower-seq op from a device
    /// whose HLC is ahead still wins.
    #[test]
    fn seq_never_overrides_the_hlc() {
        let a = [1u8; 16];
        let b = [2u8; 16];
        assert!(lww_wins(
            &stamp(2000, 0, a, 1),
            &row_of(&stamp(1000, 0, b, 9999))
        ));
    }

    /// A placeholder row that no op has stamped loses to everything.
    #[test]
    fn an_unstamped_row_loses_to_any_real_op() {
        let row = RowLww {
            hlc: Hlc::default(),
            device: None,
            seq: 0,
        };
        assert!(lww_wins(&stamp(0, 0, [0u8; 16], 0), &row));
    }

    /// `Stream.icon` was `Option<&'static str>` with
    /// `#[serde(skip_deserializing)]`, so it ALWAYS read back as `None`:
    /// setting an icon survived exactly as long as the process that set it,
    /// on every device including the one that set it. The projection had no
    /// column for it either, so this asserts both halves.
    #[test]
    fn a_streams_icon_survives_being_written() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let created = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    description: None,
                    color: None,
                    parent_id: None,
                    review_cadence: None,
                    reminder_lead_s: None,
                    icon: None,
                    default_context: None,
                }),
            )
            .unwrap();

        let mut stream = read_stream(db.conn(), created.entity.bytes())
            .unwrap()
            .expect("stream exists");
        stream.icon = Some("briefcase".into());
        let lww = e.lww_stamp(2);
        db.with_tx(|tx| update_stream_row(tx, &stream, &lww))
            .unwrap();

        let back = read_stream(db.conn(), created.entity.bytes())
            .unwrap()
            .expect("stream exists");
        assert_eq!(back.icon.as_deref(), Some("briefcase"));

        // ...and it survives the wire, which the borrowed type made impossible.
        let bytes = sunrise_cbor::encode_canonical(&back).unwrap();
        let decoded: sunrise_domain::Stream = sunrise_cbor::decode_canonical(&bytes).unwrap();
        assert_eq!(decoded.icon.as_deref(), Some("briefcase"));
    }

    /// Build the `unknown` map a newer schema would have written.
    fn future_fields() -> sunrise_domain::Unknowns {
        use ciborium::value::Value;
        let mut u = sunrise_domain::Unknowns::new();
        u.insert(
            "delegated_to".into(),
            sunrise_domain::CborValue(Value::Text("prs_future".into())),
        );
        u.insert(
            "zzz_sort_last".into(),
            sunrise_domain::CborValue(Value::Integer(7.into())),
        );
        u
    }

    /// protocol-versioning.md §7: "a v1 client receiving a v2 op preserves the
    /// unknown keys verbatim IN STORAGE and re-serializes them on outbound
    /// merges".
    ///
    /// `tasks.extra` was hardcoded to NULL by all three task writers, so the
    /// entity carried its unknown fields for exactly one transaction and the
    /// materialized row forgot them. The next outbound op then re-emitted the
    /// task WITHOUT them, and under entity-level LWW every replica converged on
    /// the truncated value — a silent, permanent data loss caused by the older
    /// device merely winning one conflict.
    #[test]
    fn unknown_entity_fields_survive_the_materialized_row() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let created = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "from a newer client".into(),
                    ..Default::default()
                }),
            )
            .unwrap();

        // Stand in for an op written by a newer schema: put the future fields
        // on the entity and write it back through the normal update path.
        let mut task = read_task_t(&e, &db, created.entity);
        task.unknown = future_fields();
        let lww = e.lww_stamp(2);
        db.with_tx(|tx| update_task_row(tx, &task, &lww)).unwrap();

        let back = read_task_t(&e, &db, created.entity);
        assert_eq!(
            back.unknown,
            future_fields(),
            "the projection must not forget what it could not interpret"
        );

        // ...and the fields ride back out in the entity's canonical CBOR, so
        // an outbound op carries them to every peer.
        let re = sunrise_cbor::encode_canonical(&back).unwrap();
        let decoded: sunrise_domain::Task = sunrise_cbor::decode_canonical(&re).unwrap();
        assert_eq!(decoded.unknown, future_fields());
    }

    /// `Stream.description` was accepted, carried in the op, and never stored.
    ///
    /// `create_stream` copied it onto the entity and `update_stream` applied
    /// the patch, so it reached every peer — and `read_stream` hardcoded
    /// `description: None`, so no replica could materialize it. The second
    /// half is what made it destructive rather than merely absent: the next
    /// `UpdateStream` read `None` and re-emitted `description: null`, and
    /// under entity-level LWW that erased the value on every replica that
    /// still had it. The seam exposed the field the whole time, so a client
    /// could set it, watch it vanish, and be right.
    ///
    /// `default_context` had no column, no patch field and no draft field.
    /// `icon` had a column that round-tripped and no way to reach it: the only
    /// writer in the tree was a test issuing direct SQL, which is why the gap
    /// outlived the test that was supposed to cover it. This one goes through
    /// the command surface for exactly that reason.
    #[test]
    fn stream_description_icon_and_default_context_survive_the_command_surface() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let ctx = e
            .apply(
                &mut db,
                Command::CreateContext(ContextDraft {
                    name: "deep-work".into(),
                    description: None,
                }),
            )
            .unwrap()
            .entity;

        let created = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    description: Some(sunrise_domain::NoteBody(b"the big rewrite".to_vec())),
                    color: None,
                    parent_id: None,
                    review_cadence: None,
                    reminder_lead_s: None,
                    icon: Some("briefcase".into()),
                    default_context: Some(ctx),
                }),
            )
            .unwrap()
            .entity;

        let got = read_stream(db.conn(), created.bytes()).unwrap().unwrap();
        assert_eq!(
            got.description.as_ref().map(|b| b.0.clone()),
            Some(b"the big rewrite".to_vec()),
            "a description set at create must reach the projection"
        );
        assert_eq!(got.icon.as_deref(), Some("briefcase"));
        assert_eq!(got.default_context, Some(ctx));

        // The erasure. An unrelated edit re-emits the whole entity, so if the
        // projection had forgotten the description this update would carry
        // `null` and win on every replica.
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: created,
                patch: StreamPatch {
                    name: Some("Work (2026)".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        let after = read_stream(db.conn(), created.bytes()).unwrap().unwrap();
        assert_eq!(after.name, "Work (2026)");
        assert_eq!(
            after.description.as_ref().map(|b| b.0.clone()),
            Some(b"the big rewrite".to_vec()),
            "an unrelated edit must not erase the description"
        );
        assert_eq!(after.icon.as_deref(), Some("briefcase"));
        assert_eq!(after.default_context, Some(ctx));

        // And each is settable and clearable through the patch.
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: created,
                patch: StreamPatch {
                    icon: Some(Some("rocket".into())),
                    default_context: Some(None),
                    description: Some(None),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        let cleared = read_stream(db.conn(), created.bytes()).unwrap().unwrap();
        assert_eq!(cleared.icon.as_deref(), Some("rocket"));
        assert_eq!(cleared.default_context, None);
        assert_eq!(cleared.description, None);
    }

    /// The same guarantee, for the five entities that did NOT have it.
    ///
    /// `tasks.extra`, `blocks.extra` and `attachments.extra` were the whole of
    /// the implementation. `streams`, `contexts`, `routines`, `focus_sessions`
    /// and `focus_session_ends` had no column at all, and their readers
    /// hardcoded `Unknowns::new()` — so an older build that merely read and
    /// re-saved one of them emitted a truncated full-state op, and entity-level
    /// LWW (ADR-0014) carried the truncation to every replica. The client
    /// advertises capability bit 34 `CLI_FORWARD_COMPAT` as a REQUIRED v1 bit
    /// while doing this, which is what made it a contract violation rather
    /// than a missing feature.
    ///
    /// Each case asserts both halves: the projection remembers, and the
    /// entity's canonical CBOR carries the fields back out to peers.
    #[test]
    fn unknown_stream_fields_survive_the_materialized_row() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let created = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    description: None,
                    color: None,
                    parent_id: None,
                    review_cadence: None,
                    reminder_lead_s: None,
                    icon: None,
                    default_context: None,
                }),
            )
            .unwrap();

        let mut stream = read_stream(db.conn(), created.entity.bytes())
            .unwrap()
            .expect("stream exists");
        stream.unknown = future_fields();
        let lww = e.lww_stamp(2);
        db.with_tx(|tx| update_stream_row(tx, &stream, &lww))
            .unwrap();

        let back = read_stream(db.conn(), created.entity.bytes())
            .unwrap()
            .expect("stream exists");
        assert_eq!(back.unknown, future_fields());

        let re = sunrise_cbor::encode_canonical(&back).unwrap();
        let decoded: sunrise_domain::Stream = sunrise_cbor::decode_canonical(&re).unwrap();
        assert_eq!(decoded.unknown, future_fields());
    }

    #[test]
    fn unknown_context_fields_survive_the_materialized_row() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let created = e
            .apply(
                &mut db,
                Command::CreateContext(ContextDraft {
                    name: "errands".into(),
                    description: None,
                }),
            )
            .unwrap();

        let mut ctx = read_context(db.conn(), created.entity.bytes())
            .unwrap()
            .expect("context exists");
        ctx.unknown = future_fields();
        let lww = e.lww_stamp(2);
        db.with_tx(|tx| update_context_row(tx, &ctx, &lww)).unwrap();

        let back = read_context(db.conn(), created.entity.bytes())
            .unwrap()
            .expect("context exists");
        assert_eq!(back.unknown, future_fields());

        let re = sunrise_cbor::encode_canonical(&back).unwrap();
        let decoded: sunrise_domain::Context = sunrise_cbor::decode_canonical(&re).unwrap();
        assert_eq!(decoded.unknown, future_fields());
    }

    #[test]
    fn unknown_routine_fields_survive_the_materialized_row() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let stream = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Home".into(),
                    description: None,
                    color: None,
                    parent_id: None,
                    review_cadence: None,
                    reminder_lead_s: None,
                    icon: None,
                    default_context: None,
                }),
            )
            .unwrap()
            .entity;
        let draft = routine_draft(
            stream,
            "FREQ=DAILY",
            T0 as i64,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e
            .apply(&mut db, Command::CreateRoutine(draft))
            .unwrap()
            .entity;

        let mut routine = read_routine(db.conn(), rid.bytes())
            .unwrap()
            .expect("routine exists");
        routine.unknown = future_fields();
        let lww = e.lww_stamp(2);
        db.with_tx(|tx| update_routine_row(tx, &routine, &lww))
            .unwrap();

        let back = read_routine(db.conn(), rid.bytes())
            .unwrap()
            .expect("routine exists");
        assert_eq!(back.unknown, future_fields());

        let re = sunrise_cbor::encode_canonical(&back).unwrap();
        let decoded: sunrise_domain::Routine = sunrise_cbor::decode_canonical(&re).unwrap();
        assert_eq!(decoded.unknown, future_fields());
    }

    /// The focus family is append-only (ADR-0013), so the unknown fields have
    /// to arrive with the insert rather than a later update — which is exactly
    /// how a newer peer's `focus.start` / `focus.end` op reaches this build.
    #[test]
    fn unknown_focus_fields_survive_the_materialized_row() {
        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);

        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "focus target".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let stream = read_task_t(&e, &db, task).stream_id;

        let session = EntityRef::new(EntityKind::FocusSession, [9u8; 16]);
        let start = FocusStart {
            id: session,
            task_id: task,
            stream_id: stream,
            started_at: sunrise_domain::epoch_ms::from_u64(T0),
            planned_ms: None,
            energy: None,
            kind: FocusKind::Work,
            chunk: None,
            unknown: future_fields(),
        };
        let end = FocusEnd {
            session_id: session,
            ended_at: sunrise_domain::epoch_ms::from_u64(T0 + 1_000),
            actual_focused_ms: 1_000,
            interruptions: Vec::new(),
            completed_task: false,
            unknown: future_fields(),
        };
        let lww = e.lww_stamp(2);
        db.with_tx(|tx| {
            insert_focus_start_row(tx, &start, &lww)?;
            insert_focus_end_row(tx, &end, &lww)
        })
        .unwrap();

        let sessions = read_focus_sessions(db.conn(), "", &[]).unwrap();
        let got = sessions
            .iter()
            .find(|s| s.start.id == session)
            .expect("session exists");
        assert_eq!(got.start.unknown, future_fields());
        assert_eq!(
            got.end.as_ref().expect("end exists").unknown,
            future_fields()
        );
    }

    /// An enum value from a newer schema must degrade, not reject: rejecting
    /// one field's value rejects the whole op, and two replicas then diverge
    /// permanently over one string.
    #[test]
    fn an_unknown_enum_variant_degrades_instead_of_failing_the_op() {
        use ciborium::value::Value;

        let clock = Arc::new(FakeClock(PLMutex::new(T0)));
        let e = engine_seeded(ROOT, [1u8; 32], clock);
        let mut db = db_root(ROOT);
        let created = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "t".into(),
                    ..Default::default()
                }),
            )
            .unwrap();

        // Rewrite the stored state string to something no build understands.
        db.conn()
            .execute(
                "UPDATE tasks SET state = 'delegated' WHERE id = ?",
                params![&created.entity.bytes()[..]],
            )
            .unwrap();

        let back = read_task_t(&e, &db, created.entity);
        assert_eq!(
            back.state,
            TaskState::Todo,
            "an unrecognised state must read as OPEN, never as done"
        );

        // The same rule on the wire: a Task encoded with an unknown state
        // decodes rather than failing.
        let mut v = ciborium::value::Value::serialized(&back).unwrap();
        if let Value::Map(entries) = &mut v {
            for (k, val) in entries.iter_mut() {
                if matches!(k, Value::Text(t) if t == "state") {
                    *val = Value::Text("delegated".into());
                }
            }
        }
        let bytes = sunrise_cbor::encode_canonical(&v).unwrap();
        let decoded: sunrise_domain::Task = sunrise_cbor::decode_lenient(&bytes)
            .expect("an unknown variant must not reject the op");
        assert_eq!(decoded.state, TaskState::Todo);
    }

    /// A clock pinned to a named zone, so a test can move a device between
    /// timezones the way a plane does.
    #[derive(Debug)]
    struct ZonedClock(u64, &'static str);
    impl Clock for ZonedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
        fn timezone(&self) -> String {
            self.1.to_string()
        }
    }

    /// #6: the three zone-less kinds must survive being written on one device
    /// and read on another in a different zone. A `Timestamp` could not
    /// express any of them, so all three used to collapse into "some UTC
    /// instant" and moved when the reader did.
    #[test]
    fn zone_less_times_survive_a_device_timezone_change() {
        use jiff::civil;

        let cases = [
            SunriseTime::floating(civil::date(2026, 3, 3).at(9, 0, 0, 0)),
            SunriseTime::all_day(civil::date(2026, 7, 4)),
            SunriseTime::zoned(civil::date(2026, 3, 3).at(9, 0, 0, 0), "America/New_York"),
        ];

        for want in cases {
            let kc = Arc::new(Keychain::for_test_seeded(
                VaultRootKey::from_bytes(ROOT),
                [1u8; 32],
            ));
            let writer = Engine::from_clock(
                Arc::new(ZonedClock(T0, "Europe/Berlin")),
                Arc::new(SystemRng),
                Arc::clone(&kc),
            );
            let mut db = db_root(ROOT);
            let res = writer
                .apply(
                    &mut db,
                    Command::CreateTask(TaskDraft {
                        title: "t".into(),
                        scheduled_at: Some(want.clone()),
                        ..Default::default()
                    }),
                )
                .unwrap();

            // The same vault, read by a device in a different zone.
            let reader = Engine::from_clock(
                Arc::new(ZonedClock(T0, "Pacific/Auckland")),
                Arc::new(SystemRng),
                kc,
            );
            let got = read_task_t(&reader, &db, res.entity).scheduled_at;
            assert_eq!(
                got,
                Some(want.clone()),
                "the stored value must not depend on who reads it"
            );

            // ...and the resolution follows the reader for the zone-less kinds
            // while staying put for the zoned one — which is the entire point
            // of keeping the kinds apart.
            let berlin = jiff::tz::TimeZone::get("Europe/Berlin").unwrap();
            let auckland = jiff::tz::TimeZone::get("Pacific/Auckland").unwrap();
            let moves = want.to_instant(&berlin) != want.to_instant(&auckland);
            assert_eq!(
                moves,
                !matches!(want, SunriseTime::Zoned { .. }),
                "only zone-less kinds follow the reader ({want:?})"
            );
        }
    }

    /// ...and the kind survives the op envelope, not just the local table:
    /// a replica that only ever saw the CBOR must reconstruct the same value.
    #[test]
    fn zone_less_times_round_trip_through_op_cbor() {
        use jiff::civil;

        for want in [
            SunriseTime::floating(civil::date(2026, 3, 3).at(9, 0, 0, 0)),
            SunriseTime::all_day(civil::date(2026, 7, 4)),
            SunriseTime::zoned(civil::date(2026, 3, 3).at(9, 0, 0, 0), "America/New_York"),
        ] {
            let ca = Arc::new(FakeClock(PLMutex::new(T0)));
            let cb = Arc::new(FakeClock(PLMutex::new(T0)));
            let ea = engine_seeded(ROOT, [1u8; 32], ca);
            let eb = engine_seeded(ROOT, [2u8; 32], cb);
            let mut dba = db_root(ROOT);
            let mut dbb = db_root(ROOT);
            trust(&eb, &mut dbb, &ea);

            let created = ea
                .apply(
                    &mut dba,
                    Command::CreateTask(TaskDraft {
                        title: "t".into(),
                        due_at: Some(want.clone()),
                        ..Default::default()
                    }),
                )
                .unwrap();
            eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
                .unwrap();

            assert_eq!(
                read_task_t(&eb, &dbb, created.entity).due_at,
                Some(want.clone()),
                "kind lost in transit for {want:?}"
            );
        }
    }

    /// #21: before HLCs, a device whose wall clock ran fast won every conflict
    /// it ever entered, permanently. Its peer could re-edit the value a hundred
    /// times and every one of those edits would lose to the same stale op,
    /// because the peer's honest `ts_ms` was still the smaller number. That is
    /// not a tie-break, it is a veto, and nothing in the system announced it.
    ///
    /// An HLC removes the veto because it is not a clock reading, it is a
    /// causal position. B cannot edit a Task it has never seen, so by the time
    /// B edits, B has ALREADY absorbed A's stamp and sits above it — whatever
    /// A's wall clock says. The fast device gets no advantage it did not earn
    /// by writing first.
    #[test]
    fn a_fast_clock_does_not_win_every_conflict() {
        // A is four minutes fast — inside the drift window, so its ops are
        // legitimate, just early.
        let skew = 4 * 60 * 1000;
        let ca = Arc::new(FakeClock(PLMutex::new(T0 + skew)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        let created = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "orig".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
            .unwrap();

        // Both edit without seeing the other's edit. A's wall clock reads four
        // minutes later than B's throughout.
        let a_up = ea
            .apply(
                &mut dba,
                Command::UpdateTask {
                    id: created.entity,
                    patch: TaskPatch {
                        title: Some("from-the-fast-device".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let b_up = eb
            .apply(
                &mut dbb,
                Command::UpdateTask {
                    id: created.entity,
                    patch: TaskPatch {
                        title: Some("from-the-slow-device".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
            .unwrap();
        ea.apply_remote(&mut dba, &env_bytes(&dbb, &b_up.op_id))
            .unwrap();

        // B's edit is causally later — it was made from a replica that had
        // seen A's create — so it wins, on BOTH replicas. Under the old
        // `(ts_ms, device_id)` rule A's four-minute lead would have won here
        // and gone on winning every subsequent round.
        assert_eq!(
            read_task_t(&eb, &dbb, created.entity).title,
            "from-the-slow-device"
        );
        assert_eq!(
            read_task_t(&ea, &dba, created.entity).title,
            "from-the-slow-device",
            "the fast clock must not hold a permanent veto over its peer"
        );

        // And the reverse still works: A edits again, having seen B's op, and
        // A's edit now wins. Neither device is stuck losing either.
        let a_again = ea
            .apply(
                &mut dba,
                Command::UpdateTask {
                    id: created.entity,
                    patch: TaskPatch {
                        title: Some("and-back-to-a".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_again.op_id))
            .unwrap();
        assert_eq!(
            read_task_t(&eb, &dbb, created.entity).title,
            "and-back-to-a"
        );
    }

    /// ...and a clock that is not merely fast but wrong is refused outright,
    /// so it cannot drag its peers' clocks along with it.
    #[test]
    fn an_op_beyond_the_drift_window_is_refused() {
        let ca = Arc::new(FakeClock(PLMutex::new(
            T0 + sunrise_cbor::hlc::MAX_DRIFT_MS + 1,
        )));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let created = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "from the future".into(),
                    ..Default::default()
                }),
            )
            .unwrap();

        let err = eb
            .apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
            .unwrap_err();
        assert!(
            matches!(&err, EngineError::RemoteOpInvalid(m) if m.contains("hlc")),
            "expected an HLC drift rejection, got {err:?}"
        );

        // Refused means refused: nothing was materialized and nothing was
        // recorded in the op log.
        let tasks: i64 = dbb
            .conn()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tasks, 0);
        let ops: i64 = dbb
            .conn()
            .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ops, 0);
    }

    /// A peer that has been offline for a week is not a clock problem: its ops
    /// are far in the PAST, which is legitimate and must be accepted.
    #[test]
    fn an_op_from_long_ago_is_accepted() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0 - 7 * 24 * 3_600_000)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let created = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "written a week ago".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
            .expect("an op from the past is not a drift violation");
        assert_eq!(
            read_task_t(&eb, &dbb, created.entity).title,
            "written a week ago"
        );
    }

    /// Every replica must record the SENDER's stamp, not its own post-merge
    /// reading — otherwise the LWW winner would depend on delivery order.
    #[test]
    fn the_receiver_stores_the_senders_stamp() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0 + 60_000)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let created = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "t".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
            .unwrap();

        let read = |db: &Db| -> (i64, i64, i64) {
            db.conn()
                .query_row(
                    "SELECT lww_hlc_ms, lww_hlc_logical, lww_seq FROM tasks WHERE id = ?",
                    params![&created.entity.bytes()[..]],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };
        assert_eq!(
            read(&dba),
            read(&dbb),
            "both replicas must stamp the row identically"
        );
        assert_eq!(
            read(&dbb).0,
            T0 as i64,
            "B stored A's physical clock, not its own"
        );
    }

    #[test]
    fn lww_tie_break_higher_device_wins_both_directions() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let da = ea.keychain.device_id();
        let db_id = eb.keychain.device_id();
        assert_ne!(da, db_id);
        // Winner's title: the op from the lexicographically-higher device id.
        let (winner_title, a_wins) = if da.as_slice() > db_id.as_slice() {
            ("A-tie", true)
        } else {
            ("B-tie", false)
        };

        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        // Mutual trust so each side can apply the other's op.
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        // A creates; both sides get the create.
        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "orig".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let create_env = env_bytes(&dba, &res.op_id);
        eb.apply_remote(&mut dbb, &create_env).unwrap();

        // Both update the SAME task at the SAME ts (a tie).
        set_clock(&ca, T0 + 1_000);
        set_clock(&cb, T0 + 1_000);
        let a_up = ea
            .apply(
                &mut dba,
                Command::UpdateTask {
                    id: res.entity,
                    patch: TaskPatch {
                        title: Some("A-tie".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let b_up = eb
            .apply(
                &mut dbb,
                Command::UpdateTask {
                    id: res.entity,
                    patch: TaskPatch {
                        title: Some("B-tie".into()),
                        ..Default::default()
                    },
                },
            )
            .unwrap();

        // Exchange the competing updates both ways.
        eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
            .unwrap();
        ea.apply_remote(&mut dba, &env_bytes(&dbb, &b_up.op_id))
            .unwrap();

        // Both replicas converge on the higher device's value, in both directions.
        assert_eq!(read_task_t(&ea, &dba, res.entity).title, winner_title);
        assert_eq!(read_task_t(&eb, &dbb, res.entity).title, winner_title);
        let _ = a_wins;
    }

    #[test]
    fn unknown_device_rejected() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        // NOTE: B does NOT trust A.

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "from stranger".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);

        assert!(matches!(
            eb.apply_remote(&mut dbb, &env),
            Err(EngineError::UnknownDevice)
        ));
        // No ops row, no materialization.
        assert_eq!(op_count(&dbb), 0);
        assert!(matches!(
            eb.query(&dbb, Query::EntityById(res.entity)),
            Err(EngineError::NotFound(_))
        ));
    }

    #[test]
    fn tampered_envelope_rejected() {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "authentic".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);

        // Flip a byte in the trailing signature region.
        let mut sig_tampered = env.clone();
        let last = sig_tampered.len() - 1;
        sig_tampered[last] ^= 0x01;
        assert!(matches!(
            eb.apply_remote(&mut dbb, &sig_tampered),
            Err(EngineError::RemoteOpInvalid(_))
        ));

        // Flip a byte in the ciphertext region (roughly the middle of the CBOR).
        let mut ct_tampered = env.clone();
        let mid = ct_tampered.len() / 2;
        ct_tampered[mid] ^= 0x01;
        assert!(matches!(
            eb.apply_remote(&mut dbb, &ct_tampered),
            Err(EngineError::RemoteOpInvalid(_))
        ));

        // Nothing was applied by either rejection.
        assert_eq!(op_count(&dbb), 0);
    }

    #[test]
    fn wrong_stream_key_rejected() {
        // A and B are on DIFFERENT vault roots, so B derives a different Stream
        // key and cannot decrypt A's payload — even though the cert (device-key
        // signed, root-independent) verifies and A is trusted.
        let root_a = [0xa1; 32];
        let root_b = [0xb2; 32];
        let ea = engine_seeded(root_a, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(root_b, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(root_a);
        let mut dbb = db_root(root_b);
        trust(&eb, &mut dbb, &ea);

        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: "secret".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let env = env_bytes(&dba, &res.op_id);

        assert!(matches!(
            eb.apply_remote(&mut dbb, &env),
            Err(EngineError::RemoteOpInvalid(_))
        ));
        assert_eq!(op_count(&dbb), 0);
    }

    #[test]
    fn deterministic_routine_task_converges() {
        // Both engines hold the SAME routine (same id) and each materializes the
        // same occurrence locally, producing a task with the SAME deterministic
        // id but different op-ids. Exchanging both create envelopes converges to
        // ONE identical task per DB.
        fn fixed_routine() -> Routine {
            Routine {
                id: EntityRef::new(EntityKind::Routine, [0x44; 16]),
                created_at: ms_to_ts(NOW),
                updated_at: ms_to_ts(NOW),
                template: TaskTemplate {
                    title: "Stretch".into(),
                    stream_id: stream_ref(7),
                    contexts: Vec::new(),
                    energy: None,
                    priority: None,
                    estimated_duration_s: None,
                    body: None,
                },
                rrule: RRule::parse("FREQ=DAILY").unwrap(),
                timezone: "UTC".into(),
                starts_at: ms_to_ts(NOW + 3_600_000),
                ends_at: None,
                scheduling_constraints: Vec::new(),
                skip_dates: Vec::new(),
                skipped_keys: Vec::new(),
                catchup_policy: RoutineCatchupPolicy::Skip,
                streak_counter: 0,
                last_completed_at: None,
                grace_window_s: None,
                forgiveness_enabled: true,
                streak_started_at: None,
                forgivenesses_in_window: 0,
                streak_keys: Vec::new(),
                paused: false,
                paused_until: None,
                archived: false,
                deleted: false,
                unknown: Unknowns::new(),
            }
        }

        let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);

        let r = fixed_routine();
        for (e, d) in [(&ea, &mut dba), (&eb, &mut dbb)] {
            d.with_tx(|tx| {
                ensure_stream_row(
                    tx,
                    &r.template.stream_id,
                    NOW as u64,
                    None,
                    false,
                    false,
                    false,
                )?;
                insert_routine_row(tx, &r, NOW as u64, &e.lww_stamp(1))
            })
            .unwrap();
            e.materialize_one_routine(d, &r, NOW as u64).unwrap();
        }

        // Pick the first materialized occurrence's deterministic task id.
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
        let occ = r.occurrences_in(window).unwrap();
        let tid = occurrence_task_id(&r.id, &occ[0].key);

        // Exchange the two create envelopes both ways.
        let env_a = create_env_for(&dba, tid.bytes());
        let env_b = create_env_for(&dbb, tid.bytes());
        eb.apply_remote(&mut dbb, &env_a).unwrap();
        ea.apply_remote(&mut dba, &env_b).unwrap();

        // Exactly one task per DB, and identical content.
        for d in [&dba, &dbb] {
            let n: i64 = d
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE id = ?",
                    params![tid.bytes().to_vec()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "one task row for the shared occurrence id");
        }
        assert_eq!(read_task_t(&ea, &dba, tid), read_task_t(&eb, &dbb, tid));
    }

    /// One canonical task row: (id, title, state, scheduled_ms, due_ms,
    /// stream_id, deleted, deferred_count, completed_ms).
    type TaskProjRow = (
        Vec<u8>,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Vec<u8>,
        i64,
        i64,
        Option<i64>,
    );

    /// Canonical projection of the `tasks` table for convergence assertions:
    /// every semantic field except op-ids and the LWW bookkeeping columns.
    fn tasks_projection(db: &Db) -> Vec<TaskProjRow> {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT id, title, state, scheduled_at_ms, due_at_ms, stream_id,
                        deleted, deferred_count, completed_at_ms
                 FROM tasks ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    }

    fn all_envelopes(db: &Db) -> Vec<Vec<u8>> {
        let mut stmt = db
            .conn()
            .prepare("SELECT envelope FROM ops ORDER BY rowid ASC")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        /// Two mutually-trusting engines run random interleaved command streams
        /// on their own DBs; exchanging ALL envelopes both ways (with reordering
        /// and duplication) converges both DBs to an identical task projection.
        #[test]
        fn convergence_under_random_interleaving(
            steps in proptest::collection::vec(
                (proptest::bool::ANY, 0u8..4u8, 0u8..8u8),
                1usize..14usize,
            ),
        ) {
            // Shared timeline so ops carry a common ts axis (ties possible).
            let clock = Arc::new(FakeClock(PLMutex::new(T0)));
            let ea = engine_seeded(ROOT, [1u8; 32], clock.clone());
            let eb = engine_seeded(ROOT, [2u8; 32], clock.clone());
            let mut dba = db_root(ROOT);
            let mut dbb = db_root(ROOT);
            trust(&eb, &mut dbb, &ea);
            trust(&ea, &mut dba, &eb);

            let mut ids_a: Vec<EntityRef> = Vec::new();
            let mut ids_b: Vec<EntityRef> = Vec::new();
            let mut tick = T0;

            for (i, (actor_a, kind, arg)) in steps.into_iter().enumerate() {
                tick += 1;
                set_clock(&clock, tick);
                let (e, d, ids) = if actor_a {
                    (&ea, &mut dba, &mut ids_a)
                } else {
                    (&eb, &mut dbb, &mut ids_b)
                };
                // With no existing task, any non-create degrades to a create.
                let effective = if ids.is_empty() { 0 } else { kind };
                match effective {
                    0 => {
                        let r = e
                            .apply(
                                d,
                                Command::CreateTask(TaskDraft {
                                    title: format!("t{}-{}", usize::from(actor_a), i),
                                    ..Default::default()
                                }),
                            )
                            .unwrap();
                        ids.push(r.entity);
                    }
                    1 => {
                        let target = ids[usize::from(arg) % ids.len()];
                        let _ = e.apply(
                            d,
                            Command::UpdateTask {
                                id: target,
                                patch: TaskPatch {
                                    title: Some(format!("u{}-{}", usize::from(actor_a), i)),
                                    ..Default::default()
                                },
                            },
                        );
                    }
                    2 => {
                        let target = ids[usize::from(arg) % ids.len()];
                        let _ = e.apply(d, Command::CompleteTask(target));
                    }
                    _ => {
                        let target = ids[usize::from(arg) % ids.len()];
                        let _ = e.apply(
                            d,
                            Command::DeferTask {
                                id: target,
                                to_ms: tick + 86_400_000,
                            },
                        );
                    }
                }
            }

            // Exchange ALL envelopes both ways, forward then reversed (the
            // reversed pass re-delivers every op as a duplicate and in a
            // different order).
            let envs_a = all_envelopes(&dba);
            let envs_b = all_envelopes(&dbb);
            for pass in 0..2 {
                let a_iter: Vec<&Vec<u8>> = if pass == 0 {
                    envs_a.iter().collect()
                } else {
                    envs_a.iter().rev().collect()
                };
                let b_iter: Vec<&Vec<u8>> = if pass == 0 {
                    envs_b.iter().collect()
                } else {
                    envs_b.iter().rev().collect()
                };
                for env in a_iter {
                    eb.apply_remote(&mut dbb, env).unwrap();
                }
                for env in b_iter {
                    ea.apply_remote(&mut dba, env).unwrap();
                }
            }

            proptest::prop_assert_eq!(tasks_projection(&dba), tasks_projection(&dbb));
        }
    }

    // ---- task dependencies: derived `blocked` + the reverse index ----

    use sunrise_domain::EffectiveTaskState;

    /// Every open task with its derived dependency counts, ranked.
    fn actionable_rows(e: &Engine, db: &Db) -> Vec<ActionableTask> {
        match e
            .query(
                db,
                Query::Actionable {
                    stream: None,
                    limit: 100,
                },
            )
            .unwrap()
        {
            QueryResult::Actionable(v) => v,
            _ => panic!("expected Actionable"),
        }
    }

    fn row_for(rows: &[ActionableTask], id: EntityRef) -> ActionableTask {
        rows.iter()
            .find(|r| r.task.id == id)
            .unwrap_or_else(|| panic!("no actionable row for {id}"))
            .clone()
    }

    fn new_task(e: &Engine, db: &mut Db, title: &str) -> EntityRef {
        e.apply(
            db,
            Command::CreateTask(TaskDraft {
                title: title.into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity
    }

    fn set_blockers(
        e: &Engine,
        db: &mut Db,
        task: EntityRef,
        blockers: Vec<EntityRef>,
    ) -> Result<CommandResult, EngineError> {
        e.apply(
            db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    blocked_by: Some(blockers),
                    ..Default::default()
                },
            },
        )
    }

    #[test]
    fn blocked_by_survives_the_materialized_projection() {
        // Before migration 0008 the `tasks` table had no blocker columns at
        // all: `blocked_by` lived only inside the op-log inner op, so every
        // read came back empty and nothing could derive `blocked` from it.
        let mut db = db();
        let e = engine();
        let blocker = new_task(&e, &mut db, "buy paint");
        let dependent = new_task(&e, &mut db, "paint the fence");
        set_blockers(&e, &mut db, dependent, vec![blocker]).unwrap();

        let t = read_task_t(&e, &db, dependent);
        assert_eq!(
            t.blocked_by,
            BTreeSet::from([blocker]),
            "blocked_by round-trips through the projection"
        );

        // Replacing the set replaces the index rows, it does not accumulate.
        set_blockers(&e, &mut db, dependent, vec![]).unwrap();
        assert!(read_task_t(&e, &db, dependent).blocked_by.is_empty());
    }

    #[test]
    fn completing_a_blocker_flips_its_dependents_to_actionable() {
        let mut db = db();
        let e = engine();
        let blocker = new_task(&e, &mut db, "buy paint");
        let dependent = new_task(&e, &mut db, "paint the fence");
        set_blockers(&e, &mut db, dependent, vec![blocker]).unwrap();

        let rows = actionable_rows(&e, &db);
        let dep = row_for(&rows, dependent);
        assert_eq!(dep.effective_state, EffectiveTaskState::Blocked);
        assert_eq!(dep.open_blockers, 1);
        let blk = row_for(&rows, blocker);
        assert_eq!(blk.effective_state, EffectiveTaskState::Todo);
        assert_eq!(blk.unblocks, 1, "reverse index: it holds one task back");

        // The whole point: no unblock op, no repair pass — completing the
        // blocker is enough.
        e.apply(&mut db, Command::CompleteTask(blocker)).unwrap();

        let rows = actionable_rows(&e, &db);
        let dep = row_for(&rows, dependent);
        assert_eq!(dep.effective_state, EffectiveTaskState::Todo);
        assert_eq!(dep.open_blockers, 0);
        assert!(
            !rows.iter().any(|r| r.task.id == blocker),
            "a done blocker drops out of the open list"
        );
        assert_eq!(
            read_task_t(&e, &db, dependent).blocked_by,
            BTreeSet::from([blocker]),
            "the edge itself is kept; only its effect went away"
        );
    }

    #[test]
    fn a_cancelled_or_deleted_blocker_also_releases_its_dependents() {
        let mut db = db();
        let e = engine();
        let cancelled = new_task(&e, &mut db, "wait for legal");
        let deleted = new_task(&e, &mut db, "chase invoice");
        let dependent = new_task(&e, &mut db, "ship it");
        set_blockers(&e, &mut db, dependent, vec![cancelled, deleted]).unwrap();
        assert_eq!(
            row_for(&actionable_rows(&e, &db), dependent).open_blockers,
            2
        );

        e.apply(
            &mut db,
            Command::UpdateTask {
                id: cancelled,
                patch: TaskPatch {
                    state: Some(TaskState::Cancelled),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(
            row_for(&actionable_rows(&e, &db), dependent).open_blockers,
            1
        );

        // A tombstoned blocker will never reach `done`; it must not block
        // forever either.
        e.apply(&mut db, Command::DeleteTask(deleted)).unwrap();
        let dep = row_for(&actionable_rows(&e, &db), dependent);
        assert_eq!(dep.open_blockers, 0);
        assert_eq!(dep.effective_state, EffectiveTaskState::Todo);
    }

    #[test]
    fn actionable_ranks_by_how_much_each_task_unblocks() {
        // The read path a Focus Planner wants: actionable first, then by the
        // size of the reverse edge set.
        let mut db = db();
        let e = engine();
        let hub = new_task(&e, &mut db, "unblock everything");
        let minor = new_task(&e, &mut db, "unblock one thing");
        let leaf = new_task(&e, &mut db, "unblock nothing");
        let d1 = new_task(&e, &mut db, "d1");
        let d2 = new_task(&e, &mut db, "d2");
        let d3 = new_task(&e, &mut db, "d3");
        for d in [d1, d2] {
            set_blockers(&e, &mut db, d, vec![hub]).unwrap();
        }
        set_blockers(&e, &mut db, d3, vec![hub, minor]).unwrap();

        let rows = actionable_rows(&e, &db);
        let order: Vec<EntityRef> = rows.iter().map(|r| r.task.id).collect();
        assert_eq!(
            &order[..3],
            &[hub, minor, leaf][..],
            "actionable first, most-unblocking first: {order:?}"
        );
        assert_eq!(row_for(&rows, hub).unblocks, 3);
        assert_eq!(row_for(&rows, minor).unblocks, 1);
        assert_eq!(row_for(&rows, leaf).unblocks, 0);
        // The three blocked ones sort after every actionable one.
        for id in [d1, d2, d3] {
            assert_eq!(
                row_for(&rows, id).effective_state,
                EffectiveTaskState::Blocked
            );
        }
        assert!(
            order.iter().position(|x| *x == d1).unwrap() > 2,
            "blocked tasks sink below actionable ones"
        );
    }

    #[test]
    fn self_blocking_and_cycles_are_rejected() {
        let mut db = db();
        let e = engine();
        let a = new_task(&e, &mut db, "a");
        let b = new_task(&e, &mut db, "b");
        let c = new_task(&e, &mut db, "c");

        assert!(matches!(
            set_blockers(&e, &mut db, a, vec![a]),
            Err(EngineError::Validation(
                sunrise_domain::ValidationError::BlockedByCycle
            ))
        ));

        // a <- b <- c, then closing the loop c <- a.
        set_blockers(&e, &mut db, b, vec![a]).unwrap();
        set_blockers(&e, &mut db, c, vec![b]).unwrap();
        assert!(matches!(
            set_blockers(&e, &mut db, a, vec![c]),
            Err(EngineError::Validation(
                sunrise_domain::ValidationError::BlockedByCycle
            ))
        ));
        // The rejected edge left nothing behind.
        assert!(read_task_t(&e, &db, a).blocked_by.is_empty());
    }

    // ---- scheduling-constraint enforcement ----

    /// NOW is 2023-11-14T22:13:20Z — a Tuesday evening, outside a 09:00–17:00
    /// window and inside it ten hours earlier (12:13Z).
    const OUTSIDE_WINDOW_MS: i64 = NOW;
    const INSIDE_WINDOW_MS: i64 = NOW - 10 * 3_600_000;

    fn soft_9_to_5() -> ScheduleConstraint {
        ScheduleConstraint {
            severity: sunrise_domain::ConstraintSeverity::Soft,
            ..sample_constraint()
        }
    }

    #[test]
    fn scheduling_against_a_hard_constraint_is_rejected_on_create() {
        let mut db = db();
        let e = engine();
        let err = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "deploy at 10pm".into(),
                    scheduled_at: Some(ms_to_ts(OUTSIDE_WINDOW_MS).into()),
                    scheduling_constraints: vec![sample_constraint()],
                    ..Default::default()
                }),
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                EngineError::Validation(sunrise_domain::ValidationError::HardScheduleConstraint)
            ),
            "got {err:?}"
        );
        // Nothing was written.
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);

        // The same constraint at an instant inside the window passes.
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "deploy at noon".into(),
                scheduled_at: Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
                scheduling_constraints: vec![sample_constraint()],
                ..Default::default()
            }),
        )
        .unwrap();
    }

    #[test]
    fn a_soft_violation_is_surfaced_rather_than_blocking() {
        let mut db = db();
        let e = engine();
        let soft = soft_9_to_5();
        let res = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "deploy at 10pm".into(),
                    scheduled_at: Some(ms_to_ts(OUTSIDE_WINDOW_MS).into()),
                    scheduling_constraints: vec![soft],
                    ..Default::default()
                }),
            )
            .unwrap();
        assert_eq!(
            res.soft_violations,
            vec![soft],
            "a soft violation never blocks, but it must not vanish either"
        );
        // Inside the window there is nothing to report.
        let res = e
            .apply(
                &mut db,
                Command::UpdateTask {
                    id: res.entity,
                    patch: TaskPatch {
                        scheduled_at: Some(Some(ms_to_ts(INSIDE_WINDOW_MS).into())),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        assert!(res.soft_violations.is_empty());
    }

    #[test]
    fn rescheduling_and_deferring_clear_the_same_hard_gate() {
        let mut db = db();
        let e = engine();
        let id = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "deploy".into(),
                    scheduled_at: Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
                    scheduling_constraints: vec![sample_constraint()],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;

        // Moving it out of the window is rejected...
        assert!(matches!(
            e.apply(
                &mut db,
                Command::UpdateTask {
                    id,
                    patch: TaskPatch {
                        scheduled_at: Some(Some(ms_to_ts(OUTSIDE_WINDOW_MS).into())),
                        ..Default::default()
                    },
                },
            ),
            Err(EngineError::Validation(
                sunrise_domain::ValidationError::HardScheduleConstraint
            ))
        ));
        // ... and so is deferring into it, since deferring *is* scheduling.
        assert!(matches!(
            e.apply(
                &mut db,
                Command::DeferTask {
                    id,
                    to_ms: OUTSIDE_WINDOW_MS as u64,
                },
            ),
            Err(EngineError::Validation(
                sunrise_domain::ValidationError::HardScheduleConstraint
            ))
        ));
        assert_eq!(
            read_task_t(&e, &db, id).scheduled_at,
            Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
            "both rejections left the schedule untouched"
        );
    }

    #[test]
    fn an_unrelated_edit_is_not_re_gated_by_constraints() {
        // The rule is "fails validation when the user *schedules* against it",
        // not "every write to a task that already sits outside its window is
        // rejected" — otherwise a task could become uneditable.
        let mut db = db();
        let e = engine();
        let id = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "deploy".into(),
                    scheduled_at: Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        // Attach a hard constraint the current schedule already violates by
        // moving the schedule and the list in one go is rejected; here we
        // instead rename a task whose stored list is violated.
        db.conn()
            .execute(
                "UPDATE tasks SET scheduled_at_ms = ?, scheduling_constraints = ? WHERE id = ?",
                params![
                    OUTSIDE_WINDOW_MS,
                    encode_constraints(&[sample_constraint()]).unwrap(),
                    id.bytes().to_vec()
                ],
            )
            .unwrap();
        e.apply(
            &mut db,
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    title: Some("deploy (renamed)".into()),
                    ..Default::default()
                },
            },
        )
        .expect("a rename must not be blocked by an already-violated window");
        assert!(e.apply(&mut db, Command::CompleteTask(id)).is_ok());
    }

    // ---- routine streak counter ----

    fn engine_clocked(clock: Arc<FakeClock>) -> Engine {
        let keychain = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
        Engine::from_clock(clock, Arc::new(SystemRng), keychain)
    }

    fn routine_of(e: &Engine, db: &Db, rid: EntityRef) -> Routine {
        match e.query(db, Query::EntityById(rid)).unwrap() {
            QueryResult::Routine(r) => *r,
            _ => panic!("expected Routine"),
        }
    }

    /// The first `n` occurrence task ids of `rid`, with their instants.
    fn occurrence_tasks(routine: &Routine, n: usize) -> Vec<(EntityRef, i64)> {
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 14 * DAY_MS));
        routine
            .occurrences_in(window)
            .unwrap()
            .into_iter()
            .take(n)
            .map(|o| {
                (
                    occurrence_task_id(&routine.id, &o.key),
                    o.at.as_millisecond(),
                )
            })
            .collect()
    }

    fn seed_routine(e: &Engine, db: &mut Db) -> (EntityRef, Vec<(EntityRef, i64)>) {
        let draft = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e.apply(db, Command::CreateRoutine(draft)).unwrap().entity;
        let routine = routine_of(e, db, rid);
        let occ = occurrence_tasks(&routine, 4);
        assert_eq!(occ.len(), 4);
        (rid, occ)
    }

    #[test]
    fn streak_survives_a_gap_inside_grace_and_resets_outside_it() {
        let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let e = engine_clocked(clock.clone());
        let mut db = db();
        let (rid, occ) = seed_routine(&e, &mut db);
        assert_eq!(routine_of(&e, &db, rid).streak_counter, 0);

        // 1. On time.
        set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
        e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();
        assert_eq!(routine_of(&e, &db, rid).streak_counter, 1);

        // 2. 23h late — inside the 24h default grace window, streak survives.
        set_clock(&clock, (occ[1].1 + 23 * 3_600_000) as u64);
        e.apply(&mut db, Command::CompleteTask(occ[1].0)).unwrap();
        let r = routine_of(&e, &db, rid);
        assert_eq!(r.streak_counter, 2, "a gap inside grace keeps the streak");
        assert_eq!(r.forgivenesses_in_window, 0);

        // 3. 25h late — outside grace. Forgiveness absorbs the first miss.
        set_clock(&clock, (occ[2].1 + 25 * 3_600_000) as u64);
        e.apply(&mut db, Command::CompleteTask(occ[2].0)).unwrap();
        let r = routine_of(&e, &db, rid);
        assert_eq!(r.streak_counter, 2, "forgiven, not advanced, not reset");
        assert_eq!(r.forgivenesses_in_window, 1);

        // 4. A second miss in the same 30-day window resets.
        set_clock(&clock, (occ[3].1 + 25 * 3_600_000) as u64);
        e.apply(&mut db, Command::CompleteTask(occ[3].0)).unwrap();
        let r = routine_of(&e, &db, rid);
        assert_eq!(r.streak_counter, 0, "a gap outside grace resets the streak");
        assert_eq!(r.streak_started_at, None);
    }

    #[test]
    fn re_completing_an_occurrence_never_moves_the_streak() {
        let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let e = engine_clocked(clock.clone());
        let mut db = db();
        let (rid, occ) = seed_routine(&e, &mut db);

        set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
        e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();
        let after_first = routine_of(&e, &db, rid);
        assert_eq!(after_first.streak_counter, 1);
        assert_eq!(after_first.streak_keys.len(), 1);
        let routine_ops_before = routine_op_count(&db);

        // Resurrect and re-complete, well outside grace. The idempotency key
        // makes it a no-op — and emits no routine op at all.
        set_clock(&clock, (occ[0].1 + 10 * DAY_MS) as u64);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: occ[0].0,
                patch: TaskPatch {
                    state: Some(TaskState::Todo),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();

        let after = routine_of(&e, &db, rid);
        assert_eq!(after.streak_counter, 1);
        assert_eq!(after.streak_keys, after_first.streak_keys);
        assert_eq!(
            routine_op_count(&db),
            routine_ops_before,
            "a duplicate completion emits no routine op"
        );
    }

    #[test]
    fn a_non_routine_task_completing_touches_no_routine() {
        let mut db = db();
        let e = engine();
        let id = new_task(&e, &mut db, "one-off");
        let before = routine_op_count(&db);
        e.apply(&mut db, Command::CompleteTask(id)).unwrap();
        assert_eq!(routine_op_count(&db), before);
    }

    fn routine_op_count(db: &Db) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM ops WHERE inner_kind = 'routine.update'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn the_streak_advance_converges_to_the_other_replica() {
        // The completion emits BOTH a task.update and a routine.update in one
        // transaction, so a replica that never saw the completion still lands
        // on the same counter.
        let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let (rid, occ) = seed_routine(&ea, &mut dba);
        // B learns the routine and the occurrence task.
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
            .unwrap();
        eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
            .unwrap();

        // Both replicas move together. An hour of real skew between two peers
        // is outside the HLC drift window on purpose; this test is about
        // convergence, not about clock abuse.
        set_clock(&ca, (occ[0].1 + 3_600_000) as u64);
        set_clock(&cb, (occ[0].1 + 3_600_000) as u64);
        ea.apply(&mut dba, Command::CompleteTask(occ[0].0)).unwrap();
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, occ[0].0.bytes(), "task.update"),
        )
        .unwrap();
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
            .unwrap();

        let ra = routine_of(&ea, &dba, rid);
        let rb = routine_of(&eb, &dbb, rid);
        assert_eq!(ra.streak_counter, 1);
        assert_eq!(rb.streak_counter, 1, "streak converged");
        assert_eq!(ra.streak_keys, rb.streak_keys);
        assert_eq!(ra.streak_started_at, rb.streak_started_at);
    }

    /// The stored LWW stamp on a row, as the four columns hold it.
    fn row_stamp(db: &Db, table: &str, id_col: &str, id: &EntityRef) -> (i64, i64, i64, Vec<u8>) {
        db.conn()
            .query_row(
                &format!(
                    "SELECT lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device
                     FROM {table} WHERE {id_col} = ?"
                ),
                params![&id.bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    /// A command that emits TWO ops must stamp each row with the stamp of the
    /// op that carries it. Stamping the routine row with the *task* op's stamp
    /// left the origin holding different `lww_*` columns from every replica —
    /// which merges identically today and resolves the NEXT concurrent edit
    /// differently on each side.
    #[test]
    fn a_two_op_command_stamps_each_row_with_its_own_ops_stamp() {
        let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let (rid, occ) = seed_routine(&ea, &mut dba);
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
            .unwrap();
        eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
            .unwrap();

        set_clock(&ca, (occ[0].1 + 3_600_000) as u64);
        set_clock(&cb, (occ[0].1 + 3_600_000) as u64);
        // One command, two ops: a task.update and the routine.update carrying
        // the streak advance.
        ea.apply(&mut dba, Command::CompleteTask(occ[0].0)).unwrap();
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, occ[0].0.bytes(), "task.update"),
        )
        .unwrap();
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
            .unwrap();

        assert_eq!(
            row_stamp(&dba, "routines", "id", &rid),
            row_stamp(&dbb, "routines", "id", &rid),
            "the routine row's stamp must match on both replicas"
        );
        assert_eq!(
            row_stamp(&dba, "tasks", "id", &occ[0].0),
            row_stamp(&dbb, "tasks", "id", &occ[0].0),
            "the task row's stamp must match on both replicas"
        );
    }

    /// The same rule on the other two-op command: `SkipRoutineOccurrence`
    /// emits a routine.update and a task.delete.
    #[test]
    fn skipping_an_occurrence_stamps_the_task_row_with_the_task_ops_stamp() {
        let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca);
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let (rid, occ) = seed_routine(&ea, &mut dba);
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
            .unwrap();
        eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
            .unwrap();

        let key = occurrence_key_at("UTC", ms_to_ts(occ[0].1)).unwrap();
        ea.apply(
            &mut dba,
            Command::SkipRoutineOccurrence {
                id: rid,
                occurrence_key: key,
            },
        )
        .unwrap();
        eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
            .unwrap();
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, occ[0].0.bytes(), "task.delete"),
        )
        .unwrap();

        assert_eq!(
            row_stamp(&dba, "tasks", "id", &occ[0].0),
            row_stamp(&dbb, "tasks", "id", &occ[0].0),
            "the tombstoned task's stamp must match on both replicas"
        );
        assert_eq!(
            row_stamp(&dba, "routines", "id", &rid),
            row_stamp(&dbb, "routines", "id", &rid),
        );
    }

    // ---- out-of-order dependency ops still converge ----

    fn blocker_edges(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut stmt = db
            .conn()
            .prepare("SELECT task_id, blocker_id FROM task_blockers ORDER BY task_id, blocker_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn a_dependency_op_that_overtakes_its_blocker_still_converges() {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb);
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let blocker = new_task(&ea, &mut dba, "buy paint");
        let dependent = new_task(&ea, &mut dba, "paint the fence");
        // Each op needs a strictly later stamp than the row it supersedes, or
        // entity-level LWW discards it as a tie.
        set_clock(&ca, T0 + 1_000);
        set_blockers(&ea, &mut dba, dependent, vec![blocker]).unwrap();

        // Deliver the dependent's ops FIRST — B has never heard of the blocker.
        eb.apply_remote(&mut dbb, &create_env_for(&dba, dependent.bytes()))
            .unwrap();
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, dependent.bytes(), "task.update"),
        )
        .unwrap();

        let rows = actionable_rows(&eb, &dbb);
        let dep = row_for(&rows, dependent);
        assert_eq!(
            dep.effective_state,
            EffectiveTaskState::Blocked,
            "an unknown blocker counts as open, so the task is not flashed as actionable"
        );
        assert_eq!(dep.open_blockers, 1);
        assert!(
            !rows.iter().any(|r| r.task.id == blocker),
            "the blocker itself does not exist on B yet"
        );

        // The blocker's create finally lands, still open.
        eb.apply_remote(&mut dbb, &create_env_for(&dba, blocker.bytes()))
            .unwrap();
        assert_eq!(
            row_for(&actionable_rows(&eb, &dbb), dependent).open_blockers,
            1
        );
        assert_eq!(row_for(&actionable_rows(&eb, &dbb), blocker).unblocks, 1);

        // A completes it; B applies that op and the dependent frees itself.
        set_clock(&ca, T0 + 2_000);
        ea.apply(&mut dba, Command::CompleteTask(blocker)).unwrap();
        eb.apply_remote(
            &mut dbb,
            &env_for_kind(&dba, blocker.bytes(), "task.update"),
        )
        .unwrap();
        let dep = row_for(&actionable_rows(&eb, &dbb), dependent);
        assert_eq!(dep.effective_state, EffectiveTaskState::Todo);
        assert_eq!(dep.open_blockers, 0);

        // Both replicas hold the identical dependency index, and re-delivering
        // every op in reverse changes nothing.
        assert_eq!(blocker_edges(&dba), blocker_edges(&dbb));
        for env in all_envelopes(&dba).iter().rev() {
            eb.apply_remote(&mut dbb, env).unwrap();
        }
        assert_eq!(blocker_edges(&dba), blocker_edges(&dbb));
        assert_eq!(tasks_projection(&dba), tasks_projection(&dbb));
    }

    // ---- focus sessions (ADR-0013) ----

    fn focus_engine(now_ms: u64) -> (Engine, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock(PLMutex::new(now_ms)));
        let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
        (
            Engine::from_clock(clock.clone(), Arc::new(SystemRng), kc),
            clock,
        )
    }

    fn task_with(e: &Engine, db: &mut Db, title: &str, d: TaskDraft) -> EntityRef {
        e.apply(
            db,
            Command::CreateTask(TaskDraft {
                title: title.into(),
                ..d
            }),
        )
        .unwrap()
        .entity
    }

    fn start_work(e: &Engine, db: &mut Db, task: EntityRef, len: SessionLength) -> EntityRef {
        e.apply(
            db,
            Command::StartFocus(FocusStartDraft {
                task_id: task,
                kind: FocusKind::Work,
                length: len,
                energy: None,
            }),
        )
        .unwrap()
        .entity
    }

    fn running(e: &Engine, db: &Db) -> Vec<FocusSessionRow> {
        match e.query(db, Query::RunningFocusSessions).unwrap() {
            QueryResult::FocusSessions(v) => v,
            other => panic!("expected FocusSessions, got {other:?}"),
        }
    }

    fn sessions_for(e: &Engine, db: &Db, task: EntityRef) -> Vec<FocusSessionRow> {
        match e
            .query(db, Query::TaskFocusSessions { task, limit: 50 })
            .unwrap()
        {
            QueryResult::FocusSessions(v) => v,
            other => panic!("expected FocusSessions, got {other:?}"),
        }
    }

    fn stats(e: &Engine, db: &Db, now_ms: u64) -> sunrise_domain::FocusStats {
        match e
            .query(
                db,
                Query::FocusStats {
                    stream: None,
                    since_ms: None,
                    now_ms,
                },
            )
            .unwrap()
        {
            QueryResult::FocusStats(s) => *s,
            other => panic!("expected FocusStats, got {other:?}"),
        }
    }

    #[test]
    fn start_focus_mints_an_fcs_entity_and_never_touches_the_task() {
        let mut db = db();
        let (e, _clock) = focus_engine(1_000);
        let task = task_with(&e, &mut db, "write the ADR", TaskDraft::default());
        let before = read_task(db.conn(), task.bytes()).unwrap().unwrap();

        let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        assert_eq!(
            session.kind(),
            EntityKind::FocusSession,
            "a session is its own entity, not a field on the Task"
        );
        assert!(session.to_str().starts_with("fcs_"));

        // The Task is byte-identical: focusing is not an edit.
        let after = read_task(db.conn(), task.bytes()).unwrap().unwrap();
        assert_eq!(before, after);

        // The op is a `focus.start` routed under the task's stream.
        let kind: String = db
            .conn()
            .query_row(
                "SELECT inner_kind FROM ops WHERE target_id = ?",
                params![&session.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "focus.start");
    }

    #[test]
    fn a_dangling_start_reads_as_running_and_elapsed_comes_from_the_clock() {
        let mut db = db();
        let (e, clock) = focus_engine(10_000);
        let task = task_with(&e, &mut db, "long haul", TaskDraft::default());
        let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);

        // No `end` op has been written — and that is a *valid* state, the one
        // the app dying mid-session leaves behind.
        let rows = running(&e, &db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session.start.id, session);
        assert!(rows[0].running);
        assert_eq!(rows[0].focused_ms, 0);

        // Elapsed is derived: nothing was written, only the clock moved.
        set_clock(&clock, 10_000 + 90_000);
        let rows = running(&e, &db);
        assert_eq!(rows[0].focused_ms, 90_000);
        set_clock(&clock, 10_000 + 300_000);
        assert_eq!(running(&e, &db)[0].focused_ms, 300_000);

        // And the row itself stores no ticking value to disagree with.
        let stored: Option<i64> = db
            .conn()
            .query_row(
                "SELECT actual_focused_ms FROM focus_session_ends WHERE session_id = ?",
                params![&session.bytes()[..]],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(stored, None, "nothing is frozen until the end op");
    }

    #[test]
    fn end_focus_freezes_the_measurement_and_the_session_is_then_immutable() {
        let mut db = db();
        let (e, clock) = focus_engine(1_000);
        let task = task_with(&e, &mut db, "ship it", TaskDraft::default());
        let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);

        set_clock(&clock, 1_000 + POMODORO_MS);
        e.apply(
            &mut db,
            Command::EndFocus {
                session,
                actual_focused_ms: None,
                completed_task: true,
            },
        )
        .unwrap();

        assert!(running(&e, &db).is_empty(), "the session is closed");
        let rows = sessions_for(&e, &db, task);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].running);
        assert_eq!(rows[0].focused_ms, POMODORO_MS);

        // The clock moving on no longer changes the answer.
        set_clock(&clock, u64::MAX);
        assert_eq!(sessions_for(&e, &db, task)[0].focused_ms, POMODORO_MS);

        // Closing twice is rejected: an ended session is immutable, which is
        // the whole reason start and end are separate append-only records.
        let err = e
            .apply(
                &mut db,
                Command::EndFocus {
                    session,
                    actual_focused_ms: None,
                    completed_task: false,
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn end_focus_accepts_a_caller_supplied_actual_smaller_than_elapsed() {
        let mut db = db();
        let (e, clock) = focus_engine(0);
        let task = task_with(&e, &mut db, "paused a lot", TaskDraft::default());
        let session = start_work(&e, &mut db, task, SessionLength::UntilDone);
        set_clock(&clock, 60 * 60 * 1000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session,
                actual_focused_ms: Some(20 * 60 * 1000),
                completed_task: false,
            },
        )
        .unwrap();
        let rows = sessions_for(&e, &db, task);
        // Elapsed wall time is an hour; focused time is what the client
        // measured.
        assert_eq!(rows[0].session.elapsed_ms(0), 60 * 60 * 1000);
        assert_eq!(rows[0].focused_ms, 20 * 60 * 1000);
    }

    #[test]
    fn ending_an_unknown_session_is_not_found() {
        let mut db = db();
        let (e, _clock) = focus_engine(0);
        let ghost = EntityRef::new(EntityKind::FocusSession, [0x5A; 16]);
        let err = e
            .apply(
                &mut db,
                Command::EndFocus {
                    session: ghost,
                    actual_focused_ms: None,
                    completed_task: false,
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::NotFound(_)), "{err:?}");
        // And a non-session id is rejected on the kind, not on lookup.
        let err = e
            .apply(
                &mut db,
                Command::EndFocus {
                    session: EntityRef::new(EntityKind::Task, [1; 16]),
                    actual_focused_ms: None,
                    completed_task: false,
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn interruptions_accumulate_and_ride_the_end_op() {
        let mut db = db();
        let (e, clock) = focus_engine(1_000);
        let task = task_with(&e, &mut db, "deep work", TaskDraft::default());
        let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);

        set_clock(&clock, 2_000);
        e.apply(
            &mut db,
            Command::LogInterruption {
                session,
                reason: InterruptionReason::Meeting,
            },
        )
        .unwrap();
        set_clock(&clock, 3_000);
        e.apply(
            &mut db,
            Command::LogInterruption {
                session,
                reason: InterruptionReason::SelfInterrupt,
            },
        )
        .unwrap();

        // Visible on the running session, before any end op exists.
        let rows = running(&e, &db);
        assert_eq!(rows[0].session.interruptions.len(), 2);

        set_clock(&clock, 4_000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap();
        let rows = sessions_for(&e, &db, task);
        assert_eq!(
            rows[0].session.end.as_ref().unwrap().interruptions.len(),
            2,
            "the end op carries the interruptions the ending device knew about"
        );

        let s = stats(&e, &db, 4_000);
        assert_eq!(s.interruptions, 2);
        assert_eq!(s.top_interruptions.len(), 2);
    }

    #[test]
    fn a_break_session_is_sized_by_the_cycle_not_the_estimate() {
        let mut db = db();
        let (e, clock) = focus_engine(0);
        let task = task_with(
            &e,
            &mut db,
            "cycle",
            TaskDraft {
                estimated_duration_s: Some(10 * 60 * 60),
                ..Default::default()
            },
        );
        // Four completed work sessions, then a break: the long one.
        for i in 0..4u64 {
            set_clock(&clock, i * 1_000_000);
            let s = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
            set_clock(&clock, i * 1_000_000 + POMODORO_MS);
            e.apply(
                &mut db,
                Command::EndFocus {
                    session: s,
                    actual_focused_ms: None,
                    completed_task: false,
                },
            )
            .unwrap();
        }
        set_clock(&clock, 9_000_000);
        let brk = e
            .apply(
                &mut db,
                Command::StartFocus(FocusStartDraft {
                    task_id: task,
                    kind: FocusKind::Break,
                    length: SessionLength::SizedToEstimate,
                    energy: None,
                }),
            )
            .unwrap()
            .entity;
        let row = running(&e, &db)
            .into_iter()
            .find(|r| r.session.start.id == brk)
            .unwrap();
        assert_eq!(row.session.start.kind, FocusKind::Break);
        assert_eq!(
            row.session.start.planned_ms,
            Some(sunrise_domain::LONG_BREAK_MS),
            "the fourth cycle earns the long break"
        );
        assert_eq!(row.session.start.chunk, None);
    }

    #[test]
    fn a_long_estimate_chunks_and_the_index_advances_with_prior_sessions() {
        let mut db = db();
        let (e, clock) = focus_engine(0);
        let task = task_with(
            &e,
            &mut db,
            "90 minutes of work",
            TaskDraft {
                estimated_duration_s: Some(90 * 60),
                ..Default::default()
            },
        );
        let first = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
        let row = sessions_for(&e, &db, task)
            .into_iter()
            .find(|r| r.session.start.id == first)
            .unwrap();
        assert_eq!(row.session.start.chunk, Some(Chunk { index: 1, total: 4 }));
        assert_eq!(row.session.start.planned_ms, Some(POMODORO_MS));

        set_clock(&clock, POMODORO_MS);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: first,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap();
        set_clock(&clock, POMODORO_MS + 1);
        let second = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
        let row = sessions_for(&e, &db, task)
            .into_iter()
            .find(|r| r.session.start.id == second)
            .unwrap();
        assert_eq!(
            row.session.start.chunk,
            Some(Chunk { index: 2, total: 4 }),
            "the second sitting reads chunk 2 of 4"
        );
    }

    // ---- the planner ----

    fn plan(e: &Engine, db: &Db, energy: Option<Energy>) -> Vec<crate::queries::FocusPlanRow> {
        match e
            .query(
                db,
                Query::FocusPlan {
                    stream: None,
                    energy,
                    length: SessionLength::OnePomodoro,
                    limit: 10,
                },
            )
            .unwrap()
        {
            QueryResult::FocusPlan(v) => v,
            other => panic!("expected FocusPlan, got {other:?}"),
        }
    }

    #[test]
    fn planner_prefers_the_high_leverage_actionable_task_at_matching_energy() {
        let mut db = db();
        let (e, _clock) = focus_engine(0);
        // Two actionable High-energy tasks. `hub` releases two dependents;
        // `chore` releases nothing.
        let hub = task_with(
            &e,
            &mut db,
            "build the thing",
            TaskDraft {
                energy: Some(Energy::High),
                ..Default::default()
            },
        );
        let chore = task_with(
            &e,
            &mut db,
            "tidy the desk",
            TaskDraft {
                energy: Some(Energy::High),
                ..Default::default()
            },
        );
        for title in ["deploy", "qa"] {
            let dep = task_with(&e, &mut db, title, TaskDraft::default());
            e.apply(
                &mut db,
                Command::UpdateTask {
                    id: dep,
                    patch: TaskPatch {
                        blocked_by: Some(vec![hub]),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        }

        let rows = plan(&e, &db, Some(Energy::High));
        assert_eq!(rows[0].task.id, hub, "leverage picks the hub: {rows:#?}");
        assert_eq!(rows[0].unblocks, 2);
        assert_eq!(rows[0].energy_fit, sunrise_domain::EnergyFit::Exact);
        assert!(rows.iter().any(|r| r.task.id == chore));

        // The two blocked dependents never appear: the planner is not a dead
        // end.
        assert_eq!(
            rows.len(),
            2,
            "only the two actionable tasks are proposed: {rows:#?}"
        );
    }

    #[test]
    fn planner_matches_energy_before_leverage() {
        let mut db = db();
        let (e, _clock) = focus_engine(0);
        let deep = task_with(
            &e,
            &mut db,
            "rewrite the parser",
            TaskDraft {
                energy: Some(Energy::High),
                ..Default::default()
            },
        );
        let shallow = task_with(
            &e,
            &mut db,
            "file receipts",
            TaskDraft {
                energy: Some(Energy::Low),
                ..Default::default()
            },
        );
        // Give the deep task real leverage so only the energy budget can
        // demote it.
        let dep = task_with(&e, &mut db, "downstream", TaskDraft::default());
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: dep,
                patch: TaskPatch {
                    blocked_by: Some(vec![deep]),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // Depleted: the low-energy task wins despite zero leverage.
        let rows = plan(&e, &db, Some(Energy::Low));
        assert_eq!(rows[0].task.id, shallow, "{rows:#?}");
        // Fresh: leverage takes over again.
        let rows = plan(&e, &db, Some(Energy::High));
        assert_eq!(rows[0].task.id, deep, "{rows:#?}");
        // No declared budget: energy carries no signal, leverage decides.
        let rows = plan(&e, &db, None);
        assert_eq!(rows[0].task.id, deep, "{rows:#?}");
    }

    #[test]
    fn planner_rows_carry_the_session_it_would_open() {
        let mut db = db();
        let (e, _clock) = focus_engine(0);
        let task = task_with(
            &e,
            &mut db,
            "big one",
            TaskDraft {
                estimated_duration_s: Some(75 * 60),
                ..Default::default()
            },
        );
        let rows = plan(&e, &db, None);
        let row = rows.iter().find(|r| r.task.id == task).unwrap();
        assert_eq!(row.prior_sessions, 0);
        assert_eq!(row.suggested.planned_ms, Some(POMODORO_MS));
        assert_eq!(row.suggested.chunk, Some(Chunk { index: 1, total: 3 }));

        // After one sitting the proposal advances the chunk index.
        let s = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: s,
                actual_focused_ms: Some(POMODORO_MS),
                completed_task: false,
            },
        )
        .unwrap();
        let rows = plan(&e, &db, None);
        let row = rows.iter().find(|r| r.task.id == task).unwrap();
        assert_eq!(row.prior_sessions, 1);
        assert_eq!(row.suggested.chunk, Some(Chunk { index: 2, total: 3 }));
    }

    // ---- calibration + cascade ----

    #[test]
    fn focus_stats_calibrate_from_recorded_actuals() {
        let mut db = db();
        let (e, clock) = focus_engine(0);
        // A 30-minute estimate that actually took 51 minutes => 1.7x.
        let task = task_with(
            &e,
            &mut db,
            "estimate me",
            TaskDraft {
                estimated_duration_s: Some(30 * 60),
                energy: Some(Energy::High),
                ..Default::default()
            },
        );
        let s = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
        set_clock(&clock, 51 * 60 * 1000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: s,
                actual_focused_ms: None,
                completed_task: true,
            },
        )
        .unwrap();

        let st = stats(&e, &db, 51 * 60 * 1000);
        assert_eq!(st.sessions, 1);
        assert_eq!(st.running, 0);
        assert_eq!(st.total_focused_ms, 51 * 60 * 1000);
        let c = st.overall.expect("calibrates");
        assert!((c.factor - 1.7).abs() < 1e-9, "factor was {}", c.factor);
        // Bucketed per Stream and per energy too.
        assert_eq!(st.per_stream.len(), 1);
        assert!((st.per_stream[0].calibration.unwrap().factor - 1.7).abs() < 1e-9);
        assert_eq!(st.per_energy[0].energy, Some(Energy::High));
    }

    #[test]
    fn focus_stats_filter_by_stream_and_since() {
        let mut db = db();
        let (e, clock) = focus_engine(1_000);
        let task = task_with(&e, &mut db, "inbox work", TaskDraft::default());
        let early = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        set_clock(&clock, 2_000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: early,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap();
        set_clock(&clock, 50_000);
        let late = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        set_clock(&clock, 60_000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: late,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap();

        let all = stats(&e, &db, 60_000);
        assert_eq!(all.sessions, 2);
        let recent = match e
            .query(
                &db,
                Query::FocusStats {
                    stream: None,
                    since_ms: Some(10_000),
                    now_ms: 60_000,
                },
            )
            .unwrap()
        {
            QueryResult::FocusStats(s) => *s,
            other => panic!("{other:?}"),
        };
        assert_eq!(recent.sessions, 1);
        assert_eq!(recent.total_focused_ms, 10_000);
    }

    #[test]
    fn unblock_cascade_reports_what_a_completion_released() {
        let mut db = db();
        let (e, _clock) = focus_engine(0);
        let build = task_with(&e, &mut db, "build", TaskDraft::default());
        let docs = task_with(&e, &mut db, "docs", TaskDraft::default());
        let deploy = task_with(&e, &mut db, "deploy", TaskDraft::default());
        let qa = task_with(&e, &mut db, "qa", TaskDraft::default());
        for (dep, blockers) in [(deploy, vec![build]), (qa, vec![build, docs])] {
            e.apply(
                &mut db,
                Command::UpdateTask {
                    id: dep,
                    patch: TaskPatch {
                        blocked_by: Some(blockers),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        }

        // Mid-session completion of `build`.
        e.apply(&mut db, Command::CompleteTask(build)).unwrap();
        let cascade = match e.query(&db, Query::UnblockCascade(build)).unwrap() {
            QueryResult::UnblockCascade(c) => *c,
            other => panic!("{other:?}"),
        };
        assert_eq!(cascade.completed, build);
        assert_eq!(cascade.released, vec![deploy], "deploy is free now");
        assert_eq!(cascade.still_blocked, vec![qa], "qa still waits on docs");

        // Finishing `docs` releases `qa` too, with no repair pass in between.
        e.apply(&mut db, Command::CompleteTask(docs)).unwrap();
        let cascade = match e.query(&db, Query::UnblockCascade(docs)).unwrap() {
            QueryResult::UnblockCascade(c) => *c,
            other => panic!("{other:?}"),
        };
        assert_eq!(cascade.released, vec![qa]);
        assert!(cascade.still_blocked.is_empty());
    }

    #[test]
    fn two_concurrent_sessions_on_one_task_both_survive_and_aggregate() {
        // The single-replica shape of the ADR's convergence property: two
        // sessions minted independently are two rows, not one contended
        // register, and their focused time sums.
        let mut db = db();
        let (e, clock) = focus_engine(0);
        let task = task_with(
            &e,
            &mut db,
            "shared work",
            TaskDraft {
                estimated_duration_s: Some(20 * 60),
                ..Default::default()
            },
        );
        let a = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        let b = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        assert_ne!(a, b, "concurrent starts mint distinct ids");
        assert_eq!(running(&e, &db).len(), 2);

        set_clock(&clock, 600_000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: a,
                actual_focused_ms: Some(600_000),
                completed_task: false,
            },
        )
        .unwrap();
        e.apply(
            &mut db,
            Command::EndFocus {
                session: b,
                actual_focused_ms: Some(600_000),
                completed_task: true,
            },
        )
        .unwrap();

        let st = stats(&e, &db, 600_000);
        assert_eq!(st.sessions, 2, "both survive");
        assert_eq!(st.total_focused_ms, 1_200_000, "and both count");
        // 20 minutes of focus against a 20-minute estimate: bang on.
        let c = st.overall.expect("calibrates");
        assert!((c.factor - 1.0).abs() < 1e-9, "factor was {}", c.factor);
        assert_eq!(c.samples, 1, "one task, two sessions");
    }

    #[test]
    fn focus_ops_are_append_only_on_the_remote_path() {
        // Two replicas of one vault. The `end` op is delivered BEFORE the
        // `start` it belongs to — the arrival order that a single mutable row
        // would silently drop.
        let clock_a = Arc::new(FakeClock(PLMutex::new(1_000)));
        let clock_b = Arc::new(FakeClock(PLMutex::new(1_000)));
        let ea = engine_seeded([0x77; 32], [1; 32], clock_a.clone());
        let eb = engine_seeded([0x77; 32], [2; 32], clock_b.clone());
        let mut da = db_root([0x77; 32]);
        let mut dbb = db_root([0x77; 32]);
        // Mutual trust so envelopes verify.
        trust(&ea, &mut da, &eb);
        trust(&eb, &mut dbb, &ea);

        let task = task_with(&ea, &mut da, "cross-device", TaskDraft::default());
        let task_env = create_env_for(&da, task.bytes());
        eb.apply_remote(&mut dbb, &task_env).unwrap();

        let session = start_work(&ea, &mut da, task, SessionLength::OnePomodoro);
        set_clock(&clock_a, 1_000 + 60_000);
        ea.apply(
            &mut da,
            Command::EndFocus {
                session,
                actual_focused_ms: None,
                completed_task: true,
            },
        )
        .unwrap();

        let env_of = |kind: &str| -> Vec<u8> {
            let op_id: Vec<u8> = da
                .conn()
                .query_row(
                    "SELECT op_id FROM ops WHERE target_id = ? AND inner_kind = ?",
                    params![&session.bytes()[..], kind],
                    |r| r.get(0),
                )
                .unwrap();
            let mut id = [0u8; 16];
            id.copy_from_slice(&op_id);
            env_bytes(&da, &id)
        };
        let start_env = env_of("focus.start");
        let end_env = env_of("focus.end");

        // Out of order on purpose, and each delivered twice.
        for env in [&end_env, &end_env, &start_env, &start_env] {
            eb.apply_remote(&mut dbb, env).unwrap();
        }

        set_clock(&clock_b, 9_999_999);
        let rows = sessions_for(&eb, &dbb, task);
        assert_eq!(rows.len(), 1, "re-delivery did not duplicate the session");
        assert!(!rows[0].running, "the end that arrived first still applied");
        assert_eq!(
            rows[0].focused_ms, 60_000,
            "the frozen value crossed intact"
        );
        assert!(rows[0].session.end.as_ref().unwrap().completed_task);
    }

    // -----------------------------------------------------------------------
    // Reviews and stats (`docs/08-features/reviews-and-stats.md`)
    //
    // These go through the *real* pipeline — every fact the folds see has been
    // sealed into an envelope, written to the op log, and decrypted back out —
    // so they prove the op log really is a usable source of truth for history,
    // not just that the pure folds add up.
    // -----------------------------------------------------------------------

    /// Monday 2026-01-05T00:00:00Z, so every week boundary below is hand-checkable.
    const REVIEW_MON: u64 = 1_767_571_200_000;
    const REVIEW_DAY: u64 = 24 * 60 * 60 * 1000;
    const REVIEW_WEEK: u64 = 7 * REVIEW_DAY;

    /// An engine whose clock the test drives, plus its DB.
    fn review_fixture() -> (Engine, Db, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock(PLMutex::new(REVIEW_MON)));
        let e = engine_seeded([0xab; 32], [0x11; 32], clock.clone());
        (e, db(), clock)
    }

    fn review_task(e: &Engine, db: &mut Db, title: &str, stream: Option<EntityRef>) -> EntityRef {
        e.apply(
            db,
            Command::CreateTask(TaskDraft {
                title: title.into(),
                stream_id: stream,
                ..Default::default()
            }),
        )
        .unwrap()
        .entity
    }

    fn weekly(e: &Engine, db: &Db, now_ms: u64) -> WeeklyReview {
        match e
            .query(
                db,
                Query::WeeklyReview {
                    week_start_ms: None,
                    now_ms,
                },
            )
            .unwrap()
        {
            QueryResult::WeeklyReview(r) => *r,
            other => panic!("expected WeeklyReview, got {other:?}"),
        }
    }

    fn timeline(e: &Engine, db: &Db, entity: EntityRef) -> Vec<ActivityEvent> {
        match e
            .query(db, Query::ActivityTimeline { entity, limit: 100 })
            .unwrap()
        {
            QueryResult::Activity(v) => v,
            other => panic!("expected Activity, got {other:?}"),
        }
    }

    #[test]
    fn activity_timeline_reads_the_op_log_back_through_the_envelope_seal() {
        let (e, mut db, clock) = review_fixture();
        let task = review_task(&e, &mut db, "write the RFC", None);

        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("write the RFC properly".into()),
                    priority: Some(Some(2)),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
        e.apply(
            &mut db,
            Command::DeferTask {
                id: task,
                to_ms: REVIEW_MON + 5 * REVIEW_DAY,
            },
        )
        .unwrap();

        set_clock(&clock, REVIEW_MON + 3 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();

        // Newest first, one event per user action, and the field edit is
        // summarized rather than expanded.
        let feed = timeline(&e, &db, task);
        let verbs: Vec<&str> = feed.iter().map(|f| f.kind.verb()).collect();
        assert_eq!(
            verbs,
            vec![
                "task.completed",
                "task.deferred",
                "task.updated",
                "task.created"
            ]
        );
        assert_eq!(
            feed[2].kind,
            ActivityKind::TaskUpdated { fields: 2 },
            "title + priority, and nothing else"
        );
        assert_eq!(feed[0].at_ms, REVIEW_MON + 3 * REVIEW_DAY);
        assert_eq!(
            feed[0].device,
            e.keychain().device_id(),
            "the feed says which device did it"
        );
    }

    #[test]
    fn a_focus_session_shows_up_on_the_tasks_timeline() {
        let (e, mut db, clock) = review_fixture();
        let task = review_task(&e, &mut db, "deep work", None);

        set_clock(&clock, REVIEW_MON + 3_600_000);
        let session = e
            .apply(
                &mut db,
                Command::StartFocus(FocusStartDraft {
                    task_id: task,
                    kind: FocusKind::Work,
                    length: SessionLength::OnePomodoro,
                    energy: None,
                }),
            )
            .unwrap()
            .entity;
        set_clock(&clock, REVIEW_MON + 3_600_000 + POMODORO_MS);
        e.apply(
            &mut db,
            Command::EndFocus {
                session,
                actual_focused_ms: Some(POMODORO_MS),
                completed_task: false,
            },
        )
        .unwrap();

        let feed = timeline(&e, &db, task);
        assert_eq!(
            feed.iter().map(|f| f.kind.verb()).collect::<Vec<_>>(),
            vec!["focus.ended", "focus.started", "task.created"],
        );
        assert_eq!(
            feed[0].kind,
            ActivityKind::FocusEnded {
                session,
                focused_ms: POMODORO_MS,
                completed_task: false,
            }
        );
    }

    #[test]
    fn a_streams_timeline_covers_the_tasks_that_live_in_it() {
        let (e, mut db, clock) = review_fixture();
        let stream = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        set_clock(&clock, REVIEW_MON + 1_000);
        review_task(&e, &mut db, "in the stream", Some(stream));
        review_task(&e, &mut db, "also in the stream", Some(stream));
        review_task(&e, &mut db, "in the inbox", None);

        let feed = timeline(&e, &db, stream);
        assert_eq!(
            feed.iter()
                .filter(|f| f.kind == ActivityKind::TaskCreated)
                .count(),
            2,
            "only the stream's own tasks: {feed:#?}"
        );
        assert!(feed.iter().any(|f| f.kind == ActivityKind::StreamCreated));
    }

    #[test]
    fn a_routine_generated_task_never_appears_as_user_activity() {
        let (e, mut db, _clock) = review_fixture();
        let stream = stream_ref(7);
        let rid = e
            .apply(
                &mut db,
                Command::CreateRoutine(routine_draft(
                    stream,
                    "FREQ=DAILY",
                    REVIEW_MON as i64,
                    RoutineCatchupPolicy::Skip,
                    Vec::new(),
                )),
            )
            .unwrap()
            .entity;
        // Materialization produced tasks; none of them are user activity.
        let generated: Vec<[u8; 16]> = live_task_ids(&db).into_iter().collect();
        assert!(!generated.is_empty(), "the routine materialized something");
        for raw in generated {
            let feed = timeline(&e, &db, EntityRef::new(EntityKind::Task, raw));
            assert!(
                !feed.iter().any(|f| f.kind == ActivityKind::TaskCreated),
                "routine generation is not user activity"
            );
        }
        assert_eq!(rid.kind(), EntityKind::Routine);
    }

    #[test]
    fn activity_timeline_rejects_an_entity_that_has_no_feed() {
        let (e, db, _clock) = review_fixture();
        let err = e
            .query(
                &db,
                Query::ActivityTimeline {
                    entity: EntityRef::new(EntityKind::Context, [1u8; 16]),
                    limit: 10,
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn the_weekly_review_counts_the_week_it_covers_and_nothing_else() {
        let (e, mut db, clock) = review_fixture();

        // Last week: one task created and completed. It must not be counted.
        set_clock(&clock, REVIEW_MON - 4 * REVIEW_DAY);
        let old = review_task(&e, &mut db, "last week's work", None);
        set_clock(&clock, REVIEW_MON - 3 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(old)).unwrap();

        // This week.
        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        let done = review_task(&e, &mut db, "finish the report", None);
        let deferred = review_task(&e, &mut db, "call the bank", None);
        let untouched = review_task(&e, &mut db, "read the RFC", None);
        let dropped = review_task(&e, &mut db, "abandon this", None);

        set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(done)).unwrap();
        e.apply(
            &mut db,
            Command::DeferTask {
                id: deferred,
                to_ms: REVIEW_MON + 6 * REVIEW_DAY,
            },
        )
        .unwrap();
        e.apply(&mut db, Command::DeleteTask(dropped)).unwrap();

        let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
        assert_eq!(r.window.start_ms, REVIEW_MON);
        assert_eq!(r.window.end_ms, REVIEW_MON + REVIEW_WEEK);
        assert_eq!(r.totals.completed, 1, "last week's completion is excluded");
        assert_eq!(r.totals.deferred, 1);
        assert_eq!(r.totals.dropped, 1);
        assert_eq!(r.totals.created, 4);

        // The Inbox row is the stream everything landed in.
        let inbox = r
            .streams
            .iter()
            .find(|s| s.stream == inbox_stream_ref())
            .expect("the inbox is reviewed too");
        assert_eq!(
            inbox
                .completed
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["finish the report"]
        );
        assert_eq!(
            inbox
                .deferred
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["call the bank"]
        );
        assert_eq!(
            inbox
                .created_untouched
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["read the RFC"],
            "the deleted one was acted on; the others were completed or deferred"
        );
        assert!(
            r.inbox.iter().any(|t| t.id == untouched),
            "step 2 offers the untriaged capture for triage"
        );
    }

    #[test]
    fn a_paused_stream_drops_out_of_the_review_and_the_pause_survives_a_reread() {
        let (e, mut db, clock) = review_fixture();
        let work = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        let task = review_task(&e, &mut db, "in the paused stream", Some(work));
        set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();

        // Before the pause the stream is reviewed.
        let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
        assert!(r.streams.iter().any(|s| s.stream == work));

        e.apply(
            &mut db,
            Command::UpdateStream {
                id: work,
                patch: StreamPatch {
                    paused: Some(true),
                    review_cadence: Some(StreamReviewCadence::Monthly),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // The pause is a persisted projection now, not a value that lives only
        // inside the op that set it.
        match e.query(&db, Query::EntityById(work)).unwrap() {
            QueryResult::Stream(s) => {
                assert!(s.paused, "the pause survived the round trip through SQL");
                assert_eq!(s.review_cadence, StreamReviewCadence::Monthly);
            }
            other => panic!("expected Stream, got {other:?}"),
        }

        let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
        assert!(
            !r.streams.iter().any(|s| s.stream == work),
            "the spec reviews non-archived, NON-PAUSED streams: {:#?}",
            r.streams.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(
            r.totals.completed, 1,
            "pausing a stream hides its section, it does not rewrite the week's counts"
        );
    }

    #[test]
    fn the_weekly_review_lists_a_commitment_that_came_due_and_stayed_open() {
        let (e, mut db, clock) = review_fixture();
        set_clock(&clock, REVIEW_MON);
        let slipped = review_task(&e, &mut db, "renew the domain", None);
        let met = review_task(&e, &mut db, "pay the invoice", None);
        for id in [slipped, met] {
            e.apply(
                &mut db,
                Command::UpdateTask {
                    id,
                    patch: TaskPatch {
                        due_at: Some(Some(ms_to_ts((REVIEW_MON + 2 * REVIEW_DAY) as i64).into())),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        }
        set_clock(&clock, REVIEW_MON + 3 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(met)).unwrap();

        let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
        assert_eq!(
            r.slipped
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["renew the domain"],
            "a commitment that was met is not a slipped one"
        );
    }

    #[test]
    fn the_weekly_review_surfaces_the_estimate_calibration_focus_produced() {
        let (e, mut db, clock) = review_fixture();
        // A task estimated at 30 minutes that actually took 51 — the spec's
        // "your 30-minute estimates run ~1.7x long".
        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "the 30-minute job".into(),
                    estimated_duration_s: Some(30 * 60),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let session = e
            .apply(
                &mut db,
                Command::StartFocus(FocusStartDraft {
                    task_id: task,
                    kind: FocusKind::Work,
                    length: SessionLength::UntilDone,
                    energy: None,
                }),
            )
            .unwrap()
            .entity;
        set_clock(&clock, REVIEW_MON + REVIEW_DAY + 51 * 60 * 1000);
        e.apply(
            &mut db,
            Command::EndFocus {
                session,
                actual_focused_ms: Some(51 * 60 * 1000),
                completed_task: true,
            },
        )
        .unwrap();

        let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
        let c = r.focus.overall.expect("a calibration factor");
        assert_eq!(c.samples, 1);
        assert!(
            (c.factor - 1.7).abs() < 1e-9,
            "51 minutes against a 30-minute estimate is 1.7x, got {}",
            c.factor
        );
        assert_eq!(r.focus.total_focused_ms, 51 * 60 * 1000);
    }

    /// The whole-vault + per-Stream trend, as the caller sees it.
    fn trends_of(e: &Engine, db: &Db, now: u64) -> Trends {
        match e
            .query(
                db,
                Query::StreamTrends {
                    weeks: 4,
                    now_ms: now,
                },
            )
            .unwrap()
        {
            QueryResult::Trends(t) => *t,
            other => panic!("expected Trends, got {other:?}"),
        }
    }

    #[test]
    fn the_trend_is_stable_across_a_reopen() {
        let (e, mut db, clock) = review_fixture();
        // Completed in the week before last.
        set_clock(&clock, REVIEW_MON - 10 * REVIEW_DAY);
        let task = review_task(&e, &mut db, "the wobbly one", None);
        set_clock(&clock, REVIEW_MON - 9 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();

        let before = trends_of(&e, &db, REVIEW_MON + REVIEW_DAY);
        assert_eq!(
            before.overall.iter().map(|b| b.completed).sum::<i64>(),
            1,
            "one completion in the window"
        );

        // Re-open it this week. The spec: the decrement lands on the week of
        // the ORIGINAL completion, so the total goes to zero and the *earlier*
        // bucket is the one that moved.
        set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    state: Some(TaskState::Todo),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        let after = trends_of(&e, &db, REVIEW_MON + 3 * REVIEW_DAY);
        assert_eq!(
            after.overall.iter().map(|b| b.completed).sum::<i64>(),
            0,
            "the chart nets out: {:#?}",
            after.overall
        );
        assert!(
            after.overall.last().is_some_and(|b| b.completed == 0),
            "the current week was never credited a completion it did not have"
        );
    }

    #[test]
    fn a_saved_review_snapshot_is_queryable_in_history() {
        let (e, mut db, clock) = review_fixture();
        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        let task = review_task(&e, &mut db, "something to count", None);
        set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();

        let r = weekly(&e, &db, REVIEW_MON + 4 * REVIEW_DAY);
        set_clock(&clock, REVIEW_MON + 4 * REVIEW_DAY);
        let saved = e
            .apply(
                &mut db,
                Command::SaveReviewSnapshot(r.to_draft(Some("a quiet week".into()))),
            )
            .unwrap();
        assert_eq!(saved.entity.kind(), EntityKind::ReviewSnapshot);

        let history = match e.query(&db, Query::ReviewHistory { limit: 10 }).unwrap() {
            QueryResult::ReviewSnapshots(v) => v,
            other => panic!("expected ReviewSnapshots, got {other:?}"),
        };
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, saved.entity);
        assert_eq!(history[0].window_start_ms(), REVIEW_MON);
        assert_eq!(history[0].window_end_ms(), REVIEW_MON + REVIEW_WEEK);
        assert_eq!(history[0].totals.completed, 1);
        assert_eq!(history[0].note.as_deref(), Some("a quiet week"));

        // Saving again records a *second* review rather than replacing the
        // first: a snapshot is an event, not a register.
        set_clock(&clock, REVIEW_MON + 5 * REVIEW_DAY);
        e.apply(&mut db, Command::SaveReviewSnapshot(r.to_draft(None)))
            .unwrap();
        let history = match e.query(&db, Query::ReviewHistory { limit: 10 }).unwrap() {
            QueryResult::ReviewSnapshots(v) => v,
            other => panic!("expected ReviewSnapshots, got {other:?}"),
        };
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn export_csv_survives_a_task_title_full_of_delimiters() {
        let (e, mut db, clock) = review_fixture();
        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        // A title with a comma, a quote pair and a newline: the three things
        // that shift a column in a naive writer.
        let hostile = "Ship \"v1\", then\nrest";
        let task = review_task(&e, &mut db, hostile, None);
        set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
        e.apply(&mut db, Command::CompleteTask(task)).unwrap();

        let csv = match e
            .query(
                &db,
                Query::ExportStats {
                    dataset: ExportDataset::Activity,
                    format: ExportFormat::Csv,
                    weeks: 2,
                    now_ms: REVIEW_MON + 3 * REVIEW_DAY,
                },
            )
            .unwrap()
        {
            QueryResult::Export(s) => s,
            other => panic!("expected Export, got {other:?}"),
        };
        // The label column holds the title verbatim, quoted, and every record
        // still has the header's six fields.
        assert!(
            csv.contains("\"Ship \"\"v1\"\", then\nrest\""),
            "title was not quoted correctly:\n{csv}"
        );
        let header_fields = csv.lines().next().unwrap().split(',').count();
        assert_eq!(header_fields, 6);

        let json = match e
            .query(
                &db,
                Query::ExportStats {
                    dataset: ExportDataset::Activity,
                    format: ExportFormat::Json,
                    weeks: 2,
                    now_ms: REVIEW_MON + 3 * REVIEW_DAY,
                },
            )
            .unwrap()
        {
            QueryResult::Export(s) => s,
            other => panic!("expected Export, got {other:?}"),
        };
        assert!(
            json.contains(r#"Ship \"v1\", then\nrest"#),
            "JSON escaping lost the title:\n{json}"
        );
    }

    #[test]
    fn export_trends_and_streaks_render_in_both_formats() {
        let (e, mut db, clock) = review_fixture();
        set_clock(&clock, REVIEW_MON + REVIEW_DAY);
        review_task(&e, &mut db, "a task", None);
        for (dataset, format) in [
            (ExportDataset::Trends, ExportFormat::Csv),
            (ExportDataset::Trends, ExportFormat::Json),
            (ExportDataset::Focus, ExportFormat::Csv),
            (ExportDataset::Streaks, ExportFormat::Json),
        ] {
            let out = match e
                .query(
                    &db,
                    Query::ExportStats {
                        dataset,
                        format,
                        weeks: 3,
                        now_ms: REVIEW_MON + 3 * REVIEW_DAY,
                    },
                )
                .unwrap()
            {
                QueryResult::Export(s) => s,
                other => panic!("expected Export, got {other:?}"),
            };
            match format {
                ExportFormat::Csv => assert!(
                    out.ends_with("\r\n") && !out.contains(",,,,,,"),
                    "{dataset:?} CSV is malformed:\n{out}"
                ),
                ExportFormat::Json => assert!(
                    out.starts_with(&format!("{{\"table\":\"{}\"", dataset.as_str())),
                    "{dataset:?} JSON is not the named table:\n{out}"
                ),
            }
        }
    }

    #[test]
    fn the_daily_review_shows_last_nights_captures_and_the_blocked_subset() {
        let (e, mut db, clock) = review_fixture();
        // Captured last night.
        set_clock(&clock, REVIEW_MON + 22 * 3_600_000);
        let capture = review_task(&e, &mut db, "shower thought", None);
        // Captured a week ago — outside the glance window.
        set_clock(&clock, REVIEW_MON - 3 * REVIEW_DAY);
        review_task(&e, &mut db, "old capture", None);

        // Two tasks planned for today, one waiting on the other. They live in
        // a real Stream so the inbox assertion below is about captures alone.
        set_clock(&clock, REVIEW_MON + 25 * 3_600_000);
        let work = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let blocker = review_task(&e, &mut db, "the blocker", Some(work));
        let waiting = review_task(&e, &mut db, "the dependent", Some(work));
        for id in [blocker, waiting] {
            e.apply(
                &mut db,
                Command::UpdateTask {
                    id,
                    patch: TaskPatch {
                        scheduled_at: Some(Some(
                            ms_to_ts((REVIEW_MON + 25 * 3_600_000) as i64).into(),
                        )),
                        blocked_by: if id == waiting {
                            Some(vec![blocker])
                        } else {
                            None
                        },
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        }

        let daily = match e
            .query(
                &db,
                Query::DailyReview {
                    since_ms: REVIEW_MON + 18 * 3_600_000,
                    now_ms: REVIEW_MON + 26 * 3_600_000,
                },
            )
            .unwrap()
        {
            QueryResult::DailyReview(d) => *d,
            other => panic!("expected DailyReview, got {other:?}"),
        };
        assert_eq!(
            daily.inbox.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![capture],
            "only captures inside the glance window: {:#?}",
            daily.inbox
        );
        assert_eq!(
            daily.blocked.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![waiting],
            "the blocker itself is not blocked"
        );
    }
}
