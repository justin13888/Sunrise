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
//!
//! ## Module map
//!
//! This module is a directory because one file of it was six impl blocks and
//! ninety free functions. The split follows what the code *touches*, not what
//! calls it. Carved out so far: `ids`, `lww`, `oplog`.

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
use crate::control_op::{DeviceRevokePayload, Recipient, RevokeReason};
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
use sunrise_storage::{Db, OpLog};
use thiserror::Error;

mod ids;
mod lww;
mod oplog;
#[cfg(test)]
mod tests;

// `META_STREAM` and `hex_short` are named from outside `engine` — `core.rs`,
// `keychain.rs` and `sync_driver.rs` — so the path they name has to keep
// resolving across this split. `META_STREAM` stays defined below; `hex_short`
// moved to `ids` and is re-exported here at its old name.
pub(crate) use self::ids::hex_short;
use self::ids::{
    blob16, blob32, decode_unknowns, encode_unknowns, energy_str, ms_to_ts, parse_energy,
    parse_task_state, require_kind, task_state_str, time_from_parts, time_to_parts,
};
use self::lww::{materialize_remote, remap_legacy_inbox, LwwStamp};
use self::oplog::{record_envelope_recipient, remote_op_id, upsert_sync_cursor};

/// Vault-meta op-log stream id: 16 zero bytes.
///
/// Routing: Stream lifecycle ops (create/update/delete), all routine ops, review
/// snapshots and the control op families are logged under this meta stream,
/// while task ops are logged under their owning Stream's id.
///
/// It is an ordinary member of the rotation set: revoking a device mints a new
/// epoch here as it does for every other stream, so the *shape* of the account
/// rotates along with its contents rather than staying on a fixed key forever.
///
/// That is a property of the rotation, not a bound on a revoked device — this
/// build seals every epoch to the account identity as well, and every paired
/// device holds `ID_D_priv`, so a revoked device reads the new meta epoch like
/// any other. Bounding it is `#76`.
///
/// The Inbox no longer shares this id: see
/// [`sunrise_domain::INBOX_STREAM_BYTES`].
pub(crate) const META_STREAM: [u8; 16] = [0u8; 16];

/// Upper end of the trend window, applied as a silent clamp at **both** ends.
///
/// Enforcement is the single `weeks.clamp(1, MAX_TREND_WEEKS)` in the trend
/// query, and nothing above it validates: a caller asking for 500 weeks gets
/// 104 and a caller asking for 0 gets 1, each with no error and no signal that
/// the request was rewritten. It is a bound on what the fold will *do*, not on
/// what a caller may *ask*.
///
/// Deliberately unlike [`MAX_EPOCH_LEAP`], which refuses what it cannot accept
/// and documents the residual that refusal leaves behind. A rewritten chart
/// window still renders a correct chart of a different size, so there is
/// nothing for a caller to recover from; a rewritten epoch would be a key the
/// caller silently does not hold.
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
/// and not retried. The refusal is a dropped *payload*, not a dropped op: the
/// envelope still enters `ops` and the cursor still advances past it, so the
/// relay will not re-send it and nothing re-offers the key. Ops sealed under
/// that `(stream, epoch)` therefore stay unreadable on this replica until the
/// device is re-paired, which is what hands it every Stream key the inviting
/// device holds. That is the recovery, and it is the same one that covers a
/// device whose `deferred_ops` entry aged out.
///
/// It is logged (`core.key.epoch_refused`) rather than swallowed, because the
/// only ways to reach it are a hostile sender and a bug.
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
/// account, at any `(stream, epoch)` it liked, with no ceiling.
///
/// Overflow evicts the **oldest** rows. Both directions lose something, and it
/// is worth being plain about which:
///
/// * *Oldest-first*, what this does: a flood of fresh junk at one
///   `(stream, epoch)` pushes out the honest ops already waiting there.
/// * *Newest-first*, the alternative: the flood's own first 256 rows occupy the
///   bucket and every honest op arriving behind it is turned away instead.
///
/// Oldest-first wins on two counts. A row that has waited longest is the one
/// whose key is least likely to still be in flight, so it is the cheapest to
/// lose; and the bucket **self-heals** — ordinary traffic pushes the junk out
/// again — where newest-first leaves it poisoned until the TTL sweeps it.
///
/// Either way an evicted op is recoverable, which is what makes this a
/// tolerable trade at all: a deferred op never reached `ops` and never advanced
/// the sync cursor, so the relay still counts it as undelivered and re-sends it
/// on the next reconnect, by which time the key that opens it has almost
/// certainly arrived.
const DEFERRED_TOTAL_CAP: i64 = 4096;

