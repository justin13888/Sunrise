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
//! calls it: `oplog` is everything that writes the log and the outbox, `lww`
//! the one merge rule and the dispatcher that applies it, `sync` the receive
//! half, `ids` the shared encode/decode primitives, and one module per entity
//! family — `task`, `stream`, `context`, `routine`, `focus`, `block`,
//! `attachment` — plus `review`, `notify` and the cross-entity reads in
//! `query`.
//!
//! What stays here is the seam: the error, the type, the caps several modules
//! read, the constructors, and the two dispatchers — [`Engine::apply`] and
//! [`Engine::query`] — which are the one place that has to name every other
//! module.

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
use crate::inner_op::InnerOpError;
use crate::keychain::Keychain;
use crate::queries::{Query, QueryResult};
use rusqlite::params;
use std::sync::Arc;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::decode_envelope;
use sunrise_domain::inbox_stream_ref;
use sunrise_storage::Db;
use thiserror::Error;

mod attachment;
mod block;
mod context;
mod focus;
mod ids;
mod lww;
mod notify;
mod oplog;
mod query;
mod review;
mod routine;
mod stream;
mod sync;
mod task;
#[cfg(test)]
mod tests;

// `META_STREAM` and `hex_short` are named from outside `engine` — `core.rs`,
// `keychain.rs` and `sync_driver.rs` — so the path they name has to keep
// resolving across this split. `META_STREAM` stays defined below; `hex_short`
// moved to `ids` and is re-exported here at its old name.
pub(crate) use self::ids::hex_short;
use self::lww::LwwStamp;

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
}
