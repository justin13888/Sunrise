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
//! calls it. Carved out so far: `context`, `focus`, `ids`, `lww`, `oplog`, `routine`, `stream`, `task`.

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

use crate::commands::{Command, CommandResult};
use crate::config::{Clock, HlcClock, Rng};
use crate::control_op::{DeviceRevokePayload, Recipient, RevokeReason};
use crate::events::DomainEvent;
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp, InnerOpError, OpEffect};
use crate::keychain::{EnvelopeRecipient, KeySource, Keychain};
use crate::queries::{ActionableTask, BlockRow, DeviceRow, Query, QueryResult};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, open_envelope_unverified, stream_key_id, verify_envelope, DeviceCert,
};
use sunrise_domain::time::SunriseTime;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    activity_for_entities, activity_table, build_daily_review, build_end_of_day_plan,
    build_morning_summary, build_weekly_review, effective_state, focus_table, fold_activity,
    fold_trends, imported_block_id, inbox_stream_ref, plan_reminders, routine_drift, streaks_table,
    trends_table, unblock_cascade, ActivityEvent, Attachment, AttachmentDraft, Block, BlockDraft,
    BlockPatch, DependencyGraph, ExportDataset, ExportFormat, OpPayload, OpRecord,
    ReminderCandidate, ReminderKind, ReminderSettings, ReviewSnapshot, ReviewSnapshotDraft,
    ReviewStream, ReviewWindow, RoutineDrift, StreakRow, Task, TaskState, Trends, WeekGrid,
    Weekday, WeeklyReview, WeeklyReviewInput, DEFAULT_DRIFT_THRESHOLD, DRIFT_WINDOW_WEEKS,
    TREND_WEEKS,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::{Db, OpLog};
use thiserror::Error;

mod context;
mod focus;
mod ids;
mod lww;
mod oplog;
mod routine;
mod stream;
mod task;
#[cfg(test)]
mod tests;

// `META_STREAM` and `hex_short` are named from outside `engine` — `core.rs`,
// `keychain.rs` and `sync_driver.rs` — so the path they name has to keep
// resolving across this split. `META_STREAM` stays defined below; `hex_short`
// moved to `ids` and is re-exported here at its old name.
use self::context::read_context;
pub(crate) use self::ids::hex_short;
use self::ids::{blob16, blob32, decode_unknowns, encode_unknowns, ms_to_ts, require_kind};
use self::lww::{materialize_remote, remap_legacy_inbox, LwwStamp};
use self::oplog::{record_envelope_recipient, remote_op_id, upsert_sync_cursor};
use self::routine::{read_routine, read_routines};
use self::stream::{ensure_stream_row, paused_streams, read_stream, stream_member_tasks};
use self::task::{actionable_scan, read_task, ref_of};

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

    // ---- query handlers ----

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