/// How long a parked op is kept before a drain sweeps it away.
///
/// Nothing re-delivers the key for an op this old: the relay's op ring has long
/// since evicted the `key_envelope` that would have released it, and no replica
/// re-emits one on request. Keeping it is keeping ciphertext this device will
/// never read. Thirty days is generous against every offline window a person
/// actually has and still bounds a slow drip that never reaches either cap.
const DEFERRED_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

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
    /// Keychain failure while resolving or absorbing a Stream key.
    #[error("keychain: {0}")]
    Keychain(String),
}

/// One command-application pipeline.
///
/// Owns no state of its own, but it is not a stateless *type*. Three of its four
/// fields are read-only injected sources a test replaces to get deterministic
/// op-ids and stamps: `clock`, `hlc` and `rng`. The fourth, `keychain`, is a
/// shared [`Keychain`] whose Stream-key cache is a `Mutex<HashMap<…>>` written
/// through `&self` by the `cache_insert` that both
/// [`Keychain::mint_epoch`](crate::keychain::Keychain::mint_epoch) and
/// [`Keychain::absorb_stream_key`](crate::keychain::Keychain::absorb_stream_key)
/// call. So cloning an `Engine` hands out a second view of one keychain rather
/// than an independent pipeline.
///
/// That the cache is interior-mutable is deliberate, and why — including why it
/// is allowed to be a superset of the `stream_keys` table — is documented on the
/// `Keychain::cache` field itself rather than re-argued here.
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

    /// Restore this device's HLC from the op log, so that what it emits after
    /// a restart sorts above what it emitted before one.
    ///
    /// `MonotonicHlc` starts at `Hlc::default()` — `(0, 0)` — and nothing
    /// advances it except stamping an op or absorbing a peer's. The physical
    /// half is therefore not the wall clock but `max(wall clock, every stamp
    /// absorbed this process)`, and `Hlc::receive` admits a peer up to
    /// `MAX_DRIFT_MS` ahead of local time. A device that has heard from a peer
    /// whose clock leads sits above its own clock for as long as the lead
    /// lasts; from a zero start after a restart it drops back to the wall
    /// clock, and everything it emits for the next few minutes sorts *below*
    /// ops it emitted before the restart.
    ///
    /// `lww_wins` compares `hlc` before `device` or `seq`, so that inversion is
    /// not the tie `seq` exists to break: the device's newer op loses to its own
    /// older one on every replica that merges both, while the origin — which
    /// runs no LWW gate on its own writes — keeps the new value. Silent
    /// divergence, from an ordinary backgrounded-app restart.
    ///
    /// The op log is the right source because `ops.ts_ms` **is** the stamp's
    /// physical half for every row: the local emit path writes
    /// `hlc.physical_ms` for an op this device mints and
    /// [`Self::apply_remote_all`] writes `env.hlc.physical_ms` for one it
    /// absorbs. Every durable stamp elsewhere — a materialized row's `lww_*`
    /// columns, a `device_revocations` cut — was carried by an op, so the log
    /// dominates all of them and no other table needs reading.
    ///
    /// The logical half is not a column, so it comes from decoding the
    /// envelopes at that one millisecond. That is a handful of rows — usually
    /// one — and it is needed: priming the physical half alone would leave a device
    /// that emitted `(t, 5)` before the restart emitting `(t, 1)` after it,
    /// which is the same inversion in the other half of the pair. An envelope
    /// that will not decode is skipped rather than fatal — it cannot have been
    /// applied, and refusing to open the vault over one is a worse answer than
    /// ignoring it.
    ///
    /// Both queries want `ops_by_ts` (migration 0021), and this is the only
    /// caller that does. Without it they are two full scans of the widest table
    /// in the vault, decrypted a page at a time, on **every** open, and the
    /// cost grows for the life of the vault: measured at 18.1 ms for a 10k-op
    /// log, 183 ms at 100k and 2.01 s at 1M, against about 60 us at all three
    /// sizes with the index (issue #156;
    /// `crates/sunrise-bench/benches/vault_open.rs` re-derives it, and
    /// toggles the index itself so it still reads on both sides of 0021).
    ///
    /// See ADR-0036 (`docs/11-adr/0036-hlc-restored-at-open.md`) for the
    /// decision, including why this is a restore rather than the durable write
    /// on the op path ADR-0016 priced and declined.
    ///
    /// # Errors
    /// Storage failures reading the log.
    pub fn prime_hlc(&self, db: &Db) -> Result<(), EngineError> {
        let conn = db.conn();
        let max_ms: Option<i64> = conn.query_row("SELECT MAX(ts_ms) FROM ops", [], |r| r.get(0))?;
        let Some(physical_ms) = max_ms.and_then(|v| u64::try_from(v).ok()) else {
            return Ok(());
        };
        let mut stmt = conn.prepare("SELECT envelope FROM ops WHERE ts_ms = ?")?;
        let mut rows = stmt.query(params![max_ms])?;
        let mut logical = 0u32;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            if let Ok(env) = decode_envelope(&bytes) {
                if env.hlc.physical_ms == physical_ms {
                    logical = logical.max(env.hlc.logical);
                }
            }
        }
        self.hlc.prime(Hlc {
            physical_ms,
            logical,
        });
        Ok(())
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
            Command::RevokeDevice { device_id, reason } => {
                self.revoke_device(db, device_id, reason)
            }
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

    /// Record a device as revoked, and mint a new epoch for every stream it
    /// could read.
    ///
    /// **This bounds the revoked device's reads, and nothing else.** Nothing
    /// queues anything, and nothing bounds its writes — see
    /// [`Self::apply_remote`] step 2 for why the relay cannot be told and why
    /// peers do not refuse. It records a cut every replica converges on, rotates
    /// every stream in the rotation set, and seals the new epochs to everyone
    /// *except* the device it just revoked — which is only meaningful because
    /// `PairingPayload` no longer carries `ID_D_priv`, so there is no
    /// identity-sealed copy for that device to open instead.
    ///
    /// It cannot bound what the device already had; no rotation can. And it
    /// does not bound the account's *creator*, which keeps `ID_D_priv` until a
    /// recovery blob exists to hold it — self-revocation is refused below, so
    /// reaching that case means revoking the creator from another device.
    ///
    /// Four things happen in one transaction, and the order is the design:
    ///
    /// 0. The revocation register is written **first**, before anything can
    ///    mint. `ensure_stream_epoch` below emits a `key_envelope` per
    ///    recipient, and with an empty register the device this transaction
    ///    exists to revoke would be one of them.
    /// 1. The `DeviceRevoke` op is emitted, and the register write above goes
    ///    through the same [`Self::apply_control_op`] a remote op takes, so
    ///    the local and remote paths cannot disagree.
    /// 2. Every stream in the rotation set — the vault-meta stream and the
    ///    Inbox included, not only user Streams — mints a fresh epoch.
    /// 3. The `key_envelope` ops carrying those keys are sealed under the
    ///    **pre-rotation** vault-meta epoch, because a device that has not yet
    ///    received the new meta key cannot read an op sealed under it.
    ///
    /// The vault-meta stream is in the rotation set so that the *shape* of the
    /// account — its Streams, Contexts and Routines — rotates with its contents
    /// rather than staying on one key forever, which is why a revoked device
    /// stops seeing new Streams and Contexts and not merely new tasks.
    ///
    /// What no rotation could ever do: the revoked device keeps every key it
    /// already held, so it keeps everything it could already read. Rotation
    /// bounds forward exposure, never backward.
    fn revoke_device(
        &self,
        db: &mut Db,
        device_id: EntityRef,
        reason: RevokeReason,
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
        let op_id = self.fresh_op_id(now_ms);
        let revoke = InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: revoked,
            reason_code: reason,
        });
        let inner = encode_inner_op(&revoke)?;

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
            let hlc = self.hlc.send();
            // The register goes first, before anything can mint a key.
            //
            // `ensure_stream_epoch` below can mint the vault-meta stream's
            // first epoch, and minting emits a `key_envelope` per recipient. If
            // the register were still empty at that moment, the device this
            // transaction exists to revoke would be one of those recipients and
            // would receive a key minted by its own revocation. Ordering the
            // write first is the whole fix; nothing downstream needs the
            // register to be absent.
            self.apply_control_op(tx, &revoke, &self.keychain.device_id(), hlc, now_ms)?;

            // The epoch every rotation op is sealed under: read before
            // anything is minted, so it is the epoch the departing devices and
            // the remaining ones all still share.
            let seal_under = self.ensure_stream_epoch(tx, &META_STREAM, now_ms)?;
            seq = self.next_seq_tx(tx, &META_STREAM)?;

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
            // The row is written by `apply_control_op` and by nothing else --
            // it ran above, before the first mint. A second writer here was a
            // real defect and not a tidiness point: this one was an
            // unconditional `UPDATE` while the remote path took a join, so a
            // user revoking an already-revoked device on their own machine
            // moved that machine's cut while every peer kept the other. The
            // local device is then the only replica accepting a window of ops,
            // which is precisely the divergence the register exists to prevent.

            // The relay's half of the revocation, queued rather than called.
            // `revoke_device` has to work with no network -- a device that is
            // gone is the whole scenario -- so the call cannot be part of the
            // command. The row goes in *this* transaction so the intent and the
            // op cannot diverge: a vault that believes it revoked a device and
            // never queued the telling is the failure this whole mechanism
            // exists to prevent. `Core::drain_relay_revocations` makes the call
            // when a session is up, and until then this row is what remembers
            // that it is owed.
            tx.execute(
                "INSERT INTO relay_revocation_intents (device_id, created_at_ms)
                 VALUES (?, ?)
                 ON CONFLICT(device_id) DO NOTHING",
                params![&revoked[..], now_ms],
            )?;

            // 2 + 3. Rotate everything, and seal each new epoch to every
            //        *unrevoked* device. One mechanism holds that, and the
            //        ordering above is what makes it sufficient: the register
            //        row was written before the first mint, and
            //        `emit_key_envelopes` excludes any device with a row. There
            //        is no clock in that path and nothing for a skewed or
            //        restarted one to get wrong.
            //
            //        The exclusion is not cosmetic, because the identity copy
            //        emitted alongside is no longer openable by a device
            //        pairing admitted.
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
    /// 2. Sender lookup — `envelope.device_id` must be a device this vault has
    ///    admitted, else [`EngineError::UnknownDevice`]. Revocation is **not**
    ///    consulted on this path, and that is deliberate rather than pending:
    ///    refusing here is not convergent, because a replica that applied an op
    ///    before the revocation arrived has no way to un-apply it and this
    ///    engine has no projection rebuild. Two replicas with the same op set
    ///    would disagree forever. **Nothing bounds a revoked device's writes
    ///    today.** The relay would have to be told out of band and cannot be:
    ///    it identifies devices by a ULID it minted at registration, and this
    ///    vault knows only its own device id, so there is no id to name in the
    ///    request. That is
    ///    [#80](https://github.com/justin13888/Sunrise/issues/80); a convergent
    ///    peer-side check is
    ///    [#82](https://github.com/justin13888/Sunrise/issues/82).
    /// 3. `verify_envelope` against the stored device pubkey — a bad signature
    ///    is never applied.
    /// 4. Decrypt under the Stream key for the envelope's `(stream_id, epoch)`
    ///    and decode the inner op. **No key is not an error**: the
    ///    `key_envelope` op carrying it may not have arrived, so the op is
    ///    parked in `deferred_ops` and this returns `Ok(vec![])` without
    ///    reaching any step below. It is retried after every absorbed key.
    /// 5. Clock gate: the envelope's `hlc` is observed into this device's HLC.
    ///    A reading beyond `MAX_DRIFT_MS` in the future is refused outright —
    ///    see [`sunrise_cbor::hlc`].
    /// 6. Idempotence gate: `INSERT OR IGNORE` into `ops` on the deterministic
    ///    op-id and the `UNIQUE(stream_id, device_id, seq)` constraint. If the
    ///    op was already present (`changes() == 0`), return `Ok(None)` with no
    ///    materialization and no event.
    /// 7. LWW materialization: the entity's stored `(hlc, device, seq)` stamp
    ///    is compared against the envelope's. The greater tuple wins. A winning
    ///    op performs the same materialized-row upsert the local path does and
    ///    stamps the row with the SENDER's values; a losing op keeps the row
    ///    but stays recorded in the op log.
    /// 8. Advance `sync_cursors(stream_id, device_id)` over the contiguous
    ///    applied prefix.
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

        // b. Sender must be a device this vault has admitted.
        //
        //    One family bypasses the lookup, because it is what *creates* the
        //    row the lookup reads: a `DeviceCertPublish` op carries the
        //    sender's own identity-signed cert, and is self-authenticating —
        //    the envelope signature is checked against the cert's own
        //    `d_s_pub`, and the cert against this vault's account identity. A
        //    stranger's cert fails the second check however it is delivered,
        //    which is exactly what `Command::TrustDevice` could not do.
        //
        //    A *revoked* device's row is found here like any other, and its op
        //    is applied like any other. Revocation is enforced against a
        //    device's *reads* — it is sealed no new epoch — and not against its
        //    writes, which is deliberate and not pending: see this function's
        //    own step 2 above for why refusing here would not converge, and
        //    ADR-0034 (`docs/11-adr/0034-revocation-bounds-reads-not-writes.md`)
        //    for the decision and what would reopen it.
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
                absorbed = self.apply_control_op(tx, &inner, &env.device_id, env.hlc, now_ms)?;
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

    /// Whether `device_id` is revoked: **is there a row**, and nothing else.
    ///
    /// The read half of revocation calls this from
    /// [`Self::backfill_key_envelopes`], and the same presence test is inlined
    /// as an anti-join in [`Self::emit_key_envelopes`], which needs it per row
    /// rather than per call.
    ///
    /// # Why there is no time comparison here
    ///
    /// There was one — `cut_ms <= at_ms` — and it never decided anything except
    /// wrongly. `cut_ms` is the revoking device's HLC physical half, so the
    /// only right-hand side comparable with it is this device's own HLC
    /// reading, which is at or above every peer stamp it has absorbed and
    /// therefore at or above every cut it has recorded. In one process the test
    /// is *always true*, which is fail-closed-on-presence written the long way.
    ///
    /// The one case where it did change an outcome is the case it got wrong.
    /// [`MonotonicHlc`](crate::config::MonotonicHlc) is deliberately not
    /// persisted, [`Engine::from_clock`] builds a fresh one at `Hlc::default()`,
    /// and nothing primes it from the op log or the register at open. So after
    /// any restart the HLC reads 0 while `cut_ms` rows survive, and the
    /// comparison silently collapsed to the bare wall clock it was introduced
    /// to replace — reopening the leak for every cut stamped ahead of local
    /// time, which a peer inside `MAX_DRIFT_MS` produces routinely and a
    /// backgrounded mobile app restarts into as a matter of course.
    ///
    /// So the row is the whole of it. `cut_ms` and `cut_logical` are untouched
    /// and still load-bearing: they are the LWW discriminator deciding *which*
    /// revocation wins when two race (see the `DeviceRevoke` arm of
    /// [`Self::apply_control_op`]). They simply do not gate whether a recorded
    /// revocation applies.
    ///
    /// This is the *local* half of a larger bound: the register is per-replica,
    /// so a device that has not yet applied the `device_revoke` op has no row
    /// to read at all and will seal a new epoch to the revoked device. That is
    /// propagation, and no comparison could ever have closed it.
    fn is_revoked(
        &self,
        conn: &rusqlite::Connection,
        device_id: &[u8; 16],
    ) -> rusqlite::Result<bool> {
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM device_revocations WHERE device_id = ?",
                params![&device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Park an op whose Stream key has not arrived yet.
    ///
    /// Bounded by [`DEFERRED_PER_EPOCH_CAP`] and [`DEFERRED_TOTAL_CAP`]. This
    /// runs before anything about the payload has been checked — the key
    /// that would open it is precisely what is missing — so the only thing
    /// standing between a member and unbounded storage on every peer is the
    /// two caps. Overflow evicts oldest-first; see [`DEFERRED_TOTAL_CAP`] for
    /// why that direction and not the other.
    ///
    /// # Why `env.hlc` is deliberately not observed here
    ///
    /// [`Self::apply_remote_all`] absorbs a peer's stamp at its step (e), which
    /// is *after* the decrypt at step (d) — so an op that parks here returns
    /// before `self.hlc.observe(env.hlc)` and its stamp does not enter this
    /// device's clock until its key arrives and [`Self::drain_deferred`] retries
    /// it through the whole path. That omission is the intended behaviour, and
    /// there are three reasons it is (issue #157):
    ///
    /// 1. **It would not survive an open.** [`Self::prime_hlc`] restores the
    ///    clock from `ops`, and a parked op is in `deferred_ops`, which has no
    ///    stamp column and is not read at open. Observing it would put the
    ///    process above a reading the next restart cannot reproduce — an
    ///    invented, unrepeatable clock position rather than a restored one.
    /// 2. **A parked op is not necessarily an op at all.** It expires at
    ///    `DEFERRED_TTL_MS`, it is evicted when either cap overflows, and a
    ///    drained op that still will not apply is dropped. Absorbing the stamp
    ///    of ciphertext this replica may never open drags every subsequent LWW
    ///    comparison forward on the strength of a byte string it cannot read.
    /// 3. **The clock bounds what was applied, not what arrived.** That is
    ///    exactly what makes it usable: everything this device emits sorts above
    ///    everything it has *acted on*. An op it cannot decrypt is not one it
    ///    has acted on, and the retry re-runs the full path, so nothing is lost.
    ///
    /// What this costs is that between arrival and drain — unbounded, for a
    /// device offline across a rotation — this replica holds a stamp on disk
    /// that its clock does not reflect. Nothing reads [`HlcClock::peek`] for a
    /// decision today; revocation did for one revision and
    /// [`Self::is_revoked`] records why it stopped. Anything that starts
    /// comparing against the local reading again has to answer this window
    /// first.
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
                n_dropped = evicted,
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
    ///
    /// What that ordering costs is a gap with no transaction over it. The TTL
    /// sweep is one `with_tx`, the bucket delete is a second, and the
    /// [`Self::apply_remote_all`] loop runs outside both — so a crash after the
    /// delete and before the loop finishes loses the parked envelopes on this
    /// replica. Recovery is the one [`DEFERRED_TOTAL_CAP`] already relies on for
    /// an evicted op, and for the same reason: a parked op never reached `ops`
    /// and never advanced the sync cursor, so the relay still counts it as
    /// undelivered and re-sends it on the next reconnect — by which time the key
    /// that opens it is already here.
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
        // The age sweep runs on **every** absorbed key, not only on one that
        // releases something. It used to sit past the early return below, so it
        // ran only when this exact `(stream, epoch)` had rows parked — which is
        // precisely the bucket about to be emptied anyway. A row ages out
        // because nothing ever arrives for its bucket, so the one condition
        // that reached the sweep was the one condition under which there was
        // nothing to sweep. See [`DEFERRED_TTL_MS`] for why a row this old is
        // ciphertext nobody will ever open.
        let cutoff =
            i64::try_from(self.clock.now_ms().saturating_sub(DEFERRED_TTL_MS)).unwrap_or(i64::MAX);
        db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM deferred_ops WHERE received_at_ms < ?",
                params![cutoff],
            )?;
            Ok(())
        })?;
        if parked.is_empty() {
            return Ok(Vec::new());
        }
        db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM deferred_ops WHERE stream_id = ? AND epoch = ?",
                params![&stream_id[..], epoch],
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
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<Vec<([u8; 16], u32)>> {
        match inner {
            InnerOp::KeyEnvelope(p) => {
                // The epoch bound runs first, ahead of the recipient match,
                // because the third-party arm below writes a
                // `key_envelope_recipients` row and returns before any other
                // check in this arm can run. An epoch far above this replica's
                // live one is refused before it can be written anywhere.
                //
                // Two distinct harms, one bound. `MAX(epoch)` is what makes a
                // key live, so *absorbing* an absurd epoch strands `mint_epoch`
                // at the saturation point and redirects every op this device
                // seals afterwards to a key nobody else holds. And *recording*
                // an absurd epoch tells `backfill_key_envelopes` that a device
                // nobody has served already holds that epoch's key, so it emits
                // nothing and the device is left unable to read the stream. See
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
                        live_epoch = live,
                        "a key envelope names an epoch too far above this vault's own"
                    );
                    return Ok(Vec::new());
                }
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
                    // fans out to every device — and not opened. It *is*
                    // recorded: this is how a device learns that some other
                    // device has already sealed this epoch to that recipient,
                    // which is what stops every replica emitting the same
                    // backfill envelope.
                    //
                    // This is a claim the recorder cannot check: it holds no
                    // key to open the ciphertext with. A member emitting a
                    // `key_envelope` full of garbage addressed to a third
                    // device suppresses that device's backfill for that
                    // `(stream, epoch)` -- `backfill_key_envelopes` finds a
                    // row, emits nothing, and the device holds no key, so its
                    // ops park in `deferred_ops` until `DEFERRED_TTL_MS` drops
                    // them with nothing surfacing. The harm is a withhold, not
                    // a leak.
                    //
                    // What bounds it is the `MAX_EPOCH_LEAP` check above, which
                    // now runs before this row is written. Two bounds this
                    // comment used to claim were not bounds and are gone:
                    //
                    // * "the next rotation corrects it, because that is a new
                    //   epoch this table has no row for" -- false while any
                    //   epoch could be claimed. Rows filed for e+1, e+2, ...
                    //   poison rotations that have not happened yet, and the
                    //   correction never arrives.
                    // * "a member who can do this can read everything already"
                    //   -- false for exactly the member class revocation now
                    //   creates. A revoked device's reads are bounded by the
                    //   rotation and its writes are bounded by nothing (see
                    //   [`Self::apply_remote`] step 2), so it can file these
                    //   rows and cannot read what they withhold.
                    //
                    // The residual is a claim inside the leap window, which is
                    // corrected by the first rotation past `live +
                    // MAX_EPOCH_LEAP`. Closing it outright would need the
                    // recorder to verify a ciphertext it holds no key for.
                    Recipient::Device(other) => {
                        record_envelope_recipient(tx, &p.stream_id, p.epoch, &other, now_ms)?;
                        return Ok(Vec::new());
                    }
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
                let learned = self.keychain.absorb_stream_key(
                    tx,
                    &p.stream_id,
                    p.epoch,
                    &key,
                    KeySource::Envelope,
                    self.rng.as_ref(),
                    now_ms,
                )?;
                // Recorded *here*, and not where the recipient was matched.
                // Everything between the two is a way for this envelope to be
                // refused — an unopenable ciphertext, a `key_id` that does not
                // re-derive — and a row written before those runs would say
                // "this device has the key" about a key it declined. Nothing
                // re-sends against a row that is already there, so that mistake
                // is not self-correcting. (The epoch bound is the exception: it
                // runs at the top of this arm, because the third-party arm
                // records and returns before reaching here.)
                record_envelope_recipient(
                    tx,
                    &p.stream_id,
                    p.epoch,
                    &self.keychain.device_id(),
                    now_ms,
                )?;
                Ok(if learned {
                    vec![(p.stream_id, p.epoch)]
                } else {
                    Vec::new()
                })
            }
            InnerOp::DeviceRevoke(p) => {
                // The revocation row is an **LWW register** keyed on the op's
                // own `(hlc, device_id)` — the same rule ADR-0014 resolves
                // every other concurrent write in this engine with, rather than
                // a bespoke one for this family.
                //
                // The cut is that same HLC. There is no `effective_at` field to
                // pick a winner between, which is the point: see
                // [`DeviceRevokePayload`] for the two bounds that could not be
                // made to hold on an emitter-chosen one.
                //
                // Why LWW and not "earliest cut wins", which this arm briefly
                // did: `MIN` converges, but it is **irreversible**. A cut that
                // lands too far in the past — which a device with a slow clock
                // produced through the ordinary command, no crafted input
                // needed — could never be corrected, and it refuses its
                // target's entire history on every replica. Under LWW a later
                // revocation supersedes an earlier one, so a bad cut is fixed
                // by revoking again from a healthy device.
                //
                // `logical` is in the key because an HLC is `(physical,
                // logical)` and comparing physical alone would drop the half
                // that orders two ops inside one millisecond. `revoked_by`
                // breaks the remaining tie the way `LwwStamp` does, so
                // `revoke_reason` and `revoked_by` follow the winning op
                // instead of being order-dependent alongside a converged
                // timestamp.
                // A device cannot revoke itself, and cannot move its own
                // cut. Register hygiene independent of any gate: a device that
                // can rewrite its own row can push its cut forward and undo a
                // revocation somebody else made of it, which is the one edit
                // the register must never accept from the party it is about.
                // `Command::RevokeDevice` refuses it locally for the separate
                // reason that rotating every key away from the only device
                // holding them is not a recoverable state; this is the remote
                // half, and neither implies the other.
                if p.revoked_device_id == *sender {
                    tracing::warn!(
                        ev = "core.device.revoke_refused",
                        reason = "self",
                        sender_h = hex_short(sender),
                        "a device tried to move its own revocation cut"
                    );
                    return Ok(Vec::new());
                }
                let cut = i64::try_from(hlc.physical_ms).unwrap_or(i64::MAX);
                let logical = i64::from(hlc.logical);
                // Its own table, keyed on the revoked device id, so a
                // revocation naming a device this replica has never seen is
                // durable without inventing one. That case is ordinary: the
                // cert travels in the same stream with no ordering guarantee,
                // and one parked in `deferred_ops` at meta epoch k drains after
                // a revocation absorbed at k+1.
                //
                // Upserting into `devices` handled it and cost too much — every
                // such op minted a row there, so revocations of ids nobody
                // knows became phantom entries in the user's device list, and a
                // phantom row also satisfied `revoke_device`'s "is this device
                // known" guard, which would mint a fresh epoch for every stream
                // in the account on the way to revoking a ghost.
                tx.execute(
                    "INSERT INTO device_revocations
                     (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
                     VALUES (?4, ?1, ?2, ?3, ?5, ?6)
                     ON CONFLICT(device_id) DO UPDATE SET
                        cut_ms = ?1,
                        cut_logical = ?2,
                        revoked_by = ?3,
                        reason = ?5,
                        recorded_at_ms = ?6
                     WHERE (?1, ?2, ?3) > (cut_ms, cut_logical, revoked_by)",
                    params![
                        cut,
                        logical,
                        &sender[..],
                        &p.revoked_device_id[..],
                        p.reason_code.as_str(),
                        i64::try_from(now_ms).unwrap_or(i64::MAX),
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
                // A device id nobody has seen before, appearing in an
                // account that has revoked something, is the observable
                // signature of the one bypass revocation does not close: a
                // revoked device still holds `ID_S_priv`, so it can mint a
                // fresh id, sign a valid cert for it, and rejoin under a name
                // the register does not list
                // ([#105](https://github.com/justin13888/Sunrise/issues/105)).
                //
                // It is the signature of an ordinary pairing too, and nothing
                // here can tell the two apart — that is exactly what #105 is,
                // and ADR-0032 records why no check available today separates
                // them without either over-blocking honest devices or diverging
                // replicas. So this discloses and does not gate: the cert is
                // applied either way, every replica applies it, and the account
                // converges. What changes is that the event exists to be seen.
                let readmission = {
                    let known: bool = tx
                        .query_row(
                            "SELECT 1 FROM devices WHERE device_id = ?",
                            params![&cert.body.device_id[..]],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    let revocations: i64 =
                        tx.query_row("SELECT count(*) FROM device_revocations", [], |r| r.get(0))?;
                    !known && revocations > 0
                };
                if readmission {
                    tracing::warn!(
                        ev = "core.device.admitted_after_revocation",
                        sender_h = hex_short(sender),
                        subject_h = hex_short(&cert.body.device_id),
                        "a device id this vault has never seen joined an account that \
                         has revoked a device"
                    );
                }
                tx.execute(
                    "INSERT INTO devices
                     (device_id, cert_blob, nickname, platform, created_at_ms,
                      identity_id, d_d_pub)
                     VALUES (?, ?, ?, ?, ?, ?, ?)
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
                // The device is a member as of this line, so anything this
                // vault holds and it does not is now a gap it cannot close by
                // itself: `ID_D_priv` used to be its way out and is gone.
                //
                // Failing the whole delivery over a backfill would be wrong --
                // the cert is valid and belongs in `devices` whatever happens
                // next -- but swallowing the error would leave the gap open
                // with nothing said, which is the failure mode this arm's own
                // comment above warns about. So it is logged and the cert
                // stands. Nothing retries it: `publish_device_cert` is guarded
                // once per vault, so a replica applies a given device's cert
                // once and this runs once. What does recover the device is the
                // next rotation of the affected stream -- `emit_key_envelopes`
                // seals a fresh epoch to every unrevoked device -- so it regains
                // access to new content and not to the epoch it missed. Another
                // online replica's backfill covers it too, which is the main
                // reason every replica runs one rather than an elected leader.
                if let Err(e) = self.backfill_key_envelopes(
                    tx,
                    &cert.body.device_id,
                    &cert.body.d_d_pub,
                    now_ms,
                ) {
                    tracing::warn!(
                        ev = "core.device.backfill_failed",
                        reason = "storage",
                        subject_h = hex_short(&cert.body.device_id),
                        cause = %e,
                        "could not seal held stream keys to a newly certified device"
                    );
                }
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// The stored cert for `device_id`, revoked or not. `None` = a device this
    /// vault has never admitted.
    ///
    /// Membership is not consulted here and must not be: this answers "which
    /// key verifies this signature", which is a fact about the device and not
    /// about its standing. Filtering it on revocation once made revocation
    /// retroactive — with the row hidden, every op from a revoked device fell
    /// through to [`Self::self_authenticating_signer`], which knows only
    /// `DeviceCertPublish`, and came back `UnknownDevice` whatever its HLC
    /// said, including work the device did honestly months earlier.
    ///
    /// A revocation affects nothing **on this path**, and that is the whole
    /// point of the paragraph above: this answers "which key verifies this
    /// signature", which is a fact about the device rather than its standing.
    ///
    /// It does affect other paths. [`Self::emit_key_envelopes`] will not seal
    /// a new epoch to a revoked device and [`Self::backfill_key_envelopes`]
    /// will not hand one its keys back, which together are what stop it reading
    /// anything written after the cut.
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
            ensure_stream_row(tx, &stream, now_ms)?;
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
            ensure_stream_row(tx, &task_for_persist.stream_id, now_ms)?;
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
    /// [`Clock`] rather than from ambient process state.
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
            ensure_stream_row(tx, &stream, now_ms)?;
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
            ensure_stream_row(tx, &stream, now_ms)?;
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
            ensure_stream_row(tx, &routine.template.stream_id, now_ms)?;
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
            ensure_stream_row(tx, &start.stream_id, now_ms)?;
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
            "SELECT d.device_id, d.nickname, d.platform, r.device_id IS NOT NULL
             FROM devices d
             LEFT JOIN device_revocations r ON r.device_id = d.device_id",
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
}

/// What `EndFocus` derives when the session says the task was finished: the
/// completed Task, its encoded `TaskUpdate` op, and the streak advance the
/// completion triggers when the task is a routine occurrence.
///
/// Built and consumed entirely on the local command path, by
/// [`Engine::autocomplete_focused_task`] and its one caller; a replica applying
/// a `focus.end` op derives nothing, so the remote materialization never sees
/// one. This is the first module scope after the focus-session methods, which
/// is as close to them as a method's return type can live.
struct FocusCompletion {
    task: Task,
    inner: Vec<u8>,
    streak: Option<(Routine, Vec<u8>)>,
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
            ensure_stream_row(tx, &f.stream_id, ts_ms)?;
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
            (
                blob16(&raw),
                u32::try_from(open.max(0)).unwrap_or(u32::MAX),
                u32::try_from(unblocks.max(0)).unwrap_or(u32::MAX),
            )
        })
        .collect())
}

/// Widen a stored 16-byte id back into a typed [`EntityRef`].
fn ref_of(kind: EntityKind, raw: &[u8]) -> EntityRef {
    EntityRef::new(kind, blob16(raw))
}

/// Materialize a placeholder `streams` row for `stream` if none exists yet.
///
/// A no-op when the row is already there: the placeholder exists only so that
/// an op naming a stream this device never saw created has a row to hang off,
/// and it must never overwrite a row that already carries real data.
fn ensure_stream_row(
    tx: &Transaction<'_>,
    stream: &EntityRef,
    now_ms: u64,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = stream.bytes().to_vec();
    let exists: i64 = tx.query_row(
        "SELECT count(*) FROM streams WHERE stream_id = ?",
        params![&id_blob],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms)
         VALUES (?, ?, 0, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            vec![0u8; 32],
            None::<Vec<u8>>,
            0_i64,
            0_i64,
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
    /// history. Nothing filters on revocation anywhere on this path — not here
    /// and not in [`Self::lookup_device_cert`], whose own doc explains why
    /// membership is not a fact about which key verifies a signature.
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
/// `Task.blocks` is never written. It is derived from the `block_tasks` join
/// every time a Task is read, which is what makes the spec's "Bound Task's
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
            ensure_stream_row(tx, &block.stream_id, now_ms)?;
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
