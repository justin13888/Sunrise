//! The engine's unit tests.
//!
//! Kept as one module across the `engine/` split. The suite is grouped by
//! theme in reading order — tasks, contexts, streams, routines, blocks,
//! attachments, focus, sync/LWW, revocation, reviews — and every test in it
//! draws on the `testutil` helpers below, so moving it whole is what keeps the
//! test diff at zero. Redistributing it module by module is a later change.

use super::*;
use crate::config::SystemRng;
use crate::events::DomainEvent;
use parking_lot::Mutex as PLMutex;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::ActivityKind;
use sunrise_domain::EffectiveTaskState;
use sunrise_domain::{EndOfDayPlan, MorningSummary};
use sunrise_domain::{RRule, Routine, RoutineCatchupPolicy, RoutineDraft, TaskTemplate};
use sunrise_storage::Db;
use testutil::*;
// The engine's own imports, which `use super::*` used to supply from the
// single-file `engine.rs`, plus one glob per sibling module of the split.
use super::attachment::*;
use super::block::*;
use super::context::*;
use super::focus::*;
use super::identity::{
    SiblingRank, MAX_ROSTER_ENTRIES, MAX_SIBLINGS_PER_PREDECESSOR, MAX_SIBLING_CANDIDATES,
    SIBLING_ORDER_DESC,
};
use super::ids::*;
use super::lww::*;
use super::oplog::*;
use super::routine::*;
use super::stream::*;
use super::task::*;
use crate::commands::{Command, CommandResult, FocusStartDraft};
use crate::config::Clock;
use crate::control_op::{
    DeviceRevokePayload, IdentityTransitionPayload, KeyEnvelopePayload, KeyShare, Recipient,
    RevokeReason, RosterEntry,
};
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp};
use crate::keychain::{to16, KeySource, Keychain};
use crate::queries::{ActionableTask, BlockRow, ContextRow, FocusSessionRow, Query, QueryResult};
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;
use std::sync::Arc;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::DeviceCert;
use sunrise_crypto::{stream_key_id, StreamKey};
use sunrise_domain::sort_order;
use sunrise_domain::time::SunriseTime;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    imported_block_id, inbox_stream_ref, occurrence_key_at, occurrence_task_id, ActivityEvent,
    Attachment, AttachmentDraft, BlockDraft, BlockPatch, Chunk, Context, ContextDraft,
    ContextPatch, Energy, ExportDataset, ExportFormat, FocusEnd, FocusKind, FocusStart,
    InterruptionReason, NoteBody, ReminderSettings, ReviewSnapshotDraft, ReviewTotals,
    RoutinePatch, ScheduleConstraint, SessionLength, Stream, StreamColor, StreamDraft, StreamPatch,
    StreamReviewCadence, Task, TaskDraft, TaskPatch, TaskState, Trends, ValidationError,
    WeeklyReview, INBOX_STREAM_BYTES, POMODORO_MS,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::{OpLog, Outbox};

/// Shared test support for every test in this module.
///
/// Extracted so that a `mod` boundary can be drawn anywhere in the tests
/// below it: a helper physically interleaved with the tests lands inside
/// whichever nested module the split puts it in, and a sibling module
/// cannot reach another sibling's private items. Everything here is
/// `pub(super)`, so it stays reachable from `tests` and from any module
/// nested inside it.
mod testutil {
    use super::*;

    #[derive(Debug)]
    pub(super) struct FakeClock(pub(super) PLMutex<u64>);

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock()
        }
    }

    pub(super) fn engine() -> Engine {
        let keychain = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
        Engine::from_clock(
            Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
            Arc::new(SystemRng),
            keychain,
        )
    }

    pub(super) fn db() -> Db {
        Db::open_memory(&VaultRootKey::from_bytes([0xab; 32])).unwrap()
    }

    pub(super) fn context_rows(e: &Engine, db: &Db) -> Vec<ContextRow> {
        match e.query(db, Query::Contexts).unwrap() {
            QueryResult::Contexts(rows) => rows,
            other => panic!("expected Contexts, got {other:?}"),
        }
    }

    pub(super) fn new_context(e: &Engine, db: &mut Db, name: &str) -> EntityRef {
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

    pub(super) fn task_context_ids(db: &Db, task: EntityRef) -> BTreeSet<[u8; 16]> {
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

    /// The externally-tagged [`InnerOp`] variant name carried by `op_id`'s
    /// sealed envelope.
    pub(super) fn inner_op_variant(e: &Engine, db: &Db, op_id: &[u8; 16]) -> String {
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

    // Fetch the most-recently-created task id in a stream (highest rowid
    // proxy via id ordering is unstable, so use created ordering by title
    // being unavailable — instead read the last inserted via the ops table
    // is overkill; query tasks table directly).
    pub(super) fn db_last_task(db: &Db, stream: EntityRef) -> EntityRef {
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

    pub(super) fn sample_constraint() -> ScheduleConstraint {
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

    pub(super) const NOW: i64 = 1_700_000_000_000;

    pub(super) const DAY_MS: i64 = 86_400_000;

    pub(super) fn stream_ref(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [b; 16])
    }

    pub(super) fn routine_draft(
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

    pub(super) fn op_count(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap()
    }

    pub(super) fn live_task_ids(db: &Db) -> BTreeSet<[u8; 16]> {
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

    pub(super) fn past_task_count(db: &Db, rid: EntityRef) -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM tasks
                     WHERE routine_id = ? AND deleted = 0 AND scheduled_at_ms <= ?",
                params![rid.bytes().to_vec(), NOW],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A block on 2026-03-04, `hour..hour + len` floating (no zone), which is
    /// what a calendar grid draws when the user has not pinned a zone.
    pub(super) fn block_at(hour: i8, len: i8) -> (SunriseTime, SunriseTime) {
        (
            SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(hour, 0, 0, 0)),
            SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(hour + len, 0, 0, 0)),
        )
    }

    pub(super) fn block_draft(hour: i8, len: i8, title: Option<&str>) -> BlockDraft {
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

    pub(super) fn day_rows(e: &Engine, db: &Db, at_ms: u64) -> Vec<BlockRow> {
        match e.query(db, Query::DayBlocks { day_ms: at_ms }).unwrap() {
            QueryResult::Blocks(rows) => rows,
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    /// The instant a floating civil time on the test date resolves to under the
    /// engine's device zone (UTC in tests).
    pub(super) fn day_ms() -> u64 {
        SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(12, 0, 0, 0)).index_ms() as u64
    }

    pub(super) fn import(source: &str, uid: &str, draft: BlockDraft) -> Command {
        Command::ImportBlock {
            source: source.into(),
            uid: uid.into(),
            draft,
        }
    }

    pub(super) fn attachment_draft(parent: EntityRef) -> AttachmentDraft {
        AttachmentDraft {
            parent,
            filename: "receipt.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 4096,
            blob_key: [7u8; 32],
            blob_id: [9u8; 16],
            chunk_count: 2,
            content_hash: [11u8; 32],
            ciphertext_hash: [13u8; 32],
        }
    }

    pub(super) fn attachments_of(e: &Engine, db: &Db, task: EntityRef) -> Vec<Attachment> {
        match e.query(db, Query::TaskAttachments(task)).unwrap() {
            QueryResult::Attachments(a) => a,
            other => panic!("expected attachments, got {other:?}"),
        }
    }

    pub(super) fn open_session(e: &Engine, db: &mut Db, task: EntityRef) -> EntityRef {
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

    pub(super) fn end_session(
        e: &Engine,
        db: &mut Db,
        session: EntityRef,
        done: bool,
    ) -> CommandResult {
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

    pub(super) fn task_of(e: &Engine, db: &Db, id: EntityRef) -> Task {
        match e.query(db, Query::EntityById(id)).unwrap() {
            QueryResult::Task(t) => *t,
            other => panic!("expected task, got {other:?}"),
        }
    }

    /// 2026-03-04 12:00 UTC, and the civil midnights around it.
    pub(super) const NOTIFY_NOON: i64 = 1_772_625_600_000;

    pub(super) fn engine_at(now_ms: i64) -> (Engine, Db) {
        let e = engine_seeded(
            ROOT,
            [1u8; 32],
            Arc::new(FakeClock(PLMutex::new(now_ms as u64))),
        );
        (e, db_root(ROOT))
    }

    pub(super) fn morning(e: &Engine, db: &Db, now_ms: i64) -> MorningSummary {
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

    pub(super) fn evening(e: &Engine, db: &Db, now_ms: i64) -> EndOfDayPlan {
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

    pub(super) fn reminders(
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

    pub(super) fn db_root(root: [u8; 32]) -> Db {
        Db::open_memory(&VaultRootKey::from_bytes(root)).unwrap()
    }

    pub(super) fn engine_seeded(root: [u8; 32], seed: [u8; 32], clock: Arc<FakeClock>) -> Engine {
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
    pub(super) fn engine_random_keys(
        root: [u8; 32],
        seed: [u8; 32],
        clock: Arc<FakeClock>,
    ) -> Engine {
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
    pub(super) fn hand_over_key(
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
    pub(super) fn key_envelope_envs(db: &Db) -> Vec<Vec<u8>> {
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
        ids.iter()
            .map(|id| env_bytes(db, &to16(id).expect("op_id is 16 bytes")))
            .collect()
    }

    pub(super) fn deferred_rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM deferred_ops", [], |r| r.get(0))
            .unwrap()
    }

    /// Apply a `device_revoke` for `target` declared at `hlc_ms`, through the
    /// same code path the op takes when it arrives over sync.
    ///
    /// The cut *is* that HLC: there is no `effective_at` to pass, which is the
    /// whole of decision 19.
    pub(super) fn revoke(
        receiver: &Engine,
        db: &mut Db,
        sender: &Engine,
        target: [u8; 16],
        hlc_ms: u64,
    ) {
        revoke_at(receiver, db, sender, target, Hlc::at(hlc_ms));
    }

    /// [`revoke`] with the declaring op's full HLC, logical half included, so a
    /// test can put two revocations inside one millisecond.
    pub(super) fn revoke_at(
        receiver: &Engine,
        db: &mut Db,
        sender: &Engine,
        target: [u8; 16],
        hlc: Hlc,
    ) {
        let sender_id = sender.keychain.device_id();
        let inner = InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: target,
            reason_code: RevokeReason::Lost,
        });
        db.with_tx(|tx| {
            receiver
                .apply_control_op(tx, &inner, &sender_id, hlc, hlc.physical_ms, 1)
                .map(|_| ())
        })
        .unwrap();
    }

    pub(super) fn set_clock(c: &FakeClock, v: u64) {
        *c.0.lock() = v;
    }

    pub(super) fn env_bytes(db: &Db, op_id: &[u8; 16]) -> Vec<u8> {
        OpLog::get_envelope(db, op_id).unwrap().unwrap()
    }

    /// Sealed envelope of the most recent `task.create` op targeting `target`.
    pub(super) fn create_env_for(db: &Db, target: &[u8; 16]) -> Vec<u8> {
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

    pub(super) fn read_task_t(e: &Engine, db: &Db, id: EntityRef) -> Task {
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
    pub(super) fn trust(receiver: &Engine, db: &mut Db, sender: &Engine) {
        let cert = sender.keychain.cert_blob();
        let sender_id = sender.keychain.device_id();
        db.with_tx(|tx| {
            receiver
                .apply_control_op(
                    tx,
                    &InnerOp::DeviceCertPublish(cert),
                    &sender_id,
                    Hlc::at(0),
                    0,
                    1,
                )
                .map(|_| ())
        })
        .unwrap();
    }

    /// [`trust`] at a chosen wall clock, for tests that care when the cert
    /// landed relative to a revocation cut.
    pub(super) fn trust_at(receiver: &Engine, db: &mut Db, sender: &Engine, now_ms: u64) {
        let cert = sender.keychain.cert_blob();
        let sender_id = sender.keychain.device_id();
        db.with_tx(|tx| {
            receiver
                .apply_control_op(
                    tx,
                    &InnerOp::DeviceCertPublish(cert),
                    &sender_id,
                    Hlc::at(now_ms),
                    now_ms,
                    1,
                )
                .map(|_| ())
        })
        .unwrap();
    }

    /// The `(stream, epoch)` pairs `db`'s owner has sealed to `recipient`.
    pub(super) fn envelopes_to(
        engine: &Engine,
        db: &Db,
        recipient: &[u8; 16],
    ) -> Vec<([u8; 16], u32)> {
        key_envelope_envs(db)
            .into_iter()
            .filter_map(|env| engine.keychain.open_op(&env).ok())
            .filter_map(|cbor| decode_inner_op(&cbor).ok())
            .filter_map(|inner| match inner {
                InnerOp::KeyEnvelope(p) if p.recipient == Recipient::Device(*recipient) => {
                    Some((p.stream_id, p.epoch))
                }
                _ => None,
            })
            .collect()
    }

    /// Every `seq` `device` has written to the vault-meta stream, ascending.
    ///
    /// Reads the `ops` columns rather than the sealed envelopes because the
    /// point is what the UNIQUE constraint sees.
    pub(super) fn meta_seqs_of(db: &Db, device: &[u8; 16]) -> Vec<u64> {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT seq FROM ops
                     WHERE stream_id = ? AND device_id = ?
                     ORDER BY seq ASC",
            )
            .unwrap();
        stmt.query_map(params![&META_STREAM[..], &device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<u64>>>()
            .unwrap()
    }

    /// A stamp for a row seeded straight into the DB with no op behind it.
    ///
    /// Zeroed device and `seq` 0: it loses every LWW comparison, which is the
    /// right answer for a row no device ever claimed to have written.
    pub(super) fn seeded_stamp() -> LwwStamp {
        LwwStamp {
            hlc: Hlc {
                physical_ms: T0,
                logical: 0,
            },
            device: [0u8; 16],
            seq: 0,
        }
    }

    /// Materialize a Stream row with **no op**, so a later command against it
    /// can still be the vault-meta stream's first writer.
    ///
    /// Every command emits an op, and the first op of any kind mints the
    /// vault-meta key — including a Task's, because minting the Inbox key
    /// emits `key_envelope` control ops into vault-meta, which mints that too.
    /// So an `UpdateStream` test that created its Stream with `CreateStream`
    /// would be testing a vault that already had the key, which is precisely
    /// the case the bug cannot reach. Seeding the row directly is what keeps
    /// each case's own command the one that mints.
    pub(super) fn seed_stream(db: &mut Db, name: &str) -> EntityRef {
        let id = EntityRef::new(EntityKind::Stream, [0x11; 16]);
        let stream = Stream {
            id,
            created_at: ms_to_ts(T0 as i64),
            updated_at: ms_to_ts(T0 as i64),
            name: name.to_string(),
            description: None,
            color: StreamColor::Slate,
            icon: None,
            parent_id: None,
            sort_order: sort_order::append_after(None),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: StreamReviewCadence::Weekly,
            default_context: None,
            reminder_lead_s: None,
            deleted: false,
            unknown: Unknowns::new(),
        };
        db.with_tx(|tx| insert_stream_row(tx, &stream, &seeded_stamp()))
            .unwrap();
        id
    }

    /// A Context row with no op behind it. See [`seed_stream`].
    pub(super) fn seed_context(db: &mut Db, name: &str) -> EntityRef {
        let id = EntityRef::new(EntityKind::Context, [0x22; 16]);
        let ctx = Context {
            id,
            created_at: ms_to_ts(T0 as i64),
            updated_at: ms_to_ts(T0 as i64),
            name: name.to_string(),
            description: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };
        db.with_tx(|tx| insert_context_row(tx, &ctx, &seeded_stamp()))
            .unwrap();
        id
    }

    /// A Routine row with no op behind it. See [`seed_stream`].
    pub(super) fn seed_bare_routine(db: &mut Db) -> EntityRef {
        let id = EntityRef::new(EntityKind::Routine, [0x33; 16]);
        let d = routine_draft(
            inbox_stream_ref(),
            "FREQ=DAILY",
            T0 as i64,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let routine = Routine {
            id,
            created_at: ms_to_ts(T0 as i64),
            updated_at: ms_to_ts(T0 as i64),
            template: d.template,
            rrule: d.rrule,
            timezone: d.timezone,
            starts_at: d.starts_at,
            ends_at: None,
            scheduling_constraints: Vec::new(),
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
        db.with_tx(|tx| {
            // The routine's template Stream has to exist for the foreign key,
            // and `ensure_stream_row` materializes one without emitting an op
            // — which is exactly the property this helper needs.
            ensure_stream_row(tx, &routine.template.stream_id, T0)?;
            insert_routine_row(tx, &routine, T0, &seeded_stamp())
        })
        .unwrap();
        id
    }

    pub(super) const ROOT: [u8; 32] = [0x5a; 32];

    pub(super) const T0: u64 = 1_700_000_000_000;

    /// Sealed envelope of the most recent op of `kind` targeting `target`.
    pub(super) fn env_for_kind(db: &Db, target: &[u8; 16], kind: &str) -> Vec<u8> {
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

    /// Every `task.update` envelope this vault holds for `target`, oldest
    /// first.
    pub(super) fn update_envs_for(db: &Db, target: &[u8; 16]) -> Vec<Vec<u8>> {
        let ids: Vec<[u8; 16]> = db
            .conn()
            .prepare(
                "SELECT op_id FROM ops
                     WHERE target_id = ? AND inner_kind = 'task.update'
                     ORDER BY rowid",
            )
            .unwrap()
            .query_map(params![&target[..]], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|v| {
                let v = v.unwrap();
                let mut a = [0u8; 16];
                a.copy_from_slice(&v[..16]);
                a
            })
            .collect();
        ids.iter().map(|id| env_bytes(db, id)).collect()
    }

    /// The HLC stamp on a materialized task row.
    pub(super) fn task_stamp(db: &Db, target: &[u8; 16]) -> Hlc {
        db.conn()
            .query_row(
                "SELECT lww_hlc_ms, lww_hlc_logical FROM tasks WHERE id = ?",
                params![&target[..]],
                |r| {
                    Ok(Hlc {
                        physical_ms: u64::try_from(r.get::<_, i64>(0)?).unwrap(),
                        logical: u32::try_from(r.get::<_, i64>(1)?).unwrap(),
                    })
                },
            )
            .unwrap()
    }

    pub(super) fn task_title(db: &Db, target: &[u8; 16]) -> String {
        db.conn()
            .query_row(
                "SELECT title FROM tasks WHERE id = ?",
                params![&target[..]],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The whole revocation register a replica holds for `device`, or `None` if
    /// it holds none.
    ///
    /// All four columns, because convergence on the timestamp alone would still
    /// leave two replicas disagreeing about who did it and why.
    pub(super) type RevocationRow = (i64, i64, Vec<u8>, String);

    /// Whether `device` is in `device_read_bounds`, and when it was first put
    /// there.
    ///
    /// The other revocation table, and the one the four key-distribution sites
    /// read. Distinct from [`revocation_row`] on purpose: the two disagreeing
    /// is what migration 0028 exists for, and a test that asked only one of
    /// them could not see the disagreement.
    pub(super) fn read_bound_row(db: &Db, device: &[u8; 16]) -> Option<i64> {
        db.conn()
            .query_row(
                "SELECT first_bound_at_ms FROM device_read_bounds WHERE device_id = ?",
                params![&device[..]],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    /// Every `(stream_id, epoch)` this vault holds a key for.
    ///
    /// `stream_keys` is where a mint lands, so this is how a test asks whether
    /// a command rotated anything at all.
    pub(super) fn held_epochs(db: &Db) -> Vec<(Vec<u8>, i64)> {
        let mut stmt = db
            .conn()
            .prepare("SELECT stream_id, epoch FROM stream_keys ORDER BY stream_id, epoch")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    }

    pub(super) fn revocation_row(db: &Db, device: &[u8; 16]) -> Option<RevocationRow> {
        db.conn()
            .query_row(
                "SELECT cut_ms, cut_logical, revoked_by, reason
                     FROM device_revocations WHERE device_id = ?",
                params![&device[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .unwrap()
    }

    /// How many `device_revoke` ops this replica has kept, believed or not.
    ///
    /// The register is a fold over these, so "stored and skipped" and "never
    /// arrived" look identical in `device_revocations` and are told apart only
    /// here.
    /// The `ev` names of every event emitted while `f` runs, in order.
    ///
    /// Hand-rolled rather than borrowed from `sunrise-log`'s capture target:
    /// this crate depends on `tracing` and not on `tracing-subscriber`, and a
    /// dev-dependency on either to read one field would be a workspace change
    /// for a test. Only the `ev` field is kept, which is the only part of a
    /// log line this repository treats as a contract —
    /// `crates/sunrise-log/tests/event_catalog.rs` is the gate on that name.
    ///
    /// The inert dispatcher is not optional and not tidiness. `tracing` caches
    /// an `Interest` per callsite, and while exactly one `Dispatch` is
    /// registered process-wide the fast path computes it from *the registering
    /// thread's* default subscriber, which in a test binary is whichever
    /// neighbour reached the callsite first and usually has none. The interest
    /// is then cached as `never`, the macro is skipped, and the capture sees
    /// nothing at all. A second dispatcher that is never dropped keeps the
    /// count above one so every rebuild reads the registry instead. The same
    /// defect and the same remedy are documented at length on
    /// `sunrise-log`'s `pin_interest_cache`.
    pub(super) fn events_emitted_by(f: impl FnOnce()) -> Vec<String> {
        use std::sync::OnceLock;
        use tracing::field::{Field, Visit};

        /// Registered once and never dropped; see above.
        static INTEREST_PIN: OnceLock<tracing::Dispatch> = OnceLock::new();

        struct Inert;
        impl tracing::Subscriber for Inert {
            fn enabled(&self, _m: &tracing::Metadata<'_>) -> bool {
                false
            }
            fn new_span(&self, _s: &tracing::span::Attributes<'_>) -> tracing::Id {
                tracing::Id::from_u64(1)
            }
            fn record(&self, _s: &tracing::Id, _v: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _s: &tracing::Id, _f: &tracing::Id) {}
            fn event(&self, _e: &tracing::Event<'_>) {}
            fn enter(&self, _s: &tracing::Id) {}
            fn exit(&self, _s: &tracing::Id) {}
        }

        struct EvVisitor(Option<String>);
        impl Visit for EvVisitor {
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "ev" {
                    self.0 = Some(value.to_owned());
                }
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "ev" && self.0.is_none() {
                    self.0 = Some(format!("{value:?}").trim_matches('"').to_owned());
                }
            }
        }

        struct EvCapture(Arc<PLMutex<Vec<String>>>);
        impl tracing::Subscriber for EvCapture {
            fn enabled(&self, _m: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _s: &tracing::span::Attributes<'_>) -> tracing::Id {
                tracing::Id::from_u64(1)
            }
            fn record(&self, _s: &tracing::Id, _v: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _s: &tracing::Id, _f: &tracing::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                let mut visitor = EvVisitor(None);
                event.record(&mut visitor);
                if let Some(ev) = visitor.0 {
                    self.0.lock().push(ev);
                }
            }
            fn enter(&self, _s: &tracing::Id) {}
            fn exit(&self, _s: &tracing::Id) {}
        }

        INTEREST_PIN.get_or_init(|| tracing::Dispatch::new(Inert));
        let seen = Arc::new(PLMutex::new(Vec::new()));
        let dispatch = tracing::Dispatch::new(EvCapture(Arc::clone(&seen)));
        tracing::dispatcher::with_default(&dispatch, f);
        let taken = seen.lock().clone();
        taken
    }

    pub(super) fn ledger_rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM device_revoke_ops", [], |r| r.get(0))
            .unwrap()
    }

    /// The queued vault device ids, oldest first.
    pub(super) fn pending_relay_revocations(db: &Db) -> Vec<[u8; 16]> {
        let conn = db.conn();
        let mut stmt = conn
            .prepare("SELECT device_id FROM relay_revocation_intents ORDER BY created_at_ms ASC")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect()
    }

    pub(super) fn device_rows(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap()
    }

    /// Read the stored cursor for `(stream, device)`, or 0 if none.
    pub(super) fn cursor_for(db: &Db, stream: &[u8; 16], device: &[u8; 16]) -> u64 {
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

    /// The stored cursor row for `(stream, device)`, `None` when there is no
    /// row at all.
    ///
    /// [`cursor_for`] reports 0 for both, which is the right answer for a
    /// reader asking "how far have I got" and the wrong one for a test whose
    /// subject is whether a row was written. A cursor of 0 is a real value —
    /// `ops_run_end`'s `ELSE ?3 - 1` arm writes it whenever seq 1 is missing.
    pub(super) fn cursor_row(db: &Db, stream: &[u8; 16], device: &[u8; 16]) -> Option<i64> {
        db.conn()
            .query_row(
                "SELECT last_applied_seq FROM sync_cursors
                     WHERE stream_id = ? AND device_id = ?",
                params![&stream[..], &device[..]],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .unwrap()
    }

    /// How many rows the outbox holds, acked or not.
    pub(super) fn outbox_rows(db: &Db) -> i64 {
        outbox_count(db.conn())
    }

    /// [`outbox_rows`] against an open transaction.
    ///
    /// A test whose subject is what a *failing* transaction left behind has to
    /// read before the rollback, and `with_tx` rolls back on the error it is
    /// about to be handed.
    pub(super) fn outbox_count(conn: &rusqlite::Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM outbox", [], |row| row.get(0))
            .unwrap()
    }

    /// How many op rows sit at `(stream, device, seq)` — 0 or 1, since `ops`
    /// has a `UNIQUE` over the three.
    pub(super) fn ops_at(
        conn: &rusqlite::Connection,
        stream: &[u8; 16],
        device: &[u8; 16],
        seq: u64,
    ) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM ops
                 WHERE stream_id = ? AND device_id = ? AND seq = ?",
            params![&stream[..], &device[..], seq],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// The stored cursor for `(stream, device)` read through an open
    /// transaction, for the same reason [`outbox_count`] exists.
    pub(super) fn cursor_in_tx(
        conn: &rusqlite::Connection,
        stream: &[u8; 16],
        device: &[u8; 16],
    ) -> i64 {
        conn.query_row(
            "SELECT last_applied_seq FROM sync_cursors
                 WHERE stream_id = ? AND device_id = ?",
            params![&stream[..], &device[..]],
            |row| row.get(0),
        )
        .unwrap()
    }

    pub(super) fn stamp(hlc_ms: u64, logical: u32, device: [u8; 16], seq: u64) -> LwwStamp {
        LwwStamp {
            hlc: Hlc {
                physical_ms: hlc_ms,
                logical,
            },
            device,
            seq,
        }
    }

    pub(super) fn row_of(s: &LwwStamp) -> RowLww {
        RowLww {
            hlc: s.hlc,
            device: Some(s.device.to_vec()),
            seq: s.seq,
        }
    }

    /// Stamps drawn from deliberately small domains, so that ties on each
    /// term are common. The tie-breaks are where a comparator stops being
    /// an order, so a strategy that rarely collides would pass whatever it
    /// was given. One arm in sixteen draws a genuinely random device id,
    /// which is what exercises the 16-byte memcmp itself.
    pub(super) fn arb_stamp() -> impl proptest::strategy::Strategy<Value = LwwStamp> {
        use proptest::prelude::Strategy;
        let device = proptest::prop_oneof![
            15 => (0u8..3u8).prop_map(|b| [b; 16]),
            1 => proptest::prelude::any::<[u8; 16]>(),
        ];
        (0u64..4, 0u32..3, device, 0u64..4)
            .prop_map(|(ms, logical, device, seq)| stamp(ms, logical, device, seq))
    }

    /// `(stream_id, device_id, seq)` triples, again from small domains: an
    /// injectivity property only means something if the inputs collide.
    pub(super) fn arb_op_id_triple(
    ) -> impl proptest::strategy::Strategy<Value = ([u8; 16], [u8; 16], u64)> {
        use proptest::prelude::Strategy;
        let id = proptest::prop_oneof![
            15 => (0u8..3u8).prop_map(|b| [b; 16]),
            1 => proptest::prelude::any::<[u8; 16]>(),
        ];
        (id.clone(), id, 0u64..4)
    }

    /// How many ops the cursor property delivers.
    pub(super) const DELIVERY_OPS: usize = 6;

    /// A delivery schedule: an arbitrary noisy run over `0..DELIVERY_OPS`
    /// — duplicates, repeats and holes at any position — followed by a
    /// shuffled permutation of all of them, so every op does eventually
    /// arrive and the cursor has to end at `DELIVERY_OPS`.
    pub(super) fn delivery_schedule() -> impl proptest::strategy::Strategy<Value = Vec<usize>> {
        use proptest::prelude::Strategy;
        (
            proptest::collection::vec(0..DELIVERY_OPS, 0..=2 * DELIVERY_OPS),
            proptest::prelude::Just((0..DELIVERY_OPS).collect::<Vec<usize>>()).prop_shuffle(),
        )
            .prop_map(|(noisy, tail)| noisy.into_iter().chain(tail).collect())
    }

    /// Build the `unknown` map a newer schema would have written.
    pub(super) fn future_fields() -> sunrise_domain::Unknowns {
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

    /// A clock pinned to a named zone, so a test can move a device between
    /// timezones the way a plane does.
    #[derive(Debug)]
    pub(super) struct ZonedClock(pub(super) u64, pub(super) &'static str);

    impl Clock for ZonedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
        fn timezone(&self) -> String {
            self.1.to_string()
        }
    }

    /// One canonical task row: (id, title, state, scheduled_ms, due_ms,
    /// stream_id, deleted, deferred_count, completed_ms).
    pub(super) type TaskProjRow = (
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
    pub(super) fn tasks_projection(db: &Db) -> Vec<TaskProjRow> {
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

    pub(super) fn all_envelopes(db: &Db) -> Vec<Vec<u8>> {
        let mut stmt = db
            .conn()
            .prepare("SELECT envelope FROM ops ORDER BY rowid ASC")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// Every open task with its derived dependency counts, ranked.
    pub(super) fn actionable_rows(e: &Engine, db: &Db) -> Vec<ActionableTask> {
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

    pub(super) fn row_for(rows: &[ActionableTask], id: EntityRef) -> ActionableTask {
        rows.iter()
            .find(|r| r.task.id == id)
            .unwrap_or_else(|| panic!("no actionable row for {id}"))
            .clone()
    }

    pub(super) fn new_task(e: &Engine, db: &mut Db, title: &str) -> EntityRef {
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

    pub(super) fn set_blockers(
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

    /// NOW is 2023-11-14T22:13:20Z — a Tuesday evening, outside a 09:00–17:00
    /// window and inside it ten hours earlier (12:13Z).
    pub(super) const OUTSIDE_WINDOW_MS: i64 = NOW;

    pub(super) const INSIDE_WINDOW_MS: i64 = NOW - 10 * 3_600_000;

    pub(super) fn soft_9_to_5() -> ScheduleConstraint {
        ScheduleConstraint {
            severity: sunrise_domain::ConstraintSeverity::Soft,
            ..sample_constraint()
        }
    }

    pub(super) fn engine_clocked(clock: Arc<FakeClock>) -> Engine {
        let keychain = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
        Engine::from_clock(clock, Arc::new(SystemRng), keychain)
    }

    pub(super) fn routine_of(e: &Engine, db: &Db, rid: EntityRef) -> Routine {
        match e.query(db, Query::EntityById(rid)).unwrap() {
            QueryResult::Routine(r) => *r,
            _ => panic!("expected Routine"),
        }
    }

    /// The first `n` occurrence task ids of `rid`, with their instants.
    pub(super) fn occurrence_tasks(routine: &Routine, n: usize) -> Vec<(EntityRef, i64)> {
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

    pub(super) fn seed_routine(e: &Engine, db: &mut Db) -> (EntityRef, Vec<(EntityRef, i64)>) {
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

    pub(super) fn routine_op_count(db: &Db) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM ops WHERE inner_kind = 'routine.update'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The stored LWW stamp on a row, as the four columns hold it.
    pub(super) fn row_stamp(
        db: &Db,
        table: &str,
        id_col: &str,
        id: &EntityRef,
    ) -> (i64, i64, i64, Vec<u8>) {
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

    pub(super) fn blocker_edges(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut stmt = db
            .conn()
            .prepare("SELECT task_id, blocker_id FROM task_blockers ORDER BY task_id, blocker_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    pub(super) fn focus_engine(now_ms: u64) -> (Engine, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock(PLMutex::new(now_ms)));
        let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
        (
            Engine::from_clock(clock.clone(), Arc::new(SystemRng), kc),
            clock,
        )
    }

    pub(super) fn task_with(e: &Engine, db: &mut Db, title: &str, d: TaskDraft) -> EntityRef {
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

    pub(super) fn start_work(
        e: &Engine,
        db: &mut Db,
        task: EntityRef,
        len: SessionLength,
    ) -> EntityRef {
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

    pub(super) fn running(e: &Engine, db: &Db) -> Vec<FocusSessionRow> {
        match e.query(db, Query::RunningFocusSessions).unwrap() {
            QueryResult::FocusSessions(v) => v,
            other => panic!("expected FocusSessions, got {other:?}"),
        }
    }

    pub(super) fn sessions_for(e: &Engine, db: &Db, task: EntityRef) -> Vec<FocusSessionRow> {
        match e
            .query(db, Query::TaskFocusSessions { task, limit: 50 })
            .unwrap()
        {
            QueryResult::FocusSessions(v) => v,
            other => panic!("expected FocusSessions, got {other:?}"),
        }
    }

    pub(super) fn stats(e: &Engine, db: &Db, now_ms: u64) -> sunrise_domain::FocusStats {
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

    pub(super) fn plan(
        e: &Engine,
        db: &Db,
        energy: Option<Energy>,
    ) -> Vec<crate::queries::FocusPlanRow> {
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

    /// Monday 2026-01-05T00:00:00Z, so every week boundary below is hand-checkable.
    pub(super) const REVIEW_MON: u64 = 1_767_571_200_000;

    pub(super) const REVIEW_DAY: u64 = 24 * 60 * 60 * 1000;

    pub(super) const REVIEW_WEEK: u64 = 7 * REVIEW_DAY;

    /// An engine whose clock the test drives, plus its DB.
    pub(super) fn review_fixture() -> (Engine, Db, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock(PLMutex::new(REVIEW_MON)));
        let e = engine_seeded([0xab; 32], [0x11; 32], clock.clone());
        (e, db(), clock)
    }

    pub(super) fn review_task(
        e: &Engine,
        db: &mut Db,
        title: &str,
        stream: Option<EntityRef>,
    ) -> EntityRef {
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

    pub(super) fn weekly(e: &Engine, db: &Db, now_ms: u64) -> WeeklyReview {
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

    pub(super) fn timeline(e: &Engine, db: &Db, entity: EntityRef) -> Vec<ActivityEvent> {
        match e
            .query(db, Query::ActivityTimeline { entity, limit: 100 })
            .unwrap()
        {
            QueryResult::Activity(v) => v,
            other => panic!("expected Activity, got {other:?}"),
        }
    }

    /// The whole-vault + per-Stream trend, as the caller sees it.
    pub(super) fn trends_of(e: &Engine, db: &Db, now: u64) -> Trends {
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

/// ADR-0014's full-state-delete rule, for the entity the defect actually
/// occurred on.
///
/// The rule is asserted deterministically for Block, Context and
/// Attachment. It was not for Task: all seven `DeleteTask` tests are
/// local-only, none calls `apply_remote`, so nothing raced a Task delete
/// against a remote update. The only guard was
/// `crates/sunrise-e2e/tests/two_core_delete_convergence.rs`, whose own
/// docstring records that the divergence "failed roughly one run in four"
/// and was "stable, not transient" — invisible in the UI, because both
/// replicas showed a plausible task.
///
/// Under an id-only delete there is no state to contribute, so the losing
/// update's title survives on the replica that applied it and not on the
/// other. The two arrival orders are checked at once: B applies A's delete
/// after its own rename, and A applies B's rename after its own delete.
#[test]
fn a_task_delete_that_wins_lww_replaces_the_whole_row() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "contested".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.create"))
        .unwrap();

    // B renames it...
    set_clock(&cb, T0 + 1_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("renamed".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // ...and A deletes it strictly later, so the delete wins on both sides.
    set_clock(&ca, T0 + 2_000);
    ea.apply(&mut dba, Command::DeleteTask(task)).unwrap();

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.delete"))
        .unwrap();
    ea.apply_remote(&mut dba, &env_for_kind(&dbb, task.bytes(), "task.update"))
        .unwrap();

    let ra = read_task(dba.conn(), task.bytes()).unwrap().unwrap();
    let rb = read_task(dbb.conn(), task.bytes()).unwrap().unwrap();
    assert!(ra.deleted && rb.deleted, "both replicas tombstoned it");
    assert_eq!(
        ra.title, rb.title,
        "and agree on the rest of the row, not just the tombstone"
    );
    assert_eq!(
        ra.title, "contested",
        "the deleting replica's state is what replaced it, so the losing \
             rename is gone on both sides"
    );
    assert_eq!(
        row_stamp(&dba, "tasks", "id", &task),
        row_stamp(&dbb, "tasks", "id", &task),
    );
}

/// The mirror of the above, which is what stops "a delete always wins" from
/// passing for the rule. A Task delete is an ordinary full-state op and
/// carries no special standing: when the remote update is strictly later it
/// wins, and its state — a live task under its new title — replaces the
/// tombstone on both replicas.
#[test]
fn a_task_update_that_wins_lww_replaces_a_delete_it_raced() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "contested".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.create"))
        .unwrap();

    // A deletes it first...
    set_clock(&ca, T0 + 1_000);
    ea.apply(&mut dba, Command::DeleteTask(task)).unwrap();

    // ...and B renames it strictly later, so the rename wins on both sides.
    set_clock(&cb, T0 + 2_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("renamed".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.delete"))
        .unwrap();
    ea.apply_remote(&mut dba, &env_for_kind(&dbb, task.bytes(), "task.update"))
        .unwrap();

    let ra = read_task(dba.conn(), task.bytes()).unwrap().unwrap();
    let rb = read_task(dbb.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(
        (ra.deleted, ra.title.as_str()),
        (rb.deleted, rb.title.as_str()),
        "the two replicas agree"
    );
    assert!(
        !ra.deleted,
        "the winning update's state replaces the row, tombstone included"
    );
    assert_eq!(ra.title, "renamed");
    assert_eq!(
        row_stamp(&dba, "tasks", "id", &task),
        row_stamp(&dbb, "tasks", "id", &task),
    );
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

/// #214: **every** command that writes into the vault-meta log must read its
/// sequence number inside the transaction that may mint that stream's key.
///
/// The mechanism, once, because it is the same for all of them. `ops_insert`
/// resolves the routing stream's epoch, and resolving it can mint the stream's
/// first key; minting emits a `key_envelope` op per recipient, and
/// `emit_control_op` puts every one of those into **vault-meta**, whatever
/// stream was being minted for. So a command whose own op is *also* routed to
/// vault-meta hands out a sequence number the mint then spends.
/// `UNIQUE(stream_id, device_id, seq)` and `INSERT OR IGNORE` do the rest: the
/// losing op disappears and its outbox row fails its foreign key.
///
/// That is why this covers exactly the commands routed to vault-meta and not
/// the ones routed to a Task's own Stream. A `CreateTask` into the Inbox reads
/// the *Inbox's* `seq`, and the envelopes its mint emits land in vault-meta —
/// a different stream, a different counter, no collision. Those paths read
/// `next_seq` outside their transaction too and are correct in doing so.
///
/// `60ee61d` fixed this for `emit_control_op` and `#214` for `create_stream`;
/// the other ten are fixed together with them, and `Engine::meta_slot` is now
/// the only way to obtain a vault-meta sequence number, so the order cannot be
/// got wrong again.
///
/// Every case runs on its own engine that has never seen `ensure_base_epochs`,
/// because the *first* op of any kind mints vault-meta and there is therefore
/// only one command per vault that can hit this.
#[test]
fn every_vault_meta_command_sequences_after_the_key_it_mints() {
    type Build = fn(&mut Db) -> Command;
    let cases: &[(&str, Build)] = &[
        ("stream.create", |_db| {
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            })
        }),
        ("stream.update", |db| Command::UpdateStream {
            id: seed_stream(db, "Work"),
            patch: StreamPatch {
                name: Some("Renamed".into()),
                ..Default::default()
            },
        }),
        ("stream.delete", |db| {
            Command::DeleteStream(seed_stream(db, "Work"))
        }),
        ("context.create", |_db| {
            Command::CreateContext(ContextDraft {
                name: "errands".into(),
                ..Default::default()
            })
        }),
        ("context.update", |db| Command::UpdateContext {
            id: seed_context(db, "errands"),
            patch: ContextPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        }),
        ("context.delete", |db| {
            Command::DeleteContext(seed_context(db, "errands"))
        }),
        ("routine.create", |_db| {
            Command::CreateRoutine(routine_draft(
                inbox_stream_ref(),
                "FREQ=DAILY",
                T0 as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            ))
        }),
        ("routine.update", |db| Command::UpdateRoutine {
            id: seed_bare_routine(db),
            patch: RoutinePatch {
                timezone: Some("UTC".into()),
                ..Default::default()
            },
        }),
        ("routine.delete", |db| {
            Command::DeleteRoutine(seed_bare_routine(db))
        }),
        ("routine.skip", |db| Command::SkipRoutineOccurrence {
            id: seed_bare_routine(db),
            occurrence_key: occurrence_key_at("UTC", ms_to_ts(T0 as i64)).unwrap(),
        }),
        ("review.snapshot", |_db| {
            Command::SaveReviewSnapshot(ReviewSnapshotDraft {
                window_start_ms: T0 - 7 * 86_400_000,
                window_end_ms: T0,
                totals: ReviewTotals::default(),
                streams: Vec::new(),
                streaks: Vec::new(),
                note: None,
            })
        }),
    ];

    // Failures are collected rather than asserted in place. A table that
    // panics on its first bad row says nothing about the other ten, and the
    // whole point of the table is that this is a *class*: reverting two of the
    // fixes has to name two cases, not one.
    let mut failures: Vec<String> = Vec::new();
    for (name, build) in cases {
        let e = engine_random_keys(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut db = db();
        // Deliberately no `ensure_base_epochs`. `Core::open` runs it, which is
        // why none of this is reachable in production; an engine built
        // directly is not so lucky, and that is every test that skips `Core`.
        let cmd = build(&mut db);
        let res = match e.apply(&mut db, cmd) {
            Ok(res) => res,
            Err(err) => {
                failures.push(format!("{name}: collided with the key it mints: {err:?}"));
                continue;
            }
        };

        let seqs = meta_seqs_of(&db, &e.keychain.device_id());
        // Without this the density check below could hold vacuously on a
        // transaction that never minted and so never raced anything.
        if seqs.len() <= 1 {
            failures.push(format!(
                "{name}: emitted only its own op, so nothing was raced: {seqs:?}"
            ));
            continue;
        }
        let dense: Vec<u64> = (1..=seqs.len() as u64).collect();
        if seqs != dense {
            failures.push(format!(
                "{name}: vault-meta seqs must be dense and unique, got {seqs:?}"
            ));
        }
        // Sequence 1 belongs to the first `key_envelope` the mint emitted. A
        // command that took it is a command that read its number before the
        // mint spent it — the bug in the form it would take if the UNIQUE
        // constraint ever stopped catching it.
        if res.seq <= 1 {
            failures.push(format!(
                "{name}: took seq {}, so it sequenced before the envelopes it caused",
                res.seq
            ));
        }
        // And the op is genuinely in the log rather than `OR IGNORE`d away.
        let logged: u32 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ops WHERE op_id = ?",
                params![&res.op_id[..]],
                |r| r.get(0),
            )
            .unwrap();
        if logged != 1 {
            failures.push(format!("{name}: {logged} rows in the log for its op id"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} vault-meta commands race the key they mint:\n  {}",
        failures.len(),
        cases.len(),
        failures.join("\n  ")
    );
}

/// No command lets a caller name the vault-meta stream as an ordinary Stream.
///
/// `EntityRef::new` does not police the bytes, and every draft and patch below
/// crosses the UniFFI seam — so "you would have to construct it by hand" means
/// "any app, any CLI path, any future binding". `require_kind` checked the
/// *kind* and not the id, which was the whole of the gap: a `Stream` ref of
/// sixteen zero bytes is a well-formed Stream reference to the vault-meta log.
///
/// What it would have bought a caller: a Task routed into vault-meta reads the
/// sequence number the control ops are themselves counting, which is `#214`
/// from the other end — the half `Engine::meta_slot` cannot close, because the
/// op is not a vault-meta *command* and has no business being there at all.
///
/// One command is deliberately **absent** from this table and must stay
/// absent: `Command::RotateStreamKey`. Rotating the vault-meta key is a real
/// operation — a device revocation does exactly that — and
/// `reserved_stream_is_still_rotatable` below pins it.
#[test]
fn no_command_accepts_the_vault_meta_stream_as_an_ordinary_target() {
    /// A well-formed `Stream` reference to the vault-meta log. Exactly what a
    /// binding could build, and what nothing rejected before.
    fn meta_ref() -> EntityRef {
        EntityRef::new(EntityKind::Stream, META_STREAM)
    }

    type Build = fn(&Engine, &mut Db) -> Command;
    let cases: &[(&str, Build)] = &[
        ("CreateTask.stream_id", |_e, _db| {
            Command::CreateTask(TaskDraft {
                title: "t".into(),
                stream_id: Some(meta_ref()),
                ..Default::default()
            })
        }),
        ("UpdateTask.patch.stream_id", |e, db| Command::UpdateTask {
            id: new_task(e, db, "t"),
            patch: TaskPatch {
                stream_id: Some(meta_ref()),
                ..Default::default()
            },
        }),
        ("PromoteToStream.stream", |e, db| Command::PromoteToStream {
            id: new_task(e, db, "t"),
            stream: meta_ref(),
        }),
        ("CreateStream.parent_id", |_e, _db| {
            Command::CreateStream(StreamDraft {
                name: "child".into(),
                parent_id: Some(meta_ref()),
                ..Default::default()
            })
        }),
        ("UpdateStream.id", |_e, _db| Command::UpdateStream {
            id: meta_ref(),
            patch: StreamPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        }),
        ("UpdateStream.patch.parent_id", |e, db| {
            let id = e
                .apply(
                    db,
                    Command::CreateStream(StreamDraft {
                        name: "child".into(),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .entity;
            Command::UpdateStream {
                id,
                patch: StreamPatch {
                    parent_id: Some(Some(meta_ref())),
                    ..Default::default()
                },
            }
        }),
        ("DeleteStream.id", |_e, _db| {
            Command::DeleteStream(meta_ref())
        }),
        ("CreateRoutine.template.stream_id", |_e, _db| {
            Command::CreateRoutine(routine_draft(
                meta_ref(),
                "FREQ=DAILY",
                T0 as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            ))
        }),
        ("UpdateRoutine.patch.template", |e, db| {
            let d = routine_draft(
                inbox_stream_ref(),
                "FREQ=DAILY",
                T0 as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            );
            let mut template = d.template.clone();
            let id = e.apply(db, Command::CreateRoutine(d)).unwrap().entity;
            template.stream_id = meta_ref();
            Command::UpdateRoutine {
                id,
                patch: RoutinePatch {
                    template: Some(template),
                    ..Default::default()
                },
            }
        }),
        ("CreateBlock.stream_id", |_e, _db| {
            let mut d = block_draft(9, 1, Some("b"));
            d.stream_id = meta_ref();
            Command::CreateBlock(d)
        }),
        ("ImportBlock.draft.stream_id", |_e, _db| {
            let mut draft = block_draft(9, 1, Some("b"));
            draft.stream_id = meta_ref();
            Command::ImportBlock {
                source: "ics".into(),
                uid: "uid-1".into(),
                draft,
            }
        }),
        ("UpdateBlock.patch.stream_id", |e, db| {
            let id = e
                .apply(db, Command::CreateBlock(block_draft(9, 1, Some("b"))))
                .unwrap()
                .entity;
            Command::UpdateBlock {
                id,
                patch: BlockPatch {
                    stream_id: Some(meta_ref()),
                    ..Default::default()
                },
            }
        }),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (name, build) in cases {
        let mut db = db();
        let e = engine();
        e.ensure_base_epochs(&mut db).unwrap();
        let cmd = build(&e, &mut db);
        match e.apply(&mut db, cmd) {
            Err(EngineError::ReservedStream) => {}
            Err(other) => failures.push(format!("{name}: refused, but not by name: {other:?}")),
            Ok(res) => failures.push(format!(
                "{name}: accepted, and wrote {:?} at seq {}",
                res.entity, res.seq
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} caller-supplied stream ids reach the vault-meta log:\n  {}",
        failures.len(),
        cases.len(),
        failures.join("\n  ")
    );
}

/// The one command that may name the vault-meta stream, and the reason the
/// guard is a per-command decision rather than a blanket ban.
///
/// `RotateStreamKey` mints a new epoch and seals it to every current device.
/// On the vault-meta stream that is half of what a device revocation does, and
/// a user who believes the control log's key is exposed has to be able to ask
/// for it by name. It routes no entity op and reads its own sequence number
/// inside the transaction, so it competes with nothing.
#[test]
fn the_vault_meta_stream_is_still_rotatable_by_name() {
    let mut db = db();
    // A random-key engine, not the derived-key one: the derived-key test
    // keychain reports every key as already held and so writes no
    // `stream_epochs` row, which is the thing this test reads.
    let e = engine_random_keys(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    e.ensure_base_epochs(&mut db).unwrap();
    let before = db
        .with_tx(|tx| e.keychain.current_epoch_tx(tx, &META_STREAM))
        .unwrap()
        .expect("base epochs exist");
    e.apply(
        &mut db,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, META_STREAM),
        },
    )
    .expect("rotating the control log's own key is a supported operation");
    let after = db
        .with_tx(|tx| e.keychain.current_epoch_tx(tx, &META_STREAM))
        .unwrap()
        .expect("still there");
    assert!(after > before, "the rotation minted a new epoch");
}

/// The `key_envelope` ops a mint emits now stamp **before** the op they
/// enable, and that reversal is the one observable consequence of taking the
/// LWW stamp inside the transaction rather than before it.
///
/// It is the causally correct order — the envelope carries the key the op is
/// sealed under — and it changes no merge outcome, because a control op
/// materializes no entity row and so is never the other side of an LWW
/// comparison. What it does mean is that HLC order and `seq` order now agree
/// within one transaction, where before they disagreed.
///
/// In production this never differs: `Core::open` runs `ensure_base_epochs`,
/// so the mint is a no-op and the transaction makes exactly one `hlc.send`
/// either way.
#[test]
fn a_mints_envelopes_stamp_before_the_op_that_caused_them() {
    let mut db = db();
    let e = engine_random_keys(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let res = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    let mut stmt = db
        .conn()
        .prepare(
            "SELECT seq, envelope FROM ops
                 WHERE stream_id = ? AND device_id = ?
                 ORDER BY seq ASC",
        )
        .unwrap();
    let rows: Vec<(u64, Vec<u8>)> = stmt
        .query_map(
            params![&META_STREAM[..], &e.keychain.device_id()[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(rows.len() > 1, "the mint emitted envelopes");

    let hlcs: Vec<Hlc> = rows
        .iter()
        .map(|(_, env)| sunrise_crypto::decode_envelope(env).unwrap().hlc)
        .collect();
    for w in hlcs.windows(2) {
        assert!(
            w[0] < w[1],
            "HLC order must follow seq order within one transaction: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
    assert_eq!(
        rows.last().unwrap().0,
        res.seq,
        "the create holds the last seq, so it also holds the last HLC"
    );
}

// ---- contexts ----

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

    let listed = |e: &Engine, db: &Db| match e.query(db, Query::ContextTasks(errands)).unwrap() {
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

/// `PromoteToStream` takes two ids and checks the kind of both. The command
/// appeared nowhere in this module's tests; its only coverage was a CLI
/// happy path, which passes ids the CLI itself resolved and so never hands
/// it a mismatched pair. The crate already has this exact test for three
/// sibling families (Context, Block, Attachment).
///
/// What this pins is the command's observable contract, not the two
/// `require_kind` calls in `promote_task` itself: `update_task`, which it
/// delegates to, repeats both checks — `require_kind(id, Task)` on entry
/// and `require_kind(s, Stream)` on `patch.stream_id` — and returns the
/// same `Invalid` variant with the same message. Deleting either call from
/// `promote_task` alone changes nothing any caller can see. Deleting both
/// copies of either check does, and is what fails this test.
#[test]
fn promote_to_stream_rejects_either_id_of_the_wrong_kind() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "promote me");
    let stream = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Ops".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;

    // A Stream id where the Task belongs.
    let err = e
        .apply(&mut db, Command::PromoteToStream { id: stream, stream })
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    // A Task id where the Stream belongs.
    let err = e
        .apply(
            &mut db,
            Command::PromoteToStream {
                id: task,
                stream: task,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    // A Context id in the Stream slot: the kind is checked, not merely
    // "is it not a Task".
    let ctx = new_context(&e, &mut db, "errand");
    let err = e
        .apply(
            &mut db,
            Command::PromoteToStream {
                id: task,
                stream: ctx,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    // And the well-kinded pair still works, so the rejections above are the
    // guards and not a broken fixture.
    e.apply(&mut db, Command::PromoteToStream { id: task, stream })
        .unwrap();
    assert_eq!(read_task_t(&e, &db, task).stream_id, stream);
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
            ensure_stream_row(tx, &r.template.stream_id, NOW as u64)?;
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

/// **A restart must not make a device emit beneath its own ops** (#112).
///
/// `MonotonicHlc` starts at `Hlc::default()`, so before
/// [`Engine::prime_hlc`] a reopened vault's clock was the bare wall clock.
/// That is not the same thing as where the device left off: `Hlc::receive`
/// admits a peer up to `MAX_DRIFT_MS` ahead, and `Hlc::send` keeps that
/// lead until local time catches up. Four minutes is inside the window and
/// is what B's clock leads by here.
///
/// The consequence is not a tie for `seq` to break. `lww_wins` compares
/// `hlc` first, so A's *second* edit sorts below its first and every
/// replica that merges both keeps the first — while A, which runs no LWW
/// gate on its own writes, keeps the second. The last assertion is that
/// divergence, made concrete on B.
#[test]
fn a_restart_does_not_make_this_device_emit_beneath_its_own_ops() {
    const LEAD_MS: u64 = 4 * 60 * 1000;

    let a_clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], a_clock.clone());
    let eb = engine_seeded(
        ROOT,
        [2u8; 32],
        Arc::new(FakeClock(PLMutex::new(T0 + LEAD_MS))),
    );
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    // B, whose clock leads, creates a task. A absorbs it and with it B's
    // stamp: A's HLC is now four minutes above A's own wall clock.
    let task = eb
        .apply(
            &mut dbb,
            Command::CreateTask(TaskDraft {
                title: "from b".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let create_env = create_env_for(&dbb, task.bytes());
    ea.apply_remote_all(&mut dba, &create_env).unwrap();
    assert!(
        ea.hlc.peek().physical_ms >= T0 + LEAD_MS,
        "A has adopted the leading stamp it just absorbed"
    );

    // A edits it, carrying that lead into its own op.
    ea.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("edited before the restart".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let before = task_stamp(&dba, task.bytes());
    assert!(before.physical_ms >= T0 + LEAD_MS);

    // The restart: a fresh `Engine` over the same database, which is what
    // `Core::open` constructs — and, like `Core::open`, primed from the log.
    let ea2 = engine_seeded(ROOT, [1u8; 32], a_clock);
    ea2.prime_hlc(&dba).unwrap();
    ea2.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("edited after the restart".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let after = task_stamp(&dba, task.bytes());

    assert!(
        after > before,
        "a device's own later op must sort after its earlier one across a \
             restart: before={before:?} after={after:?}"
    );

    // And the whole point of that: B, merging both of A's edits under LWW,
    // must end on the later one.
    for env in update_envs_for(&dba, task.bytes()) {
        eb.apply_remote_all(&mut dbb, &env).unwrap();
    }
    assert_eq!(
        task_title(&dbb, task.bytes()),
        "edited after the restart",
        "the peer must keep A's newer edit, not silently discard it"
    );
    assert_eq!(
        task_title(&dba, task.bytes()),
        task_title(&dbb, task.bytes()),
        "and the two replicas must agree"
    );
}

/// The logical half has to come back too, which is why `prime_hlc` decodes
/// envelopes instead of reading `MAX(ts_ms)` and stopping there.
///
/// Nothing here involves a peer or a skewed clock: one device, one stalled
/// millisecond, several ops. `Hlc::send` puts them at `(t, 1)`, `(t, 2)`,
/// `(t, 3)`; a restart primed only from `ts_ms` would resume at `(t, 0)`
/// and emit `(t, 1)` again — below the ops already stamped, and an
/// inversion rather than the tie `seq` breaks.
#[test]
fn priming_restores_the_logical_half_not_only_the_physical_one() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], clock.clone());
    let mut dba = db_root(ROOT);

    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "one".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    for title in ["two", "three"] {
        ea.apply(
            &mut dba,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some((*title).to_string()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }
    let before = task_stamp(&dba, task.bytes());
    assert_eq!(before.physical_ms, T0, "the wall clock never moved");
    assert!(
        before.logical > 0,
        "the ops are separated by the logical counter alone"
    );

    let ea2 = engine_seeded(ROOT, [1u8; 32], clock);
    ea2.prime_hlc(&dba).unwrap();
    ea2.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("four".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let after = task_stamp(&dba, task.bytes());

    assert_eq!(after.physical_ms, before.physical_ms);
    assert!(
        after.logical > before.logical,
        "the counter must resume, not restart: before={before:?} after={after:?}"
    );
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
    let cert = ea
        .keychain
        .issue_cert_for(
            c_id,
            ec.keychain.device_signing_pub(),
            ea.keychain.device_dh_pub(),
            "impostor",
            "macos",
            T0,
        )
        .expect("issue the impostor cert");
    dbb.with_tx(|tx| {
        eb.apply_control_op(
            tx,
            &InnerOp::DeviceCertPublish(cert),
            &a_id,
            Hlc::at(T0),
            T0,
            1,
        )
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
    let own = ec.keychain.cert_blob();
    dbb.with_tx(|tx| {
        eb.apply_control_op(
            tx,
            &InnerOp::DeviceCertPublish(own.clone()),
            &c_id,
            Hlc::at(T0),
            T0,
            1,
        )
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
        dbb.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0, 1))
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

/// **A revoked device's third-party recipient claim is not recorded at all.**
///
/// The other half of the bound below, and the second thing ADR-0041 takes away
/// from a revoked device. `key_envelope_recipients` is what
/// `backfill_key_envelopes` reads to decide a device has already been served,
/// so a claim filed on nobody's behalf is a way to withhold a key — and the
/// comment in `apply_control_op` names the exact sender class it could not
/// argue away: a revoked device's reads are bounded by the rotation and (before
/// this) its writes by nothing, so it could file these rows and could not read
/// what they withhold.
///
/// This gate is free of the convergence question the register's fold is about,
/// because the table is a hint rather than state: a replica that declines the
/// row finds no row and emits the backfill, so the gate can only ever cause
/// *more* key distribution, never less.
#[test]
fn a_revoked_devices_third_party_envelope_claim_is_not_recorded() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_random_keys(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let victim = [0x77u8; 16];
    let stream = [0x5d; 16];

    let claim = |db: &mut Db| {
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch: 1,
            recipient: Recipient::Device(victim),
            key_id: [0u8; 8],
            hpke_ciphertext: vec![0u8; 48],
        });
        db.with_tx(|tx| er.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0, 1))
            .unwrap();
    };
    let filed = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND recipient = ?",
                params![&stream[..], &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    // While A is a member the claim is recorded, unchecked, as it always was.
    claim(&mut db);
    assert_eq!(filed(&db), 1);

    // Revoked, and a claim about a *different* epoch is refused outright.
    revoke(&er, &mut db, &eb, a_id, T0);
    let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
        stream_id: stream,
        epoch: 2,
        recipient: Recipient::Device(victim),
        key_id: [0u8; 8],
        hpke_ciphertext: vec![0u8; 48],
    });
    db.with_tx(|tx| er.apply_control_op(tx, &inner, &a_id, Hlc::at(T0 + 1), T0 + 1, 1))
        .unwrap();
    assert_eq!(
        filed(&db),
        1,
        "a revoked device must not be able to say a key has already been delivered"
    );
}

/// **An *unwound* device's third-party recipient claim is not recorded
/// either.**
///
/// The gate in `apply_control_op` asks `is_read_bounded` and not `is_revoked`,
/// and this is the case that separates the two. Its own justification names the
/// sender class it exists to catch — a device whose *reads are bounded* by the
/// rotation, so it could file these rows and could not read what they withhold
/// — and since migration 0028 that class is `device_read_bounds`. A device the
/// fold rehabilitated is `read_bounded: true, revoked: false`: it receives no
/// key this vault mints, which is exactly the standing the comment describes,
/// and under `is_revoked` it passed.
///
/// Widening the gate cannot withhold a key, for the reason
/// `a_revoked_devices_third_party_envelope_claim_is_not_recorded` gives: a
/// replica that declines the row finds no row, so `backfill_key_envelopes`
/// emits the envelope. More senders refused means more backfill, never less.
#[test]
fn an_unwound_devices_third_party_envelope_claim_is_not_recorded() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_random_keys(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    let victim = [0x77u8; 16];
    let stream = [0x5d; 16];

    let claim = |db: &mut Db, epoch: u32| {
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch,
            recipient: Recipient::Device(victim),
            key_id: [0u8; 8],
            hpke_ciphertext: vec![0u8; 48],
        });
        db.with_tx(|tx| er.apply_control_op(tx, &inner, &c_id, Hlc::at(T0 + 2), T0 + 2, 1))
            .unwrap();
    };
    let filed = |db: &Db, epoch: u32| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&stream[..], epoch, &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    // A retires laptop C; months later B retires A. The fold stops believing
    // A's op, so C leaves the register — and stays in `device_read_bounds`.
    revoke(&er, &mut db, &ea, c_id, T0);
    revoke(&er, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "the register no longer calls C revoked: this is the unwind"
    );
    assert!(
        read_bound_row(&db, &c_id).is_some(),
        "and the bound is still C's, which is the whole of what 0028 buys"
    );

    claim(&mut db, 1);
    assert_eq!(
        filed(&db, 1),
        0,
        "a device that receives no key must not be able to say a key has \
         already been delivered, whether or not the register still names it"
    );
}

/// A third party's recipient claim is bounded in epoch before it is
/// recorded.
///
/// The `Recipient::Device(other)` arm files a `key_envelope_recipients`
/// row on another device's word alone — it holds no key for that
/// ciphertext, so nothing about the claim is checkable — and that row is
/// what `backfill_key_envelopes` reads to decide a device already has the
/// key. Unbounded, it is a way to make a *future* epoch undeliverable:
/// file rows for e+1..e+k, wait for the rotation that reaches one, and the
/// backfill finds a row, emits nothing, and leaves a device with no key
/// while its ops park in `deferred_ops` until the TTL drops them with
/// nothing surfacing.
#[test]
fn a_third_party_envelope_claim_is_bounded_in_epoch() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let victim = [0x77u8; 16];
    let stream = [0x5d; 16];

    // The bytes are garbage on purpose. This path never opens them, which
    // is precisely why the claim needs a bound that does not depend on
    // opening them.
    let claim = |db: &mut Db, epoch: u32| {
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch,
            recipient: Recipient::Device(victim),
            key_id: [0u8; 8],
            hpke_ciphertext: vec![0u8; 48],
        });
        db.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0, 1))
            .unwrap();
    };
    let filed = |db: &Db, epoch: u32| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                     WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&stream[..], epoch, &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    // This vault holds no key for the stream, so its live epoch is 0.
    claim(&mut dbb, MAX_EPOCH_LEAP);
    assert_eq!(
        filed(&dbb, MAX_EPOCH_LEAP),
        1,
        "the far edge of the window is still inside it"
    );

    claim(&mut dbb, MAX_EPOCH_LEAP + 1);
    assert_eq!(
        filed(&dbb, MAX_EPOCH_LEAP + 1),
        0,
        "a claim one epoch past the window must not be recorded"
    );
    claim(&mut dbb, u32::MAX);
    assert_eq!(
        filed(&dbb, u32::MAX),
        0,
        "and an absurd epoch must not poison every rotation this account will ever do"
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

/// The relay's half of a revocation is queued by the same transaction that
/// writes the op, so the two cannot diverge.
///
/// A vault that believes it revoked a device and never queued the telling
/// is the failure the queue exists to prevent — and it is silent, because
/// the local half succeeds and the user sees the device marked revoked. It
/// is one transaction rather than two writes for the same reason the outbox
/// row shares its op's transaction.
///
/// The queue is a queue rather than a call because `revoke_device` has to
/// work with no network at all: a device that is gone is the whole
/// scenario, and it is exactly the moment a user is least likely to be
/// online. `sync_driver::drain_relay_revocations` makes the call.
#[test]
fn revoking_a_device_queues_the_relays_half_in_the_same_transaction() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    assert!(
        pending_relay_revocations(&dba).is_empty(),
        "nothing is owed before a revocation"
    );

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap();

    assert_eq!(
        pending_relay_revocations(&dba),
        vec![eb.keychain.device_id()],
        "the op and the intent are written together or not at all"
    );
    assert!(
        revocation_row(&dba, &eb.keychain.device_id()).is_some(),
        "and the register itself is still written"
    );
}

/// Two replicas, the same two revocation ops, opposite arrival orders, one
/// final state — the register, and the effect it has on a third device's
/// ops.
///
/// The row is an LWW register on the declaring op's own `(hlc, device_id)`,
/// which is how ADR-0014 resolves every other concurrent write here. It
/// converges regardless of order, and — unlike the `MIN` join it replaces —
/// it is reversible: a later revocation supersedes an earlier one, so a cut
/// that landed wrong can be corrected by revoking again.
///
/// `revoke_reason` and `revoked_by` are asserted alongside the timestamp on
/// purpose. Convergence on the cut alone would still leave two replicas
/// disagreeing about who did it and why, which is what the previous
/// `(cut, revoker)` tie-break left order-dependent.
#[test]
fn concurrent_revocations_converge_on_one_register_whatever_the_order() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let earlier = engine_seeded(ROOT, [2u8; 32], clock());
    let later = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    // The same two ops. r1 sees the later one first; r2 sees it last.
    revoke(&r1, &mut db1, &later, t, T0 + 9_000);
    revoke(&r1, &mut db1, &earlier, t, T0 + 1_000);
    revoke(&r2, &mut db2, &earlier, t, T0 + 1_000);
    revoke(&r2, &mut db2, &later, t, T0 + 9_000);

    let row1 = revocation_row(&db1, &t);
    assert_eq!(
        row1,
        revocation_row(&db2, &t),
        "arrival order decided nothing"
    );
    assert_eq!(
        row1.as_ref().unwrap().0,
        i64::try_from(T0).unwrap() + 9_000,
        "the last writer wins, so a later revocation can correct an earlier one"
    );
    assert_eq!(
        row1.as_ref().unwrap().2.as_slice(),
        &later.keychain.device_id()[..],
        "and every column follows the winning op, not just the timestamp"
    );

    // The effect converges with the row. A recorded revocation is
    // effective on both replicas -- there is no time test to disagree
    // about -- and the columns they agree on above are what decide *which*
    // of two racing revocations is the one on record.
    for (e, db) in [(&r1, &db1), (&r2, &db2)] {
        assert!(e.is_revoked(db.conn(), &t).unwrap());
    }
}

/// A parked op older than [`DEFERRED_TTL_MS`] is swept by the next drain.
///
/// The caps are tested; the TTL was not, which left "a row this old is
/// ciphertext nobody will ever open" as a sentence rather than a property.
/// A drain is the only moment the table is known to be changing, so it is
/// where the sweep runs — and this is what fails if it stops running.
#[test]
fn a_parked_op_older_than_the_ttl_is_swept_by_the_next_drain() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    // A writes to the Inbox. B holds the vault-meta key but never the
    // Inbox key, so the op parks.
    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "parked and forgotten".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    let envelopes_before = key_envelope_envs(&dba).len();
    assert!(eb
        .apply_remote_all(&mut dbb, &env_bytes(&dba, &task.op_id))
        .unwrap()
        .is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "the premise: it is parked");

    // A month passes on B, and then some *other* key arrives — a fresh
    // Stream A creates, whose key B does receive.
    set_clock(&cb, T0 + DEFERRED_TTL_MS + 1);
    set_clock(&ca, T0 + DEFERRED_TTL_MS + 1);
    let stream = ea
        .apply(
            &mut dba,
            Command::CreateStream(StreamDraft {
                name: "later".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "in the new stream".into(),
            stream_id: Some(stream),
            ..Default::default()
        }),
    )
    .unwrap();

    // Only the envelopes minted after the park, so B never learns the
    // Inbox key and the row can only leave by being swept.
    for env in key_envelope_envs(&dba).into_iter().skip(envelopes_before) {
        let _ = eb.apply_remote_all(&mut dbb, &env);
    }
    assert!(
        eb.keychain
            .stream_keys_at(&sunrise_domain::INBOX_STREAM_BYTES, 1)
            .is_empty(),
        "B still has no Inbox key, so the row was not drained normally"
    );
    assert_eq!(
        deferred_rows(&dbb),
        0,
        "an op parked past the TTL is swept by the next drain"
    );
    assert!(
        read_task(dbb.conn(), task.entity.bytes())
            .unwrap()
            .is_none(),
        "and it never materialized"
    );
}

/// A device cannot move its own revocation cut.
///
/// Register hygiene, independent of any gate: a device that can rewrite its
/// own row can push its cut forward and undo a revocation somebody else
/// made of it, and the register is last-writer-wins, so its op would win.
/// `Command::RevokeDevice` refuses self-revocation locally for a different
/// reason — rotating every key away from the only device holding them is
/// not a recoverable state — and neither check implies the other.
#[test]
fn a_device_cannot_move_its_own_revocation_cut() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    trust(&er, &mut db, &ea);
    let a_id = ea.keychain.device_id();

    // B revokes A.
    revoke(&er, &mut db, &eb, a_id, T0);
    let after_b = revocation_row(&db, &a_id).expect("B's revocation stands");

    // A tries to move its own cut forward, which under last-writer-wins
    // would otherwise supersede B's and un-revoke it for everything after.
    revoke(&er, &mut db, &ea, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &a_id),
        Some(after_b),
        "a device must not be able to edit the register entry about itself"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and it is still revoked: the row it tried to edit is what decides that"
    );

    // A revocation of A by anyone else still lands, so this is a check on
    // the subject, not a freeze on the row.
    revoke(&er, &mut db, &eb, a_id, T0 + 90_000);
    assert_eq!(
        revocation_row(&db, &a_id).unwrap().0,
        i64::try_from(T0).unwrap() + 90_000
    );
}

/// **A revoked device cannot revoke another device.**
///
/// The hole ADR-0041 closes. Before it, the only `device_revoke` the arm
/// refused was one naming its own sender, so a laptop the account had already
/// expelled could expel everything else in the account: its cert is still on
/// the chain, it still holds the vault-meta key for the epoch it was cut at,
/// and peers keep old epoch keys, so the op verifies, decrypts and applies
/// everywhere. Revocation has no inverse — nothing deletes a
/// `device_revocations` row and `is_revoked` is presence and nothing else — so
/// the result was permanent on every replica
/// ([#82](https://github.com/justin13888/Sunrise/issues/82)).
#[test]
fn a_revoked_device_cannot_revoke_another_device() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // B revokes A: the ordinary administrative action.
    revoke(&er, &mut db, &eb, a_id, T0);
    assert!(er.is_revoked(db.conn(), &a_id).unwrap());

    // A then tries to revoke C, which is the account turning on itself.
    revoke(&er, &mut db, &ea, c_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "an expelled device must not be able to expel anything else"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A is still out"
    );

    // The op is kept, not dropped: that is what makes the skip reversible.
    assert_eq!(
        ledger_rows(&db),
        2,
        "the refused op is stored and only its effect is refused"
    );

    // Anyone else revoking C still works, so this gates the sender and does
    // not freeze the row.
    revoke(&er, &mut db, &eb, c_id, T0 + 90_000);
    assert!(revocation_row(&db, &c_id).is_some());
}

/// **And it cannot get around that by dating the op earlier.**
///
/// The test above puts A's op *after* its own cut, which is the polite case.
/// The sort key is A's to choose, and choosing is free: [`Hlc`] bounds only
/// the future through `MAX_DRIFT_MS`, a reading in the past is ordinary and
/// `crates/sunrise-core/src/engine/sync.rs` says so, and nothing ties an op's
/// HLC to that sender's own `seq` or to any earlier stamp it sent.
///
/// So while the gate judged a sender against the **prefix** of the walk, the
/// whole of ADR-0041 was one subtraction away from being bypassed: date the op
/// a millisecond below your own cut, sort first, and land. Iterating the
/// remaining device ids then revokes the account, which is precisely the
/// outcome #82 is about and which §Alternatives (f) rejects a rival design for
/// admitting. Judging the sender against the whole ledger is what closes it,
/// and this is the assertion that a date cannot reopen it.
#[test]
fn a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // B revokes A: the ordinary administrative action.
    revoke(&er, &mut db, &eb, a_id, T0);
    assert!(er.is_revoked(db.conn(), &a_id).unwrap());

    // A answers by reaching for C one millisecond *below* its own cut, which
    // is the entire attack.
    revoke(&er, &mut db, &ea, c_id, T0 - 1);

    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "an expelled device must not expel a third party by back-dating the op"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A is still out: its op did not move its own cut either"
    );
    assert_eq!(
        ledger_rows(&db),
        2,
        "the refused op is stored and only its effect is refused"
    );
}

/// **The skip does not depend on which of the two ops arrived first.**
///
/// This is the property ADR-0034 corollary 3 demands of any peer-side
/// enforcement, and the reason the register is a fold over a kept ledger rather
/// than a refusal taken where an op lands. A replica that sees A's revocation
/// of C *before* it learns A was itself revoked must end up with the same
/// register as one that sees them the other way round — otherwise two replicas
/// holding identical op sets disagree forever about who is a member.
#[test]
fn the_register_is_the_same_whichever_order_the_two_revocations_arrive() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // Replica one: B-revokes-A first, then A-revokes-C.
    let mut one = db_root(ROOT);
    revoke(&er, &mut one, &eb, a_id, T0);
    revoke(&er, &mut one, &ea, c_id, T0 + 60_000);

    // Replica two: the same two ops, delivered the other way round. A's op
    // lands while A is still, as far as this replica knows, a member.
    let mut two = db_root(ROOT);
    revoke(&er, &mut two, &ea, c_id, T0 + 60_000);
    assert!(
        revocation_row(&two, &c_id).is_some(),
        "before it knows better, this replica believes A's op"
    );
    revoke(&er, &mut two, &eb, a_id, T0);

    assert_eq!(revocation_row(&one, &a_id), revocation_row(&two, &a_id));
    assert_eq!(
        revocation_row(&one, &c_id),
        revocation_row(&two, &c_id),
        "the same op set must give the same register in either delivery order"
    );
    assert_eq!(
        revocation_row(&two, &c_id),
        None,
        "and learning the sender was revoked undoes what it had already written"
    );

    // **The register converges and the read bound does not, and that is the
    // shipped behaviour rather than an oversight.**
    //
    // This test builds the two arrival orders that separate them, so asserting
    // only the register leaves the interesting half unobserved. `one` applied
    // `B -> A` first: its first fold produced the register `{A}`, its second
    // already gated `A -> C`, so C never entered a register on this replica and
    // the `INSERT OR IGNORE` had nothing to write. `two` applied `A -> C` first,
    // bounded C on that fold, and keeps the row forever because nothing deletes
    // from `device_read_bounds`.
    //
    // Pinned as the **known current behaviour**, not as the behaviour anyone
    // wants: on `two` the bound holds C out of every recipient set, and on `one`
    // C is an ordinary member that will be sealed every epoch this replica
    // mints. Closing
    // [#282](https://github.com/justin13888/Sunrise/issues/282) means the two
    // become equal and these assertions turn red — which is the point of
    // writing them down rather than leaving the gap undiscovered.
    assert_eq!(
        read_bound_row(&one, &c_id),
        None,
        "known gap (#282): the replica that learned of A's own revocation first \
         never bounds C, so it goes on sealing C every epoch it mints"
    );
    assert!(
        read_bound_row(&two, &c_id).is_some(),
        "while the replica that believed A's op first keeps the bound, because \
         the ratchet never gives a row back"
    );
    assert!(
        read_bound_row(&one, &a_id).is_some() && read_bound_row(&two, &a_id).is_some(),
        "both replicas bound A, which is why the divergence is C's alone and not \
         a difference in what either replica believes about the ledger"
    );
}

/// **A cut correction does not bring a skipped revocation back, and the
/// remedy is to make it again.**
///
/// The question [#82](https://github.com/justin13888/Sunrise/issues/82) defers,
/// answered with its second option and pinned here so the answer is asserted
/// rather than assumed. **The gate reads no cut of any kind**: it is a
/// set-membership test over the ledger's `(sender, revoked)` pairs, and an HLC
/// decides only which row wins the register. Correcting a cut appends a second
/// revocation of the same sender by the same party, which changes that winner
/// and changes nothing about who has revoked whom — so the gate answers the
/// same question the same way, whether the correction is dated **after** the op
/// it would rescue or **before** it. Both directions are asserted below, and
/// asserting both is the whole of what this test pins: the "either direction"
/// half of that sentence, which is the half a reader has no other reason to
/// believe.
///
/// What it does **not** do is tell the shipped whole-ledger rule apart from one
/// reading the walk's prefix. Replay the prefix rule over this test's own four
/// rows, ascending — `(T0-30s, B, A)`, `(T0, B, A)`, `(T0+60s, A, C)`,
/// `(T0+120s, B, A)` — and it gates A's op too: the first two rows are ungated
/// and seat `revokers[A] = {B}`, so by the third row `B != C` already holds and
/// C stays off, which is the answer the shipped rule gives. That is structural
/// and not an artefact of these constants: a revocation of the sender can only
/// ever **add** to a prefix's gating set for that sender, never take a revoker
/// out of it, so back-dating a correction can only make a prefix rule gate more
/// readily and can never be the direction that separates the two rules. The
/// direction that separates them is a cut sorting *above* the op with none
/// below it, and that is pinned by
/// `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`.
///
/// What *does* un-skip a row is the one thing that empties its sender's revoker
/// set: somebody revoking that sender's revoker. A cut correction is not that,
/// in either direction.
///
/// That is acceptable here and would not be acceptable for a task edit, which
/// is exactly why ADR-0041 scopes the gate to this op family. What is lost is
/// an administrative act by a device the account has expelled — a thing a human
/// can simply do again, from a device the account still trusts, and a thing
/// they would want to look at again anyway.
#[test]
fn a_cut_correction_does_not_re_fold_a_skipped_revocation() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // A device with a slow clock cuts A at T0, before A's honest revocation of
    // C at T0 + 60s — so that one is skipped.
    revoke(&er, &mut db, &eb, a_id, T0);
    revoke(&er, &mut db, &ea, c_id, T0 + 60_000);
    assert_eq!(revocation_row(&db, &c_id), None);

    // Correcting the cut forward appends a second revocation of A by B. It
    // changes which row wins A's entry in the register and nothing about who
    // has revoked whom, so C stays off.
    revoke(&er, &mut db, &eb, a_id, T0 + 120_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "the gate reads no cut, so moving it forward does not un-skip the op"
    );
    // And backward, past the op it would rescue. This half is here because the
    // sentence above claims *either* direction, not because it discriminates: a
    // prefix rule gates A's op here too, since the T0 cut already sits below it
    // and a fourth row revoking A can only add to A's gating set. What does
    // discriminate is a cut sorting above the op with none below it, in
    // `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`.
    revoke(&er, &mut db, &eb, a_id, T0 - 30_000);
    assert_eq!(
        ledger_rows(&db),
        4,
        "the back-dated correction is in the ledger, so the next assertion is \
         about a row that exists"
    );
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "and moving it backward does not either, for the same reason — which \
         pins the sentence's either-direction half and not a difference from a \
         prefix rule, since that gates this op as well"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A is still revoked: correcting the cut is not un-revoking"
    );

    // The remedy, which is the whole reason the loss is tolerable: anyone the
    // account still trusts revokes C again.
    revoke(&er, &mut db, &eb, c_id, T0 + 180_000);
    assert!(er.is_revoked(db.conn(), &c_id).unwrap());
}

/// **Two devices revoking each other still converge on both revocations.**
///
/// The gate's exception, asserted from the direction that makes it necessary.
/// An HLC sorts first by being dated earlier and nothing charges for that, so a
/// rule of "whoever revoked first silences the other" would hand a stolen
/// laptop the account: back-date a revocation of the owner's Mac and the Mac's
/// answer never lands. Both revocations standing is the safe resolution, and it
/// is what the engine did before this gate existed.
#[test]
fn a_gate_on_the_sender_does_not_let_a_back_dated_revocation_silence_its_target() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, b_id) = (ea.keychain.device_id(), eb.keychain.device_id());

    // A, holding stolen credentials, gets its revocation of B in a year early.
    revoke(&er, &mut db, &ea, b_id, T0 - 365 * 24 * 60 * 60 * 1000);
    // B answers at the real time.
    revoke(&er, &mut db, &eb, a_id, T0);

    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "the answer to a back-dated revocation must still land"
    );
    assert!(er.is_revoked(db.conn(), &b_id).unwrap());
}

/// A revoked device's revocations from **before** its own cut are unwound.
///
/// This test asserted the opposite until ADR-0041 §Decision 1 was rewritten,
/// and the inversion is the point of it rather than an accident of one. The
/// old property — revocation is not retroactive, so a revoked device's earlier
/// revocations still stand — reads well and was *the vulnerability*, because
/// "earlier" is not a fact the ledger holds. The cut is the op's own HLC, and
/// an HLC is whatever its sender wrote: `MAX_DRIFT_MS` bounds only the future,
/// a reading in the past is ordinary, and nothing ties an op's stamp to that
/// sender's `seq` or to any earlier stamp it sent. So nothing distinguishes an
/// honestly-earlier revocation from one a revoked device back-dated a minute
/// ago, and a gate that believed the first believed the second — which is the
/// whole account, one small integer at a time. See
/// `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`.
///
/// So non-retroactivity is retired here deliberately, for this op family only.
/// Revocation stays non-retroactive everywhere else in this module — a cert
/// issued under a superseded identity still identifies its device — and this
/// family is the exception because its effect can be re-derived, which is the
/// same scoping argument ADR-0041 makes for gating control ops and not entity
/// ops. What is lost is an administrative act by a device the account has
/// expelled: a thing a human can simply do again from a device the account
/// still trusts, and a thing they would want to look at again anyway.
#[test]
fn a_revocation_written_before_the_senders_own_cut_is_unwound_when_the_sender_is_revoked() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // A retires C, apparently long before anything happens to A.
    revoke(&er, &mut db, &ea, c_id, T0);
    assert!(
        revocation_row(&db, &c_id).is_some(),
        "before it knows better, this replica believes A's op"
    );
    // A is itself revoked a minute later.
    revoke(&er, &mut db, &eb, a_id, T0 + 60_000);

    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "a revoked device's revocations do not stand, whatever date they carry"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A itself is out"
    );

    // The remedy, which is what makes the loss tolerable: anyone the account
    // still trusts revokes C again.
    revoke(&er, &mut db, &eb, c_id, T0 + 120_000);
    assert!(er.is_revoked(db.conn(), &c_id).unwrap());
}

/// **A mutual pair locks both devices out of revoking anybody else.**
///
/// Green before this test existed and green after, which is the point of it:
/// the behaviour is deliberate and pinned, not accidental and undiscovered.
///
/// The mutual exception is what keeps two devices revoking each other
/// converging on *both* revocations — see
/// `a_gate_on_the_sender_does_not_let_a_back_dated_revocation_silence_its_target`
/// for the takeover it prevents. Its cost is here. Once X and O have revoked
/// each other, each one's revoker set holds exactly one entry and it is the
/// other, so each is forgiven for revoking the other and gated for revoking
/// anyone else — including the honest device of the pair, and permanently,
/// because revocation has no inverse
/// ([#241](https://github.com/justin13888/Sunrise/issues/241)).
///
/// What it costs is *third-party* revocation, and it costs it to the two
/// devices in the relationship and to nobody else. That bound was asserted
/// here through `et` — a device neither of them ever named — which is exactly
/// the device the bound is least interesting about, while `ed`, the device
/// **both** of them named, went unchecked. It was also, for one revision of
/// the fold, false of `ed`: each gated op seated its sender in `ed`'s revoker
/// set, so being named by a device the account had expelled cost `ed` its own
/// ability to revoke. The discount pass closed that and the assertion on `ed`
/// is here so the bound is pinned where it can fail.
///
/// Revocation is not gated on `ID_S_priv` anywhere either, so identity
/// rotation and pairing sponsorship are untouched, and the last lines here are
/// the remedy: any current device outside the pair still revokes whoever it
/// likes. The lockout is total only in a two-device account, where there is no
/// third device to ask.
///
/// It is recorded rather than repaired because no ledger-only rule can do
/// better: after a mutual revocation the two devices are symmetric in the
/// ledger and nothing tells the honest one from the compromised one.
#[test]
fn a_mutual_pair_locks_both_devices_out_of_third_party_revocation() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ez = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let (d_id, z_id) = (ed.keychain.device_id(), ez.keychain.device_id());

    // X is compromised and gets its revocation of O in first; O answers.
    revoke(&er, &mut db, &ex, o_id, T0 + 10_000);
    revoke(&er, &mut db, &eo, x_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        er.is_revoked(db.conn(), &o_id).unwrap(),
        "the mutual exception is what puts both of them out"
    );

    // O now reaches for a third party, and is gated: its only revoker is X,
    // and X is not the device this op names.
    revoke(&er, &mut db, &eo, d_id, T0 + 30_000);
    assert_eq!(
        revocation_row(&db, &d_id),
        None,
        "the surviving half of a mutual pair cannot revoke a third party"
    );
    // And symmetrically, so this is a property of the pair and not of O.
    revoke(&er, &mut db, &ex, d_id, T0 + 40_000);
    assert_eq!(revocation_row(&db, &d_id), None);
    assert_eq!(
        ledger_rows(&db),
        4,
        "both refused ops are stored; only their effect is refused"
    );

    // **D is the device the pair actually reached**, and it is the assertion
    // this test was missing: two gated ops named it, and until the discount
    // pass existed each of those ops seated its sender in D's revoker set, so
    // D — which nobody has revoked — could revoke nothing. Both are
    // discounted, because each of X and O is revoked by the other, who is not
    // D.
    revoke(&er, &mut db, &ed, z_id, T0 + 45_000);
    assert!(
        er.is_revoked(db.conn(), &z_id).unwrap(),
        "being named by a gated op must not cost D its own ability to revoke"
    );

    // The remedy, and the bound on the damage: any other current device in the
    // account still revokes whoever it likes.
    revoke(&er, &mut db, &et, d_id, T0 + 50_000);
    assert!(
        er.is_revoked(db.conn(), &d_id).unwrap(),
        "a third current device is the way out, and a two-device account has none"
    );
    assert_eq!(
        revocation_row(&db, &z_id),
        None,
        "and D's own revocation goes with it, which is decision 1's unwinding \
         and not this gate"
    );
}

/// **A gated op must not seat its sender in the revoker set of the device it
/// named.**
///
/// The hole the discount pass closes, asserted from the direction that makes
/// it an account takeover rather than a curiosity. The revoker map is built
/// from *every* row and only the walk judges one, so before the discount a row
/// the walk threw away still left its sender sitting in its target's set — and
/// one entry in that set gates the target against every third party, for good.
///
/// The cost to the attacker was N ordinary ops and nothing else. No crafted
/// stamp, no back-dating, no id discovery: X already holds every `devices`
/// row, a revoked device's ops are stored unconditionally, and its envelopes
/// still verify. X names each remaining device once; every op is correctly
/// gated and revokes nobody; and the account can never revoke a stolen device
/// again on any replica.
///
/// So the second half here is the assertion that matters. P not being revoked
/// was always true. P still being *able to revoke* is what was lost.
#[test]
fn a_gated_revocation_does_not_seat_its_sender_in_its_targets_revoker_set() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eq = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, p_id) = (ex.keychain.device_id(), ep.keychain.device_id());
    let (q_id, d_id) = (eq.keychain.device_id(), ed.keychain.device_id());

    // O expels X: the ordinary administrative action.
    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());

    // X answers by naming the devices that are left, one ordinary op each.
    revoke(&er, &mut db, &ex, p_id, T0 + 10_000);
    revoke(&er, &mut db, &ex, q_id, T0 + 20_000);
    assert_eq!(revocation_row(&db, &p_id), None);
    assert_eq!(revocation_row(&db, &q_id), None);
    assert_eq!(
        ledger_rows(&db),
        3,
        "the two refused ops are stored; only their effect is refused"
    );

    // And this is the whole of it: a device an expelled device merely *named*
    // must still be able to expel a genuinely stolen one.
    revoke(&er, &mut db, &ep, d_id, T0 + 30_000);
    assert!(
        er.is_revoked(db.conn(), &d_id).unwrap(),
        "a gated op must not cost the device it named its own ability to revoke"
    );
    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "and X is still out: naming third parties did not move its own cut"
    );
}

/// **What the discount gives up (1): a third party revoking the sole revoker
/// returns its target to full standing.**
///
/// A known consequence, pinned so it stays deliberate. The discount asks of
/// the *ledger* — has anybody other than V expelled S? — and not of the
/// register, because asking the register is ADR-0041 §Alternatives (h)'s wider
/// form, which reopens decision 1's hole with the arrow reversed.
///
/// The price of asking the ledger is here. O expels X; P then expels O. Two
/// things follow, and only the second is new. Decision 1 already unwound O's
/// revocation of X, because a revoked device's revocations are not believed
/// whatever date they carry — so X was already off the revoked list before the
/// discount existed. What the discount adds is that X is no longer *gated*
/// either: P's row discounts O out of X's revoker set, and X revokes third
/// parties again.
///
/// It costs two revocations in one chain rather than the one ordinary op the
/// defect above cost, and the device doing the second of them is by
/// construction not the attacker.
/// [#241](https://github.com/justin13888/Sunrise/issues/241)'s un-revoke is
/// what would let the account say which of the two readings it meant.
#[test]
fn the_discount_rehabilitates_a_device_whose_sole_revoker_a_third_party_revokes() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let t_id = et.keychain.device_id();

    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());

    // P expels O, and X falls off the revoked list with it. That half is
    // decision 1 and predates the discount entirely.
    revoke(&er, &mut db, &ep, o_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &o_id).unwrap());
    assert_eq!(
        revocation_row(&db, &x_id),
        None,
        "a revoked device's revocations stop being believed"
    );

    // This half is the discount's: X is ungated as well as unrevoked.
    revoke(&er, &mut db, &ex, t_id, T0 + 20_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "discounting O out of X's set hands X back third-party revocation"
    );
}

/// **What the discount gives up (1a): one attacker holding two revoked
/// devices reaches shape (1) with a single op, and the "third party" is its
/// own second device.**
///
/// The sharpest reading of the condition above, and the one a threat model
/// has to carry, because "a third party revokes O" sounds like a bystander
/// and nothing requires it to be one. O revokes both X1 and X2 — one
/// administrative act against a pair of devices one person holds. X1 then
/// revokes O. The mutual exception lands both of those, so X1 and O are out;
/// and O being out unwinds O's *other* revocation, so X2 comes off the list.
/// That half is decision 1's retroactivity and predates the discount, which
/// is why the register up to here is what it was before the discount existed.
///
/// What the discount adds is the assertion after it: X1's row discounts O out
/// of X2's revoker set, because X1 is a sender other than X2 — so X2 is
/// ungated as well as unrevoked, and revokes the rest of the account.
///
/// The remedy is the mutual pair's remedy and no better: a device X2 reaches
/// may revoke X2 back, which re-gates X2 and leaves that device revoked as
/// well, because the exception lands X2's op too. What the account needs is
/// still a current device the attacker never reached.
///
/// Pinned separately from the general statement of shape (1) because the cost
/// is one op from a device the account has already expelled, and the
/// precondition — two devices revoked by a single revoker — is an ordinary
/// thing for an account to do.
#[test]
fn the_discount_lets_one_of_two_devices_revoked_together_ungate_the_other() {
    let ex1 = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ex2 = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x1_id, x2_id) = (ex1.keychain.device_id(), ex2.keychain.device_id());
    let (o_id, t_id) = (eo.keychain.device_id(), et.keychain.device_id());

    // O expels both of the attacker's devices.
    revoke(&er, &mut db, &eo, x1_id, T0);
    revoke(&er, &mut db, &eo, x2_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &x1_id).unwrap());
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());

    // One op from X1. The register half of this predates the discount: the
    // mutual exception puts O out, and O being out unwinds its revocation of
    // X2.
    revoke(&er, &mut db, &ex1, o_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &o_id).unwrap());
    assert!(er.is_revoked(db.conn(), &x1_id).unwrap());
    assert_eq!(
        revocation_row(&db, &x2_id),
        None,
        "O's other revocation is unwound with O, which is decision 1"
    );

    // And this is the discount's half: X2 is ungated too, so the attacker's
    // surviving device now revokes the account.
    revoke(&er, &mut db, &ex2, t_id, T0 + 30_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "X1's row discounts O out of X2's set, so X2 revokes third parties"
    );

    // What is left is the mutual pair again, with the same cost: T revokes X2
    // back and re-gates it, and stays revoked itself.
    revoke(&er, &mut db, &et, x2_id, T0 + 40_000);
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "and T stays out: the pair converges on both revocations"
    );
    revoke(&er, &mut db, &ex2, o_id, T0 + 50_000);
    assert_eq!(
        revocation_row(&db, &o_id).map(|r| r.2),
        Some(x1_id.to_vec()),
        "X2 is gated again, so its second attempt on O does not move the row"
    );
}

/// **What the discount gives up (2): a device that is still on the revoked
/// list revokes third parties, when a chain revokes its revoker's revoker.**
///
/// The sharper shape of the consequence above, and the one that is genuinely a
/// hole rather than a re-reading: here X stays revoked *and* is ungated, which
/// is the pair of facts the gate exists to keep apart.
///
/// It needs three revocations arranged in a chain — O expels X, P expels O, Q
/// expels P — and it is the chain that does it. Q's row discounts P out of O's
/// set, so O's revocation of X lands and X is revoked; P's row discounts O out
/// of X's set, so X is ungated. Neither discount can be written by X, because
/// a device authors only rows whose sender is itself and every discount of S
/// from V's set needs a row from a sender that is not V. So X cannot reach
/// this state alone — though "not alone" is a weaker bound than it sounds,
/// and `the_discount_lets_one_of_two_devices_revoked_together_ungate_the_other`
/// is where that is asserted.
///
/// Recorded in ADR-0041 §"What a user sees" item 4 with the bound that
/// replaces the one the poisoning defect refuted.
#[test]
fn the_discount_leaves_a_revoked_device_revoking_when_a_chain_revokes_its_revoker() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eq = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let (p_id, t_id) = (ep.keychain.device_id(), et.keychain.device_id());

    revoke(&er, &mut db, &eo, x_id, T0);
    revoke(&er, &mut db, &ep, o_id, T0 + 10_000);
    revoke(&er, &mut db, &eq, p_id, T0 + 20_000);

    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "Q's row gates P's, so O's revocation of X stands and X is revoked"
    );
    assert!(er.is_revoked(db.conn(), &p_id).unwrap());
    assert_eq!(
        revocation_row(&db, &o_id),
        None,
        "and O is not revoked, because the device that expelled it was expelled"
    );

    revoke(&er, &mut db, &ex, t_id, T0 + 30_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "the residual: X is on the revoked list and revokes a third party anyway"
    );
}

/// **A revocation that stops being believed says so.**
///
/// The one outcome of this design a user could be surprised by: a device that
/// was on the revoked list is on it no longer, because the fold learned that
/// whoever revoked it had itself been revoked. Judging the sender over the
/// whole ledger rather than over a prefix makes that routine rather than
/// exotic — it now happens whenever a revocation arrives *after* an op its
/// author back-dated — so the branch is worth an assertion rather than a
/// reading of the source.
///
/// It is a log line and not a returned value because the register it is about
/// is not the one the caller asked to change: `revoke_device` reports on its
/// own op through `CommandResult`, and the device list is what shows the rest.
/// `core.device.revocation_unwound` is catalogued in
/// `docs/10-cross-cutting/log-events.md`.
#[test]
fn a_revocation_the_fold_stops_believing_is_announced() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // A revokes C, and this replica believes it.
    revoke(&er, &mut db, &ea, c_id, T0);
    assert!(er.is_revoked(db.conn(), &c_id).unwrap());

    // Then it learns A was itself revoked, and C comes back off the list.
    let events = events_emitted_by(|| revoke(&er, &mut db, &eb, a_id, T0 + 60_000));

    assert_eq!(revocation_row(&db, &c_id), None);
    assert!(
        events
            .iter()
            .any(|ev| ev == "core.device.revocation_unwound"),
        "a device quietly leaving the revoked list is the one surprise this \
         design owes a user; got {events:?}"
    );
}

/// A revoked device's ops still **apply** at a receiving replica, and its
/// cursor still advances.
///
/// This half is deliberately not a gate, and there is now a write bound
/// elsewhere for it to defer to. `Command::RevokeDevice` records a
/// `relay_revocation_intents` row in the same transaction as the op and the
/// sync driver drains it into `DELETE /api/v1/devices/{device_id}`, after which
/// the relay refuses the device's uploads and tears down its live session
/// ([#80](https://github.com/justin13888/Sunrise/issues/80), closed). That
/// bound is where an *entity* write is stopped. The gate ADR-0041 adds here is
/// over control ops only, and this test is the boundary of it: a task the
/// revoked device wrote still applies.
///
/// Refusing here instead is a live hazard, and six review rounds produced
/// six defects that were all this shape. Refusing a device's ops freezes
/// its sync cursor while the relay keeps accepting its uploads, and
/// retention then latches a permanent data-loss warning on every peer.
/// Refusing at apply time is also not convergent: a replica that applied an
/// op before the revocation arrived cannot un-apply it, and there is no
/// projection rebuild in this engine to make it, so two replicas with the
/// same op set would disagree forever. Because nothing refuses, they do
/// not: delivery order is not an input to the materialized state, and a
/// cut correction is lossless. Decided in ADR-0034
/// (`docs/11-adr/0034-revocation-bounds-reads-not-writes.md`), which closed
/// #78; #82 is where a convergent form belongs, after the relay bound, and
/// ADR-0041 (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md`) is that
/// form — scoped to the control ops whose effect can be re-derived, which a
/// task edit's cannot.
///
/// What *is* enforced here is reads, and that is the test below.
#[test]
fn a_revoked_devices_ops_still_apply_at_the_replica() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    // A is revoked before it writes anything, at a cut before every op
    // below — the strongest form of the premise.
    revoke(&eb, &mut dbb, &eb, ea.keychain.device_id(), T0);
    assert!(eb.is_revoked(dbb.conn(), &ea.keychain.device_id()).unwrap());

    set_clock(&ca, T0 + 10_000);
    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "written after the cut".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    // B is given the vault-meta key, which is what lets it read control
    // ops at all; every Stream key below still has to arrive by envelope.
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);

    // Its Stream keys are absorbed, so the op it sealed can be opened.
    for env in key_envelope_envs(&dba) {
        eb.apply_remote_all(&mut dbb, &env)
            .expect("a revoked device's key envelope applies");
    }
    assert!(
        !eb.keychain
            .stream_keys_at(&sunrise_domain::INBOX_STREAM_BYTES, 1)
            .is_empty(),
        "the key it distributed was absorbed"
    );

    // And the op itself is applied, materialized, and passed by the cursor.
    let env = env_bytes(&dba, &task.op_id);
    let head = sunrise_cbor::decode_envelope_header(&env).unwrap();
    eb.apply_remote(&mut dbb, &env)
        .expect("a revoked device's op applies")
        .expect("and produces its event");
    assert_eq!(
        read_task_t(&eb, &dbb, task.entity).title,
        "written after the cut"
    );
    assert_eq!(
        cursor_for(&dbb, &head.stream_id, &head.device_id),
        1,
        "the cursor advances, so nothing stalls and nothing replays forever"
    );
}

/// The refusal that survives on the apply path still counts the op it refused.
///
/// `apply_control_op` refuses a `device_revoke` that names its own sender —
/// `core.device.revoke_refused`, `reason = "self"` — because the register is
/// last-writer-wins and a device that can rewrite its own row can undo
/// somebody else's revocation of it. What it refuses is the **register
/// write**, not the delivery: the op row went in at `apply_remote_all`'s
/// idempotence gate before the control op was dispatched, so
/// `upsert_sync_cursor` still runs afterwards with that row in the log. The
/// delivery here is seq 1, so the contiguous prefix is that one op and the
/// stored number becomes 1.
/// `a_self_refused_revoke_out_of_order_leaves_the_cursor_short` is the same
/// refusal with a gap below it, where the number does not move at all — the
/// refusal is not what decides it either way.
///
/// Two further refusals live in the revocation machinery and neither is on
/// this path: `Engine::revoke_device` refuses a self-revocation, and refuses
/// a target with no `devices` row. Both are local command guards that never
/// produce an op, so neither is what a reader arrives here holding.
///
/// That is the opposite of what `upsert_sync_cursor`'s doc claimed until
/// [#253](https://github.com/justin13888/Sunrise/issues/253) — that a refusal
/// leaves the cursor where it is and the op applies if it is resent. Only
/// prose ever said so, which is how four other copies of the same claim
/// reached #240's review uncorrected and this one outlived them, so this
/// holds both halves: the register is untouched and the cursor advances.
///
/// `a_device_cannot_move_its_own_revocation_cut` covers the register half
/// through `apply_control_op` directly. It cannot see this one, because that
/// path has no envelope, no op row and therefore no cursor.
#[test]
fn a_self_refused_revoke_still_advances_the_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();

    // B has admitted A, so `apply_remote_all`'s step b finds the sender's row
    // and the delivery is an ordinary one rather than a rejected stranger.
    trust(&eb, &mut dbb, &ea);

    // A seals a `device_revoke` naming **itself**. `Command::RevokeDevice`
    // refuses that locally for an unrelated reason — rotating every key away
    // from the only device holding them is not recoverable — so the op is
    // built at the op-log seam instead, which is all a peer running older or
    // hostile code has to do.
    let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
        revoked_device_id: a_id,
        reason_code: RevokeReason::Lost,
    }))
    .expect("encode the inner op");
    let (seq, env) = dba
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            // The epoch first, then the seq: minting can emit `key_envelope`
            // ops into this very stream, and each would spend a seq a number
            // read beforehand had already claimed.
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            let seq = ea.next_seq_tx(tx, &META_STREAM)?;
            let env = ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under A's own live meta epoch");
            Ok((seq, env))
        })
        .unwrap();
    assert_eq!(
        seq, 1,
        "the cursor is the contiguous prefix from seq 1, so this op has to be it"
    );

    // Captured, because a paragraph in `upsert_sync_cursor`'s doc rests on this
    // event by name and names both of its emitters. Asserting only the
    // absent register row would leave the log line deletable with nothing red.
    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(eb.apply_remote_all(&mut dbb, &env));
    });
    let events = delivered
        .expect("the capture ran the delivery")
        .expect("a self-naming revoke is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");
    assert!(
        logged.iter().any(|ev| ev == "core.device.revoke_refused"),
        "the refusal is announced, not silent: replace the `tracing::warn!` in \
         `apply_device_revoke` with nothing and this is what fails; got {logged:?}"
    );

    // The register refused it.
    assert_eq!(
        revocation_row(&dbb, &a_id),
        None,
        "a device must not be able to write the register entry about itself"
    );
    assert!(
        !eb.is_revoked(dbb.conn(), &a_id).unwrap(),
        "and it is not revoked by its own op"
    );

    // And the cursor advanced past it regardless, which is the half nothing
    // else in this suite holds.
    assert_eq!(
        cursor_for(&dbb, &META_STREAM, &a_id),
        seq,
        "the refusal is the register's, not the delivery's: the op row is in, \
         so the cursor covers it"
    );

    // Resending recovers nothing, and the arm is never entered a second time.
    // `remote_op_id` is derived from `(stream_id, device_id, seq)`, so the
    // same coordinates carry the same op-log primary key whatever the payload
    // says: the insert collides, `tx.changes()` is 0, and `apply_remote_all`
    // returns before `apply_control_op` is reached at all.
    //
    // So the resend carries a **different** inner op under those same
    // coordinates -- a revoke naming a third device, which the self guard
    // would not refuse and which would write a register row the instant the
    // arm ran. No such row is the observation that the arm was not entered.
    // Resending the identical bytes and asserting nothing changed cannot make
    // it: an idempotent second run of the refusal looks exactly the same.
    let third = [9u8; 16];
    let other = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
        revoked_device_id: third,
        reason_code: RevokeReason::Lost,
    }))
    .expect("encode a different inner op");
    let resend = dba
        .with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            Ok(ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    ea.hlc.send(),
                    &other,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal a different payload at the same seq"))
        })
        .unwrap();
    assert!(
        eb.apply_remote_all(&mut dbb, &resend)
            .expect("a resend is not an error")
            .is_empty(),
        "the second delivery is not re-evaluated"
    );
    assert_eq!(
        revocation_row(&dbb, &third),
        None,
        "the idempotence gate returned before `apply_control_op`, so the arm never \
         saw the payload the resend carried"
    );
    assert_eq!(
        revocation_row(&dbb, &a_id),
        None,
        "and a resend does not get the register a second look"
    );
    assert_eq!(
        cursor_for(&dbb, &META_STREAM, &a_id),
        seq,
        "and it moves nothing"
    );
}

/// **A read-bounded sender's third-party recipient claim is refused, and the
/// cursor counts the op anyway.**
///
/// This is the delivery half of the gate at
/// `crates/sunrise-core/src/engine/sync.rs:741#apply_control_op`. The two
/// units that reach that gate today —
/// `a_revoked_devices_third_party_envelope_claim_is_not_recorded` and
/// `an_unwound_devices_third_party_envelope_claim_is_not_recorded` — call
/// `apply_control_op` directly inside a test transaction, so there is no
/// envelope, no op row and no cursor, and the only thing either can observe is
/// the absent hint row.
///
/// `upsert_sync_cursor`'s doc names this site in its enumeration of the
/// refusals that survive, and what it claims there is exactly the half a
/// direct call cannot see: this refusal declines a `key_envelope_recipients`
/// hint row and nothing else. The op row went in at the idempotence gate
/// before the control op was dispatched, so it stands, and `upsert_sync_cursor`
/// runs afterwards at step g and counts it. The delivery is seq 1, so the
/// contiguous prefix is that one op and the stored number becomes 1.
///
/// All three are asserted, because no two of them constrain the third. An
/// absent hint row is also what a delivery that never arrived looks like; a
/// cursor at 1 is also what an *accepted* claim writes. The refusal's own log
/// line is observed rather than assumed, for the reason
/// `a_self_refused_revoke_still_advances_the_cursor` gives: asserting only the
/// absent row would leave the `tracing::warn!` deletable with nothing red.
#[test]
fn a_read_bounded_senders_recipient_claim_is_refused_and_the_cursor_counts_the_op() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let victim = [0x77u8; 16];

    // B has admitted A, so `apply_remote_all`'s step b finds the sender's row
    // and the delivery is an ordinary one rather than a rejected stranger.
    trust(&eb, &mut dbb, &ea);

    // C retires A at B. The gate reads `device_read_bounds` and not the
    // register, so the premise is asserted on the table the gate reads.
    revoke(&eb, &mut dbb, &ec, a_id, T0);
    assert!(
        eb.is_read_bounded(dbb.conn(), &a_id).unwrap(),
        "the gate asks `is_read_bounded`, so that is what has to hold going in"
    );

    // A claims a third device already holds the key at (vault-meta, 1). That
    // is the withhold the gate exists to refuse: a device that can file these
    // rows and cannot read what they suppress.
    let inner = encode_inner_op(&InnerOp::KeyEnvelope(KeyEnvelopePayload {
        stream_id: META_STREAM,
        epoch: 1,
        recipient: Recipient::Device(victim),
        key_id: [0u8; 8],
        hpke_ciphertext: vec![0u8; 48],
    }))
    .expect("encode the inner op");
    let (seq, env) = dba
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            // The epoch first, then the seq, for the reason
            // `a_self_refused_revoke_still_advances_the_cursor` states.
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            let seq = ea.next_seq_tx(tx, &META_STREAM)?;
            let env = ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under A's own live meta epoch");
            Ok((seq, env))
        })
        .unwrap();
    assert_eq!(
        seq, 1,
        "the cursor is the contiguous prefix from seq 1, so this op has to be it"
    );

    let filed = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&META_STREAM[..], 1u32, &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(filed(&dbb), 0, "nothing has filed a hint row yet");

    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(eb.apply_remote_all(&mut dbb, &env));
    });
    let events = delivered
        .expect("the capture ran the delivery")
        .expect("a refused recipient claim is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");
    assert!(
        logged
            .iter()
            .any(|ev| ev == "core.key.recipient_claim_refused"),
        "the refusal is announced, not silent: replace the `tracing::warn!` at \
         the `is_read_bounded` gate with nothing and this is what fails; got \
         {logged:?}"
    );

    // What the refusal took: the hint row, and only the hint row.
    assert_eq!(
        filed(&dbb),
        0,
        "a read-bounded device must not be able to say a key has already been \
         delivered"
    );

    // What it left behind: the op row, in at the idempotence gate before the
    // control arm ever ran.
    assert_eq!(
        ops_at(dbb.conn(), &META_STREAM, &a_id, seq),
        1,
        "the refusal is the hint table's, not the delivery's: the op row is in"
    );

    // And the cursor counted it, which is the claim
    // `upsert_sync_cursor`'s doc makes about this site.
    assert_eq!(
        cursor_for(&dbb, &META_STREAM, &a_id),
        seq,
        "`upsert_sync_cursor` runs at step g with that row in the log, so the \
         contiguous prefix covers it"
    );
}

/// The apply path reads the **read bound**, and what that read decides is
/// keys — not whether the delivery applies.
///
/// This walks the trace `upsert_sync_cursor`'s doc describes, as a delivery
/// and end to end: `apply_remote_all` dispatches the control op into
/// `apply_control_op`, whose `DeviceCertPublish` arm calls
/// `backfill_key_envelopes`, which asks `is_read_bounded` over
/// `device_read_bounds` and returns before it seals anything.
///
/// It is **not** the revocation register, and since ADR-0041 the difference
/// is the point: the register is a fold that can hand a device back, and
/// `device_read_bounds` only ever grows. Revoking still writes both, so the
/// premise here is set by the ordinary command and then asserted on the
/// table the gate actually reads.
///
/// Nothing else in this suite walks that trace. Every other
/// `InnerOp::DeviceCertPublish` here — the `trust` and `trust_at` helpers,
/// and the impostor-cert cases — calls `apply_control_op` directly inside a
/// test transaction, so there is no envelope, no op row and no cursor, and
/// the reachability of the bound check from a remote delivery goes unpinned.
/// That gap is how the doc came to describe this trace against the wrong
/// table for a whole release: nothing disagreed.
///
/// So this holds all three halves. The read-bounded device is sealed nothing,
/// which is the security-relevant end of ADR-0034's "revocation bounds
/// reads". Its cert is recorded and this replica's cursor covers the delivery
/// anyway, which is the "not writes" end. And an unbounded device taking the
/// identical route **is** sealed, without which the first assertion would
/// hold for the trivial reason that this vault had no key to seal to anyone.
#[test]
fn a_revoked_device_cert_through_apply_remote_seals_no_keys() {
    // Real random Stream keys, so `stream_keys` holds rows and
    // `Keychain::held_epochs_tx` — the set a backfill walks — is not empty.
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    let c_id = ec.keychain.device_id();

    // A mints the vault-meta and Inbox epochs, so a backfill has keys to seal.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before either peer was known".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(
        held.len() >= 2,
        "the capture minted at least the meta and Inbox epochs, or there is nothing \
         a backfill could seal and the assertions below would hold vacuously"
    );

    // A revokes B. B is a device A has never seen, which is the ordinary case
    // rather than a contrived one: a cert and a revocation naming it travel in
    // the same stream with no ordering guarantee between them.
    revoke(&ea, &mut dba, &ea, b_id, T0);
    assert!(
        ea.is_read_bounded(dba.conn(), &b_id).unwrap(),
        "the premise, on the table the gate reads: revoking B bounded its reads \
         before the delivery below"
    );

    // The meta epoch A is on. A peer seals under the key a `key_envelope`
    // handed it; reading the same bytes out of A is that key, and keeps the
    // delivery about the register rather than about key distribution.
    let (meta_epoch, meta_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("A's live vault-meta epoch");

    // Seal a `device_cert_publish` the way a peer does and hand the bytes to
    // A. The op is self-authenticating, so A needs no prior row for the
    // sender — which is the whole point of the arm this reaches.
    let publish = |e: &Engine, db: &mut Db| -> (u64, Vec<u8>) {
        let inner = encode_inner_op(&InnerOp::DeviceCertPublish(e.keychain.cert_blob()))
            .expect("encode the cert publish");
        db.with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = e.next_seq_tx(tx, &META_STREAM)?;
            let env = e
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    e.hlc.send(),
                    &inner,
                    e.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch A holds");
            Ok((seq, env))
        })
        .unwrap()
    };

    let (b_seq, b_env) = publish(&eb, &mut dbb);
    let events = ea
        .apply_remote_all(&mut dba, &b_env)
        .expect("a revoked device's cert is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");

    // The bound was consulted, and it stopped the backfill.
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a read-bounded device republished its cert through a delivery and was handed \
         the keys the revocation had just rotated away"
    );

    // It did not stop the op. The cert is recorded and the cursor covers it —
    // step b found the sender like any other, and the early return inside
    // `backfill_key_envelopes` leaves both alone.
    let recorded: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        recorded, 1,
        "the backfill is refused, the delivery is not: the cert belongs in `devices` \
         whatever the bound says"
    );
    assert_eq!(
        cursor_for(&dba, &META_STREAM, &b_id),
        b_seq,
        "and the cursor advances over it like any other applied op"
    );

    // The control. An unrevoked device down the identical route is sealed, so
    // the emptiness above is the register's answer and not this vault having
    // nothing to give.
    let (_, c_env) = publish(&ec, &mut dbc);
    ea.apply_remote_all(&mut dba, &c_env)
        .expect("an unrevoked device's cert is a well-formed delivery too");
    let mut sealed = envelopes_to(&ea, &dba, &c_id);
    sealed.sort_unstable();
    let mut want = held;
    want.sort_unstable();
    assert_eq!(
        sealed, want,
        "the same delivery route seals every epoch A holds to an unbounded device, \
         which is what makes B's empty list the bound's doing"
    );
}

/// The same refusal, delivered with a gap below it: the cursor does not move.
///
/// `a_self_refused_revoke_still_advances_the_cursor` delivers the refused op
/// at seq 1 and asserts the cursor becomes 1. That is true, and it says
/// nothing about refusals — at seq 1 the op *is* the whole contiguous prefix,
/// so the assertion holds for every op that reaches the log. This is the case
/// that separates the two. B holds nothing from A, A's self-naming
/// `device_revoke` arrives stamped seq 2, and `ops_run_end` starts its run at
/// seq 1: with seq 1 absent the `ELSE ?3 - 1` arm returns 0, so the row
/// `upsert_sync_cursor` writes says 0 and the refused op sits outside it.
///
/// So "a refused op advances the cursor past it" is not a fact about
/// refusals. The control at the end delivers an **accepted** op through the
/// same gap and gets the same 0: the number is decided by the seqs below and
/// never by the answer the control op got. Without this, both the prose in
/// `upsert_sync_cursor` and the assertion in the test above read as promising
/// that a delivery moves the cursor, which is the shape of claim
/// [#253](https://github.com/justin13888/Sunrise/issues/253) was opened about.
#[test]
fn a_self_refused_revoke_out_of_order_leaves_the_cursor_short() {
    // The frame carrying seq 1 is the one that was dropped. What the receiver
    // acts on is the seq in the envelope, so the sender's own log is beside
    // the point and the op is sealed at 2 directly.
    const GAPPED_SEQ: u64 = 2;

    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let c_id = ec.keychain.device_id();

    trust(&eb, &mut dbb, &ea);

    let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
        revoked_device_id: a_id,
        reason_code: RevokeReason::Lost,
    }))
    .expect("encode the inner op");
    let env = dba
        .with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            Ok(ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    GAPPED_SEQ,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under A's own live meta epoch"))
        })
        .unwrap();

    let events = eb
        .apply_remote_all(&mut dbb, &env)
        .expect("a self-naming revoke with a gap below it is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");

    // The delivery applied: the refusal is the register's, not the op's.
    assert_eq!(
        ops_at(dbb.conn(), &META_STREAM, &a_id, GAPPED_SEQ),
        1,
        "the op row is in the log, which is the premise the cursor claim is made from"
    );
    assert_eq!(
        revocation_row(&dbb, &a_id),
        None,
        "a device must not be able to write the register entry about itself"
    );

    // And the cursor says 0, not 2. `cursor_for` reports 0 for a missing row
    // too, so the row itself is read: one *was* written, and it says 0.
    assert_eq!(
        cursor_row(&dbb, &META_STREAM, &a_id),
        Some(0),
        "seq 1 never arrived, so the contiguous prefix from 1 is empty and the cursor \
         is written below the op that was just delivered"
    );

    // The control. An accepted op through the identical gap writes the same 0,
    // so the 0 above is the gap's doing and not the refusal's. A cert publish
    // is self-authenticating, so C needs no prior row here either.
    let cert = encode_inner_op(&InnerOp::DeviceCertPublish(ec.keychain.cert_blob()))
        .expect("encode the cert publish");
    let cert_env = dbc
        .with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            let (epoch, key) = ec.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            Ok(ec
                .keychain
                .seal_op_at(
                    META_STREAM,
                    GAPPED_SEQ,
                    ec.hlc.send(),
                    &cert,
                    ec.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under C's own live meta epoch"))
        })
        .unwrap();
    eb.apply_remote_all(&mut dbb, &cert_env)
        .expect("an accepted op with a gap below it is a well-formed delivery too");
    assert_eq!(
        ops_at(dbb.conn(), &META_STREAM, &c_id, GAPPED_SEQ),
        1,
        "the control's op row is in the log as well"
    );
    assert_eq!(
        cursor_row(&dbb, &META_STREAM, &c_id),
        Some(0),
        "an accepted op through the same gap writes the same 0: what the cursor reports \
         is the run below the op, never the answer the op got"
    );
}

/// A backfill that fails is logged, not raised — and the delivery still lands.
///
/// `upsert_sync_cursor`'s doc says the cursor is reached on every answer the
/// register read can give, and names the failing backfill as one of them.
/// Nothing provoked one. The `DeviceCertPublish` arm swallows the `Err` into
/// `core.device.backfill_failed` deliberately, because the cert is valid and
/// belongs in `devices` whatever happens next, and that arm's own comment
/// says so — but a build that returned the error instead would fail the whole
/// delivery, roll the op row back, and leave the relay resending an op this
/// replica had already decided. Every other unit here goes down the success
/// path or the revoked early return, so nothing disagreed with the change.
///
/// The fault is a trigger on the one table `backfill_key_envelopes` writes,
/// which is what makes it a *storage* failure and not a rearranged input:
/// the arm reaches the backfill exactly as it does in production.
#[test]
fn a_failed_backfill_is_logged_and_the_cert_delivery_still_applies() {
    // Real random Stream keys, so `held_epochs_tx` is non-empty and the
    // backfill has something to try to record.
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before the peer was known".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(
        held.len() >= 2,
        "the capture minted at least the meta and Inbox epochs, or the backfill below \
         has nothing to fail at"
    );

    let (meta_epoch, meta_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("A's live vault-meta epoch");

    // The fault. `record_envelope_recipient` is the last statement of each
    // backfill iteration, so the loop gets as far as sealing and emitting and
    // then fails on storage — the shape the log event exists for.
    dba.conn()
        .execute_batch(
            "CREATE TEMP TRIGGER no_recipient_rows
               BEFORE INSERT ON main.key_envelope_recipients
               BEGIN SELECT RAISE(ABORT, 'key_envelope_recipients is unavailable'); END;",
        )
        .expect("install the storage fault");

    let inner = encode_inner_op(&InnerOp::DeviceCertPublish(eb.keychain.cert_blob()))
        .expect("encode the cert publish");
    let (b_seq, b_env) = dbb
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = eb.next_seq_tx(tx, &META_STREAM)?;
            let env = eb
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    eb.hlc.send(),
                    &inner,
                    eb.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch A holds");
            Ok((seq, env))
        })
        .unwrap();

    // Captured, so the name of this unit is a claim it proves. Without it the
    // `tracing::warn!` in the `DeviceCertPublish` arm could be deleted and
    // every assertion below would still pass — the partial seal, the recorded
    // cert and the cursor are all true of a backfill that failed silently.
    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(ea.apply_remote_all(&mut dba, &b_env));
    });
    let events = delivered
        .expect("the capture ran the delivery")
        .expect("a backfill error is logged, not raised: the delivery is not failed by it");
    assert!(events.is_empty(), "a control op materializes no entity");
    assert!(
        logged.iter().any(|ev| ev == "core.device.backfill_failed"),
        "the arm swallows the `Err` into a log line, and the log line is the whole \
         of what the operator gets; got {logged:?}"
    );

    // The backfill did fail, and part-way: it emitted for the epoch it was on
    // and never reached the rest. Were this list complete, the trigger had not
    // fired and the test would be asserting nothing.
    let sealed = envelopes_to(&ea, &dba, &b_id);
    assert!(
        !sealed.is_empty() && sealed.len() < held.len(),
        "the backfill stopped at the first epoch it tried to record, which is the \
         partial state the error is logged about"
    );

    // And the delivery stands: the cert is recorded and the cursor covers it.
    let recorded: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        recorded, 1,
        "the cert belongs in `devices` whatever the backfill did"
    );
    assert_eq!(
        cursor_for(&dba, &META_STREAM, &b_id),
        b_seq,
        "and the cursor is reached on this answer like any other"
    );
}

/// A local emit that fails leaves the cursor exactly where it was, by either
/// of the two routes it can fail.
///
/// `ops_insert_at` seals, inserts into `ops`, enqueues the outbox row and only
/// then calls `upsert_sync_cursor`. Nothing held what happens when the first
/// of those does not land: the `Err` arm on `OpLog::insert` had no test at
/// all, and neither did the collision that arm cannot see.
///
/// **The collision.** `OpLog::insert` is `INSERT OR IGNORE`, so an op at a
/// `seq` this device has already written is dropped without a word and
/// without an error — the failure `emit_control_op`'s doc calls "the op would
/// be ignored, and its outbox row would then fail its foreign key, which is
/// exactly how this was found". The `Err` arm never runs. What turns the
/// silence into an error is `outbox.op_id REFERENCES ops (op_id)`, one table
/// away, so this half also pins that foreign key: drop it and a spent seq
/// becomes a silent no-op that still advertises a cursor.
///
/// **The storage failure.** A fault at the `ops` insert itself, which is the
/// arm that maps `OpLogError` and returns. Here the outbox is never reached
/// at all.
///
/// Both halves read **inside the transaction**, before the rollback `with_tx`
/// performs. Looking afterwards would pass on the rollback alone. And the
/// cursor is held twice over in each: `upsert_sync_cursor` is not reached,
/// and could not have moved the number if it were, because it recomputes the
/// prefix out of `ops` rather than taking it off the op being written — which
/// is precisely what the high-water mark it replaced did do.
#[test]
fn a_failed_local_emit_leaves_the_cursor_where_it_was() {
    let e = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let self_id = e.keychain.device_id();

    // One op through the same seam first, so there is a stored cursor to leave
    // alone rather than an absent row that would prove nothing, and a spent
    // seq for the first half to collide with.
    let cert = InnerOp::DeviceCertPublish(e.keychain.cert_blob());
    db.with_tx(|tx| e.emit_control_op(tx, &cert, T0, None))
        .expect("the first emit goes through");
    let spent = cursor_for(&db, &META_STREAM, &self_id);
    assert!(
        spent >= 1,
        "the emit above put at least one op in this device's own log"
    );
    let queued_before = outbox_rows(&db);
    let inner = encode_inner_op(&cert).expect("encode the cert publish");

    // --- the collision ---
    let collided = db.with_tx(|tx| -> rusqlite::Result<()> {
        let (epoch, key) = e.ensure_stream_epoch(tx, &META_STREAM, T0)?;
        let err = e
            .ops_insert_at(
                tx,
                &e.fresh_op_id(T0),
                &META_STREAM,
                spent,
                e.hlc.send(),
                &inner,
                cert.inner_kind(),
                cert.target_kind(),
                None,
                Some(T0),
                None,
                T0,
                &[],
                epoch,
                &key,
            )
            .expect_err("a spent seq does not reach the caller as a success");

        assert_eq!(
            ops_at(tx, &META_STREAM, &self_id, spent),
            1,
            "`INSERT OR IGNORE` kept the op already at this seq and dropped the new one"
        );
        assert_eq!(
            outbox_count(tx),
            queued_before,
            "and the foreign key rejected the outbox row for the op that was dropped, \
             which is the only thing that made the drop visible at all"
        );
        assert_eq!(
            cursor_in_tx(tx, &META_STREAM, &self_id),
            i64::try_from(spent).unwrap(),
            "so `upsert_sync_cursor` was never reached and the cursor still reports the \
             run the log actually holds"
        );
        Err(err)
    });
    assert!(
        collided.is_err(),
        "the collision reaches the caller rather than being swallowed"
    );

    // --- the storage failure ---
    let faulted = db.with_tx(|tx| -> rusqlite::Result<()> {
        // A fault at the one statement `OpLog::insert` runs, and at nothing
        // before it, so what fails is the insert and not the seal.
        tx.execute_batch(
            "CREATE TEMP TRIGGER no_op_rows
               BEFORE INSERT ON main.ops
               BEGIN SELECT RAISE(ABORT, 'ops is unavailable'); END;",
        )?;
        let err = e
            .emit_control_op(tx, &cert, T0, None)
            .expect_err("a failed op-log insert is returned, not swallowed");

        assert_eq!(
            outbox_count(tx),
            queued_before,
            "the `Err` arm returned before `Outbox::enqueue`, so this route does not even \
             reach the foreign key the other one leans on"
        );
        assert_eq!(
            cursor_in_tx(tx, &META_STREAM, &self_id),
            i64::try_from(spent).unwrap(),
            "and the cursor is untouched here too"
        );
        Err(err)
    });
    assert!(
        faulted.is_err(),
        "the storage failure reaches the caller as well"
    );

    // Both transactions rolled back, which is belt to the braces above.
    assert_eq!(
        cursor_for(&db, &META_STREAM, &self_id),
        spent,
        "and the rollback leaves the cursor where the code path already had it"
    );
    assert_eq!(outbox_rows(&db), queued_before, "the outbox likewise");
}

/// A stranger's cert through `apply_remote_all` is refused at step b, and the
/// refusal costs the cursor nothing because there is no op row yet.
///
/// `upsert_sync_cursor`'s doc says admission is settled at step b — by the
/// `devices` lookup, or for the `DeviceCertPublish` family by
/// `self_authenticating_signer` — and never by the revocation register. Only
/// the *accepting* half of that function was reached through a delivery:
/// `a_revoked_device_cert_through_apply_remote_seals_no_keys` walks an
/// unknown sender whose cert does verify. The refusing half was reached by
/// nothing. The impostor-cert cases next to this one call `apply_control_op`
/// directly, which is past the point this function is consulted at.
///
/// The sender here is a device this vault has never admitted, publishing
/// **another device's** cert. Both halves are deliberate. A sender already in
/// `devices` never reaches this function at all — step b's lookup answers
/// first — and a sender publishing its own genuine cert is admitted, which is
/// the half already covered. What is left is the impostor test: a well-formed
/// cert that names somebody other than the envelope's signer, which is the
/// rebinding `Command::TrustDevice` could not refuse.
///
/// The cursor half is the reason this unit sits here rather than beside the
/// other cert cases. A refusal at step b happens **before** the `with_tx`
/// block, so the op row is never inserted and `upsert_sync_cursor` is never
/// reached — unlike the refusals inside `apply_control_op`, which run after
/// the row is in. The doc's two kinds of refusal are only distinguishable by
/// what they leave behind, and this is the one that leaves nothing.
#[test]
fn a_stranger_cert_through_apply_remote_is_refused_and_writes_no_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // Neither of these is in A's `devices`, so the envelope's sender falls
    // through step b's lookup into `self_authenticating_signer`.
    let stranger = engine_seeded(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let victim = engine_seeded(ROOT, [8u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbs = db_root(ROOT);
    let s_id = stranger.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before the stranger knocked".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // The meta key A holds, handed to the stranger the way a leaked or
    // retained key reaches one. This is what gets the delivery past step d and
    // into the cert check, which is the whole subject.
    let (meta_epoch, meta_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("A's live vault-meta epoch");

    // The cert of a *third* device, published under the stranger's own
    // envelope. Genuine bytes, wrong signer.
    let inner = encode_inner_op(&InnerOp::DeviceCertPublish(victim.keychain.cert_blob()))
        .expect("encode the cert publish");
    let (s_seq, s_env) = dbs
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = stranger.next_seq_tx(tx, &META_STREAM)?;
            let env = stranger
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    stranger.hlc.send(),
                    &inner,
                    stranger.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch A holds");
            Ok((seq, env))
        })
        .unwrap();

    let err = ea
        .apply_remote_all(&mut dba, &s_env)
        .expect_err("a cert naming another device is refused");
    assert!(
        matches!(&err, EngineError::RemoteOpInvalid(m)
            if m.contains("published cert names another device")),
        "refused by the impostor test in `self_authenticating_signer`, which is the \
         half no delivery reached before; got {err:?}"
    );

    // Nothing was admitted.
    let recorded: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&s_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 0, "and the stranger is not in `devices`");

    // And the cursor was never reached: this refusal is the kind that runs
    // before the op row exists, so there is no row for the prefix to count and
    // no cursor row at all.
    assert_eq!(
        ops_at(dba.conn(), &META_STREAM, &s_id, s_seq),
        0,
        "a step-b refusal returns before the `with_tx` block, so no op row was \
         inserted"
    );
    assert_eq!(
        cursor_row(&dba, &META_STREAM, &s_id),
        None,
        "and `upsert_sync_cursor` was never reached, so there is no row — which is \
         what tells this refusal apart from the ones inside `apply_control_op`"
    );
}

/// What the cursor means across a re-fold: nothing the fold does reaches it.
///
/// Since ADR-0041 the revocation register is derived, and it **shrinks**:
/// applying one `device_revoke` can unwind another already believed, and
/// `core.device.revocation_unwound` is a routine event rather than an alarm.
/// `upsert_sync_cursor`'s doc asserts the cursor is a fact about `ops` and
/// never about any judgement passed on one, so the re-fold is the sharpest
/// case that claim has: a delivery that *retracts* a decision this replica
/// had already made.
///
/// Nothing pinned it. Every unwind unit in this suite drives
/// `apply_control_op` directly through the `revoke` helper, so there is no
/// envelope, no op row and no cursor to observe; every cursor unit here
/// delivers something the fold does not disturb.
///
/// So this delivers both revocations as remote ops and holds three things at
/// once. The register did unwind — C is off the list and the event fired.
/// The read bound did not, because it only ever grows, which is the
/// asymmetry `Engine::is_read_bounded` exists for. And both cursors stand
/// exactly where the op rows put them: the fold rewrote `device_revocations`
/// and touched neither `ops` nor `sync_cursors`.
#[test]
fn a_refold_that_unwinds_a_revocation_moves_no_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbr = db_root(ROOT);
    let (a_id, b_id, c_id) = (
        ea.keychain.device_id(),
        eb.keychain.device_id(),
        ec.keychain.device_id(),
    );

    // R has admitted both senders, so step b finds each row and the deliveries
    // are ordinary ones.
    trust(&er, &mut dbr, &ea);
    trust(&er, &mut dbr, &eb);

    // The meta epoch R is on; both peers seal under the key R holds.
    let (meta_epoch, meta_key) = dbr
        .with_tx(|tx| er.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("R's live vault-meta epoch");

    let revoke_env = |e: &Engine, db: &mut Db, target: [u8; 16]| -> (u64, Vec<u8>) {
        let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: target,
            reason_code: RevokeReason::Lost,
        }))
        .expect("encode the revoke");
        db.with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = e.next_seq_tx(tx, &META_STREAM)?;
            let env = e
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    e.hlc.send(),
                    &inner,
                    e.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch R holds");
            Ok((seq, env))
        })
        .unwrap()
    };

    // A revokes C, delivered. R believes it.
    let (a_seq, a_env) = revoke_env(&ea, &mut dba, c_id);
    er.apply_remote_all(&mut dbr, &a_env)
        .expect("a revoke is a well-formed delivery");
    assert!(
        er.is_revoked(dbr.conn(), &c_id).unwrap(),
        "the premise: R believes C is revoked before the second delivery"
    );
    assert_eq!(
        cursor_for(&dbr, &META_STREAM, &a_id),
        a_seq,
        "and the cursor covers A's op"
    );

    // Then B revokes A. The fold discounts every row A wrote, so C comes back
    // off the register — the retraction this unit is about.
    let (b_seq, b_env) = revoke_env(&eb, &mut dbb, a_id);
    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(er.apply_remote_all(&mut dbr, &b_env));
    });
    delivered
        .expect("the capture ran the delivery")
        .expect("revoking the revoker is a well-formed delivery too");

    assert!(
        er.is_revoked(dbr.conn(), &a_id).unwrap(),
        "B's revocation of A stands"
    );
    assert_eq!(
        revocation_row(&dbr, &c_id),
        None,
        "and A's revocation of C was unwound by the fold, which is the premise the \
         rest of this unit needs"
    );
    assert!(
        logged
            .iter()
            .any(|ev| ev == "core.device.revocation_unwound"),
        "a device leaving the revoked list is announced; got {logged:?}"
    );

    // The read bound did not unwind. It is the monotone half, which is why the
    // four key-distribution sites ask it and not the register.
    assert!(
        er.is_read_bounded(dbr.conn(), &c_id).unwrap(),
        "C stays read-bounded however the register folds: `device_read_bounds` has \
         no `DELETE`"
    );

    // And neither cursor moved with the register. This is the answer the doc
    // owed a reader: the cursor is the contiguous prefix of `ops`, the fold
    // rewrites `device_revocations`, and the two never meet.
    assert_eq!(
        cursor_for(&dbr, &META_STREAM, &a_id),
        a_seq,
        "A's op is still in the log and still counted, though the decision it \
         carried has been retracted"
    );
    assert_eq!(
        cursor_for(&dbr, &META_STREAM, &b_id),
        b_seq,
        "and B's op is counted like any other applied op"
    );
}

/// A device certified *after* an epoch was minted still receives that
/// epoch's key.
///
/// This is the gap dropping `ID_D_priv` from `PairingPayload` opens, and it
/// is not a corner: minting happens when a Stream is created, so a Stream
/// made on one device while another device's cert was still in flight would
/// have been unreadable on that device permanently. The identity copy used
/// to absorb this silently.
#[test]
fn a_device_certified_after_an_epoch_was_minted_is_backfilled() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    // A mints the vault-meta and Inbox epochs while it has never heard of
    // B, so B is on no recipient list.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before B was known".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "B cannot have been sealed to before A had its cert"
    );
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(
        held.len() >= 2,
        "the capture minted at least the meta and Inbox epochs"
    );

    // B's cert arrives.
    trust_at(&ea, &mut dba, &eb, T0);

    let mut sealed = envelopes_to(&ea, &dba, &b_id);
    sealed.sort_unstable();
    let mut want = held;
    want.sort_unstable();
    assert_eq!(
        sealed, want,
        "every epoch A holds must be sealed to B once its cert lands"
    );

    // And it is not done twice: a second publication of the same cert
    // emits nothing, because `key_envelope_recipients` already has the row.
    let before = envelopes_to(&ea, &dba, &b_id).len();
    trust_at(&ea, &mut dba, &eb, T0 + 1);
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        before,
        "a re-published cert must not re-send every key"
    );
}

/// A device certified after a **rotation** receives the epoch it missed,
/// not only the live one.
///
/// The gap this closes is [#107](https://github.com/justin13888/Sunrise/issues/107)
/// and it is silent: ops written under the superseded epoch park in
/// `deferred_ops` on the new device and expire at `DEFERRED_TTL_MS`, so it
/// presents to the user as "some items from around when I set up this
/// device never arrived" and to every log as nothing at all.
///
/// The rotation here is `Command::RotateStreamKey`, but the ordinary cause
/// is creating a Stream or revoking a device — both mint — so the race is
/// between a new device's certificate and any of the account's routine
/// key-minting activity, which is not a corner.
#[test]
fn a_device_certified_after_a_rotation_receives_the_epoch_it_missed() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    // Epoch 1 of the Inbox exists and carries a task, all while B is
    // unknown to A.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written under epoch 1".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // The rotation that lands between B's pairing and B's certificate.
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    let live = dba
        .with_tx(|tx| ea.keychain.current_epoch_tx(tx, &INBOX_STREAM_BYTES))
        .unwrap()
        .expect("the Inbox has a key");
    assert!(
        live >= 2,
        "the rotation must have superseded epoch 1, or this test proves nothing"
    );

    trust_at(&ea, &mut dba, &eb, T0);

    let sealed = envelopes_to(&ea, &dba, &b_id);
    assert!(
        sealed.contains(&(INBOX_STREAM_BYTES, live)),
        "the live epoch is sealed to a newly certified device"
    );
    assert!(
        sealed.contains(&(INBOX_STREAM_BYTES, 1)),
        "and so is the superseded epoch, or every op written under it is \
             undecryptable on B and expires out of `deferred_ops` unremarked"
    );

    // Every epoch, not merely the two this assertion names, and not twice.
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    let mut want = held;
    want.sort_unstable();
    let mut got = sealed;
    got.sort_unstable();
    assert_eq!(
        got, want,
        "the backfill emits exactly the epochs this replica holds"
    );

    let before = envelopes_to(&ea, &dba, &b_id).len();
    trust_at(&ea, &mut dba, &eb, T0 + 1);
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        before,
        "`key_envelope_recipients` still makes a re-published cert emit nothing"
    );
}

/// A revoked device that republishes its cert is **not** backfilled.
///
/// Without this the read half would be trivially undone: revocation
/// rotates every key away, and the device then asks for all of them back
/// by re-sending the cert it already had.
#[test]
fn a_revoked_device_is_not_backfilled_by_republishing_its_cert() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    // B is revoked at a cut at or before the moment its cert is applied.
    revoke(&ea, &mut dba, &ea, b_id, T0);
    assert!(ea.is_revoked(dba.conn(), &b_id).unwrap());

    trust_at(&ea, &mut dba, &eb, T0);
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a revoked device must not recover its keys by re-sending its cert"
    );
}

/// **A revoked device cannot rejoin under a fresh device id.** The test
/// that had to flip, flipped.
///
/// [#105](https://github.com/justin13888/Sunrise/issues/105). The setup is
/// unchanged from the version that asserted the bypass, deliberately: same
/// engines, same vault root, same fresh id minted from the `ID_S_priv` the
/// revoked device kept, same publish. Every step the issue describes still
/// happens and still succeeds. The cert still verifies — it is not a
/// forgery and nothing here pretends it is.
///
/// What changed is underneath it. `RevokeDevice` now rotates the account
/// identity as well as the Stream keys, and the revoked device is not in
/// the new roster, so it holds no share of the successor's `ID_S_priv`. The
/// fresh cert it signs is therefore issued under an identity the account
/// has retired: `DeviceCertPublish` records *which* chain identity verified
/// it, that is not the head, and every membership test reads the row as not
/// current. The row lands — applying is unconditional, so every replica
/// agrees it landed — and it is a recipient of nothing.
///
/// ADR-0032 records why the fix is identity rotation and not any of the
/// narrower checks that were considered here.
#[test]
fn a_revoked_device_cannot_rejoin_under_a_fresh_device_id() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // The same account identity, a device id nobody has revoked: what the
    // revoked device produces for itself out of the `ID_S_priv` it kept.
    let ec2 = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let c_id = ec.keychain.device_id();
    let c2_id = ec2.keychain.device_id();

    trust(&ea, &mut dba, &ec);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    let after_the_cut = envelopes_to(&ea, &dba, &c_id).len();

    // The fresh cert verifies under this account's identity: nothing in
    // `DeviceCertPublish` has anything to check a revocation against,
    // because a `DeviceCert` names no issuer.
    trust_at(&ea, &mut dba, &ec2, T0 + 1);

    assert!(
        ea.is_revoked(dba.conn(), &c_id).unwrap(),
        "the register still names the old id"
    );
    assert!(
        !ea.is_revoked(dba.conn(), &c2_id).unwrap(),
        "and has nothing to say about the new one"
    );
    assert_eq!(
        envelopes_to(&ea, &dba, &c_id).len(),
        after_the_cut,
        "the read bound holds for the id it names, which is the whole of what it does"
    );

    // The fresh id is admitted. That is not a concession — it is the
    // property that keeps the fix convergent: facts about devices apply
    // unconditionally on every replica, and only standing is derived.
    let stored: Option<Vec<u8>> = dba
        .conn()
        .query_row(
            "SELECT identity_id FROM devices WHERE device_id = ?",
            params![&c2_id[..]],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    assert!(stored.is_some(), "the cert verifies, so the row lands");
    assert_ne!(
        stored.as_deref().and_then(to16),
        Some(head),
        "but under the identity the rotation retired, not the one in force"
    );

    // And the round trip stops. Step 4 of the issue -- "backfill_key_envelopes
    // then seals C' the current epoch of every stream" -- emits nothing.
    let want = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(!want.is_empty(), "the revocation rotated something");
    assert!(
        envelopes_to(&ea, &dba, &c2_id).is_empty(),
        "the keys the revocation rotated away are not handed back to the same \
             device under a new name"
    );

    // The device list says so, in the field a UI renders.
    let QueryResult::Devices(rows) = ea.query(&dba, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    let c2 = rows
        .iter()
        .find(|r| r.device_id == c2_id)
        .expect("the fresh id is listed");
    assert!(!c2.revoked, "the register has never heard of this id");
    assert!(
        !c2.current,
        "and `current` is the field that shows it anyway"
    );
}

/// A helper for the four tests below: the device list row for one id.
fn device_list_row(e: &Engine, db: &Db, id: &[u8; 16]) -> crate::queries::DeviceRow {
    let QueryResult::Devices(rows) = e.query(db, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    rows.into_iter()
        .find(|r| &r.device_id == id)
        .expect("the device is listed")
}

/// A device id that turns up **after** this vault has recorded a revocation is
/// marked as such in the device list, not only in a log line.
///
/// `core.device.admitted_after_revocation` has existed since ADR-0032 and
/// reaches an operator reading NDJSON and nobody else, which is the whole of
/// issue #144's second half. The condition cannot be recomputed afterwards —
/// `0026_device_admitted_after_revocation.sql` walks the three columns that
/// look like they could and cannot — so it is written down where it is known.
#[test]
fn a_device_admitted_after_a_revocation_says_so_in_the_device_list() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    let c_id = ec.keychain.device_id();

    // B is here before anything has been revoked.
    trust(&ea, &mut dba, &eb);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .expect("revoke");
    // C turns up afterwards.
    trust_at(&ea, &mut dba, &ec, T0 + 1);

    assert!(
        device_list_row(&ea, &dba, &c_id).admitted_after_revocation,
        "the device list must carry the signal the log line has always had"
    );
    assert!(
        !device_list_row(&ea, &dba, &b_id).admitted_after_revocation,
        "a device that was already here did not join after anything"
    );
}

/// And an account that has revoked nothing marks nobody, so the field is a
/// signal rather than a badge every device wears.
#[test]
fn a_device_admitted_with_no_revocation_on_record_is_not_marked() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    assert!(
        !device_list_row(&ea, &dba, &eb.keychain.device_id()).admitted_after_revocation,
        "nothing has been revoked, so nobody joined after a revocation"
    );
}

/// The mark is not something the marked device can rub out.
///
/// `DeviceCertPublish` upserts, and a device that re-publishes its certificate
/// takes the `ON CONFLICT` arm — where `readmission` is necessarily false,
/// because the row it is conflicting with is what makes `known` true. Carrying
/// `excluded.admitted_after_revocation` across would therefore hand the one
/// party the signal is about a one-op way to clear it.
#[test]
fn a_republished_certificate_does_not_clear_the_readmission_mark() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let c_id = ec.keychain.device_id();

    trust(&ea, &mut dba, &eb);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .expect("revoke");
    trust_at(&ea, &mut dba, &ec, T0 + 1);
    assert!(device_list_row(&ea, &dba, &c_id).admitted_after_revocation);

    trust_at(&ea, &mut dba, &ec, T0 + 2);
    assert!(
        device_list_row(&ea, &dba, &c_id).admitted_after_revocation,
        "publishing the certificate again must not clear the mark"
    );
}

/// **The combination no other column in the device list can show**, and the
/// reason this one is worth a migration.
///
/// `revoked` names an id in the register and `current` names the identity that
/// verified a cert, so between them they already cover the readmission ADR-0032
/// describes *when the revocation rotated the identity*: the fresh cert is
/// under a retired link and `current` is false. They cover nothing when it did
/// not. A replica that has applied a `device_revoke` and not (yet, or ever) the
/// transition beside it — the ordinary shape of a remote revocation, and the
/// permanent shape of one run from a device that holds no `ID_S_priv` and logs
/// `core.identity.rotation_unavailable` — sees the fresh device id certified
/// under the identity in force. It reads `revoked: false, current: true`: the
/// same three columns as any honest member.
///
/// So this asserts the negative half first. Without the fourth column the row
/// is indistinguishable, and the user is told nothing.
#[test]
fn a_readmission_under_the_live_identity_is_visible_in_no_other_column() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let c_id = ec.keychain.device_id();

    trust(&ea, &mut dba, &eb);
    // A revocation that arrived as an op, with no transition beside it.
    revoke(&ea, &mut dba, &ea, eb.keychain.device_id(), T0);
    trust_at(&ea, &mut dba, &ec, T0 + 1);

    let row = device_list_row(&ea, &dba, &c_id);
    assert!(
        !row.revoked,
        "the register names ids and has never heard of this one"
    );
    assert!(
        row.current,
        "and the identity that certified it is the one in force, so every \
         other column reads exactly like an honest member's"
    );
    assert!(
        row.admitted_after_revocation,
        "which leaves this as the only field that can tell the user anything"
    );
}

/// **#105 closed rather than bounded.** The device that gets revoked cannot
/// certify itself back in because it never held the key to do it with —
/// not because a rotation retired the identity it would have used.
///
/// The test above is the *remedy*: the revoked device mints a cert, the
/// cert verifies, and rotation makes it worthless. It had to be, while
/// every paired device held `ID_S_priv`. It also only bought one
/// revocation at a time — the device paired *after* a rotation held the new
/// key, so revoking that one replayed the whole trick one identity along.
///
/// This is the fix. A device admitted by pairing is handed `ID_S_pub`, so
/// the first step of the attack — sign a cert for a fresh device id — does
/// not complete, before or after any revocation. Nothing about the
/// revocation is what stops it, which is exactly why it keeps working on
/// the second device and the third.
#[test]
fn a_paired_device_cannot_certify_a_fresh_device_id_before_or_after_its_revocation() {
    use crate::KeychainError;
    let founder = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // The device under test: same account, admitted by pairing.
    let paired = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        Arc::new(SystemRng),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [2u8; 32],
        )),
    );
    let mut dba = db_root(ROOT);
    let paired_id = paired.keychain.device_id();
    trust(&founder, &mut dba, &paired);

    // The fresh id the attack needs, with real keys behind it.
    let fresh = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let attempt = |e: &Engine| {
        e.keychain.issue_cert_for(
            fresh.keychain.device_id(),
            fresh.keychain.device_signing_pub(),
            fresh.keychain.device_dh_pub(),
            "a fresh id",
            "test",
            T0,
        )
    };

    // Before: the founder can, the paired device cannot. Both are members
    // of the same account and both hold valid certs of their own.
    assert!(attempt(&founder).is_ok(), "the control: the creator can");
    assert!(matches!(
        attempt(&paired),
        Err(KeychainError::IdentitySigningKeyAbsent)
    ));

    // The revocation, from the founder, which rotates the identity.
    let head_before = founder.current_identity(dba.conn()).unwrap().identity_id;
    founder
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, paired_id),
                reason: RevokeReason::Lost,
            },
        )
        .unwrap();
    assert_ne!(
        founder.current_identity(dba.conn()).unwrap().identity_id,
        head_before,
        "the creator holds ID_S_priv, so the revocation still rotates"
    );

    // After: unchanged, and that is the point. The revoked device's
    // inability to certify anything is a property of the device, not a
    // consequence of the rotation — so it survives the *next* revocation
    // too, which is what the remedy could not do.
    assert!(matches!(
        attempt(&paired),
        Err(KeychainError::IdentitySigningKeyAbsent)
    ));
}

/// A revocation run from a device that cannot rotate still cuts the reads.
///
/// Only the account's creator holds `ID_S_priv` now, so a paired device can
/// revoke and cannot rotate the identity. Refusing the command outright
/// would be worse than not rotating — the device a user is revoking is
/// often the one they lost, and the creator may be the one they lost — so
/// the revocation completes and says what it did not do.
///
/// What it gives up is bounded: a revoked device that was itself paired
/// holds no signing key either way, so there is nothing for the rotation to
/// have retired. The case that genuinely needs it is revoking the creator.
#[test]
fn a_revocation_from_a_paired_device_cuts_the_keys_without_rotating_the_identity() {
    let paired = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        Arc::new(SystemRng),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [1u8; 32],
        )),
    );
    let victim = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let victim_id = victim.keychain.device_id();
    trust(&paired, &mut db, &victim);

    paired
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "before the cut".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let head_before = paired.current_identity(db.conn()).unwrap().identity_id;
    let sealed_before = envelopes_to(&paired, &db, &victim_id).len();

    paired
        .apply(
            &mut db,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, victim_id),
                reason: RevokeReason::Stolen,
            },
        )
        .expect("a device with no signing key can still revoke");

    assert!(
        paired.is_revoked(db.conn(), &victim_id).unwrap(),
        "the cut is recorded"
    );
    assert_eq!(
        envelopes_to(&paired, &db, &victim_id).len(),
        sealed_before,
        "and the rotation sealed the new epochs to everyone except it"
    );
    assert_eq!(
        paired.current_identity(db.conn()).unwrap().identity_id,
        head_before,
        "the identity did not move, because this device cannot sign a transition"
    );

    // ...and asking for the rotation on its own says so, rather than
    // failing with something storage-shaped.
    let err = paired
        .apply(
            &mut db,
            Command::RotateIdentity {
                keep_recovery_code: true,
            },
        )
        .expect_err("a paired device cannot rotate the account identity");
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains("admitted by pairing")),
        "expected a named refusal, got {err:?}"
    );
}

// ---- the verification rule (ADR-0032, #105) ----

/// Build a complete, valid `identity_transition` from `emitter` onto a
/// fresh successor, with `roster` as the surviving devices.
///
/// Assembled by hand rather than through a command, because the command is
/// the next commit's. Every byte of it is what a real emitter produces —
/// the roster is signed by the successor, the shares are real HPKE seals,
/// and the two signatures are taken by the keychain over the real digests —
/// so a test that applies one is exercising the production path and not a
/// fixture.
///
/// # `roster` must name `emitter` for the emitter to adopt
///
/// `roster` is a free parameter here and is **not** free in production:
/// [`Engine::rotate_identity`] pushes this device into the survivor set
/// unconditionally, right after the `devices` query that deliberately skips
/// it, so a real emitter is always in its own roster. Since `#105`,
/// `recompute_identity_head` takes the adopting device's cert *from* the
/// roster rather than re-issuing it — most devices hold no `ID_S_priv` and
/// could not sign one — so an emitter left out of `roster` opens its share,
/// finds no cert to adopt under, and keeps its old `identity_id`.
///
/// That matters to any caller that rotates more than once, because
/// `from_identity_id` below is `emitter.keychain.identity_id()`: an emitter
/// that never adopts emits every later transition from the *same*
/// predecessor, so what accumulates is a fan of siblings off one identity
/// rather than a chain — and the fan hits
/// [`MAX_SIBLINGS_PER_PREDECESSOR`] at ingest on the seventeenth.
/// A caller building a chain must pass `emitter` in `roster`. A caller
/// deliberately testing a losing branch, a rejected payload or a single
/// transition need not, and several below do not.
fn build_transition(
    emitter: &Engine,
    roster: &[&Engine],
    carry: bool,
    now_ms: u64,
) -> (InnerOp, [u8; 16]) {
    let successor = emitter.keychain.mint_successor_identity(&SystemRng);
    let mut entries: Vec<RosterEntry> = Vec::new();
    let mut shares: Vec<KeyShare> = Vec::new();
    for e in roster {
        let cert = Keychain::issue_roster_cert(
            &successor,
            e.keychain.device_id(),
            e.keychain.device_signing_pub(),
            e.keychain.device_dh_pub(),
            "device",
            "test",
            now_ms,
        )
        .expect("roster cert");
        entries.push(RosterEntry { cert });
        shares.push(KeyShare {
            device_id: e.keychain.device_id(),
            hpke_ciphertext: emitter
                .keychain
                .seal_successor_device_share(
                    &successor,
                    &e.keychain.device_dh_pub(),
                    &e.keychain.device_id(),
                    &SystemRng,
                )
                .expect("device share"),
        });
    }
    let identity_share = carry.then(|| {
        serde_bytes::ByteBuf::from(
            emitter
                .keychain
                .seal_successor_carry_share(&successor, &SystemRng)
                .expect("carry share"),
        )
    });

    let certs: Vec<&[u8]> = entries.iter().map(|e| e.cert.as_slice()).collect();
    let share_refs: Vec<([u8; 16], &[u8])> = shares
        .iter()
        .map(|s| (s.device_id, s.hpke_ciphertext.as_slice()))
        .collect();
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id: emitter.keychain.identity_id(),
        to_identity_id: successor.identity_id(),
        to_id_s_pub: successor.id_s_pub(),
        to_id_d_pub: successor.id_d_pub(),
        roster_digest: sunrise_crypto::roster_digest(&certs).expect("roster digest"),
        shares_digest: sunrise_crypto::shares_digest(
            &share_refs,
            identity_share.as_ref().map(|b| b.as_slice()),
        )
        .expect("shares digest"),
    };
    let sigs = emitter
        .keychain
        .sign_transition(&body, &successor)
        .expect("sign");
    let to = successor.identity_id();
    (
        InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
            from_identity_id: body.from_identity_id,
            to_identity_id: body.to_identity_id,
            to_id_s_pub: body.to_id_s_pub,
            to_id_d_pub: body.to_id_d_pub,
            roster: entries,
            device_shares: shares,
            identity_share,
            prev_sig: sigs.prev_sig,
            next_sig: sigs.next_sig,
        })),
        to,
    )
}

/// Apply `inner` to `receiver` as `sender`, at a chosen meta epoch.
fn apply_control_at(
    receiver: &Engine,
    db: &mut Db,
    inner: &InnerOp,
    sender: &[u8; 16],
    hlc: Hlc,
    meta_epoch: u32,
) {
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, inner, sender, hlc, hlc.physical_ms, meta_epoch)
            .map(|_| ())
    })
    .unwrap();
}

/// A valid transition moves the head, and the emitter adopts it.
#[test]
fn an_applied_transition_moves_the_head() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let before = ea.current_identity(dba.conn()).unwrap();

    let (inner, to) = build_transition(&ea, &[&ea, &eb], true, T0);
    apply_control_at(
        &ea,
        &mut dba,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let head = ea.current_identity(dba.conn()).unwrap();
    assert_eq!(head.identity_id, to);
    assert_ne!(head.identity_id, before.identity_id);
    assert_eq!(
        ea.chain_identities(dba.conn()).unwrap(),
        vec![
            (before.identity_id, before.id_s_pub),
            (head.identity_id, head.id_s_pub)
        ],
        "the chain is genesis then successor, in that order"
    );
    // The emitter holds the outgoing ID_D_priv, so it opens the carry share
    // and keeps the account's unwrapping key across the rotation.
    assert_eq!(ea.keychain.identity_id(), to);
    assert!(ea.keychain.holds_only_copy_of_identity_key());
}

/// Re-delivery is a no-op: the row is keyed on `to_identity_id`.
#[test]
fn applying_the_same_transition_twice_changes_nothing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let (inner, to) = build_transition(&ea, &[&eb], true, T0);
    let a_id = ea.keychain.device_id();
    apply_control_at(&ea, &mut dba, &inner, &a_id, Hlc::at(T0), 1);
    apply_control_at(&ea, &mut dba, &inner, &a_id, Hlc::at(T0 + 5), 1);
    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(ea.current_identity(dba.conn()).unwrap().identity_id, to);
}

/// A device left out of the roster does not adopt, and reads itself as no
/// longer speaking for the account. This is the mechanism, not a failure.
#[test]
fn a_device_left_out_of_the_roster_does_not_adopt() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    // A rotates, and B is not in the roster.
    let (inner, to) = build_transition(&ea, &[], false, T0);
    apply_control_at(
        &eb,
        &mut dbb,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        to,
        "B agrees who the account now is"
    );
    assert_ne!(
        eb.keychain.identity_id(),
        to,
        "...and knows it is not itself: no share, no adoption"
    );
}

/// Structural rejections are logged and dropped, never returned, and leave
/// the head where it was. Each of the four is a different way to be
/// malformed and none of them is a delivery failure.
#[test]
fn a_malformed_transition_is_dropped_and_moves_nothing() {
    /// One named way to malform a transition. Declared before the first
    /// statement, because `clippy::items_after_statements` is denied.
    type Case<'a> = (
        &'static str,
        Box<dyn Fn(&mut IdentityTransitionPayload) + 'a>,
    );

    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    let a_id = ea.keychain.device_id();

    let mutate = |f: &dyn Fn(&mut IdentityTransitionPayload)| {
        let (inner, _) = build_transition(&ea, &[&eb], true, T0);
        let InnerOp::IdentityTransition(mut p) = inner else {
            unreachable!()
        };
        f(&mut p);
        InnerOp::IdentityTransition(p)
    };

    let cases: Vec<Case<'_>> = vec![
        // The id is a derivation of the key, so a chosen one is refused.
        (
            "id_derivation",
            Box::new(|p: &mut IdentityTransitionPayload| p.to_identity_id[0] ^= 0x01),
        ),
        // A roster entry that is not a cert.
        (
            "roster",
            Box::new(|p: &mut IdentityTransitionPayload| {
                p.roster[0].cert = b"not a cert".to_vec();
            }),
        ),
        // A share of the wrong width: the digest refuses to be computed
        // rather than hashing an ambiguous concatenation.
        (
            "shares",
            Box::new(|p: &mut IdentityTransitionPayload| {
                p.device_shares[0].hpke_ciphertext.truncate(60);
            }),
        ),
        // A cert that is a valid cert, but not one the successor issued.
        (
            "roster_binding",
            Box::new(|p: &mut IdentityTransitionPayload| {
                p.roster[0].cert = eb.keychain.cert_blob();
            }),
        ),
    ];

    for (name, f) in cases {
        let inner = mutate(f.as_ref());
        apply_control_at(&ea, &mut dba, &inner, &a_id, Hlc::at(T0), 1);
        assert_eq!(
            ea.current_identity(dba.conn()).unwrap().identity_id,
            head,
            "`{name}` must not move the head"
        );
        let n: i64 = dba
            .conn()
            .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "`{name}` must not become a link");
    }
}

/// A whole, genuinely signed transition belonging to **another account**.
///
/// Built from `sunrise_crypto` directly rather than from a keychain, because
/// every engine a test can construct shares this vault root and therefore this
/// account's identity — a stranger needs a predecessor key no keychain here
/// holds. The shares are the right width and nothing else: the apply path
/// recomputes their digest and never opens them, and what this fixture is for
/// is the path, not the payload.
fn stranger_transition(for_device: &Engine, now_ms: u64) -> InnerOp {
    use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
    use sunrise_crypto::{DeviceCertInner, DEVICE_SHARE_LEN};

    let from = IdentitySigningKeyPair::from_secret_bytes(&[0x5e; 32]);
    let to = IdentitySigningKeyPair::from_secret_bytes(&[0x7a; 32]);
    let to_dh = IdentityDhKeyPair::from_secret_bytes([0x7b; 32]);
    let to_identity_id = sunrise_crypto::identity_id_from_pub(&to.public_bytes());

    let cert = DeviceCert::issue(
        DeviceCertInner {
            v: 1,
            device_id: for_device.keychain.device_id(),
            d_s_pub: for_device.keychain.device_signing_pub(),
            d_d_pub: for_device.keychain.device_dh_pub(),
            identity_id: to_identity_id,
            created_at_ms: now_ms,
            nickname: "stranger".into(),
            platform: "test".into(),
        },
        &to,
    )
    .expect("issue the stranger's roster cert")
    .to_cbor()
    .expect("cbor");

    let share = vec![0x11u8; DEVICE_SHARE_LEN];
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id: sunrise_crypto::identity_id_from_pub(&from.public_bytes()),
        to_identity_id,
        to_id_s_pub: to.public_bytes(),
        to_id_d_pub: to_dh.public_bytes(),
        roster_digest: sunrise_crypto::roster_digest(std::slice::from_ref(&cert))
            .expect("roster digest"),
        shares_digest: sunrise_crypto::shares_digest(
            &[(for_device.keychain.device_id(), share.as_slice())],
            None,
        )
        .expect("shares digest"),
    };
    let sigs = sunrise_crypto::sign_identity_transition(&body, &from, &to).expect("sign");
    InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
        from_identity_id: body.from_identity_id,
        to_identity_id: body.to_identity_id,
        to_id_s_pub: body.to_id_s_pub,
        to_id_d_pub: body.to_id_d_pub,
        roster: vec![RosterEntry { cert }],
        device_shares: vec![KeyShare {
            device_id: for_device.keychain.device_id(),
            hpke_ciphertext: share,
        }],
        identity_share: None,
        prev_sig: sigs.prev_sig,
        next_sig: sigs.next_sig,
    }))
}

/// A transition signed by somebody who is not the predecessor is stored —
/// applying is unconditional — and is never a link, because the fold checks
/// `prev_sig` under the identity it actually claims to succeed.
#[test]
fn a_transition_from_a_stranger_is_stored_and_never_folded() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    let inner = stranger_transition(&ea, T0);
    apply_control_at(
        &ea,
        &mut dba,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1, "it is structurally well formed, so it is stored");
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "and it extends nothing: it does not succeed this account's head"
    );
}

/// **A body edited after it was signed never reaches the table.**
///
/// The row is keyed on `to_identity_id` and written with `INSERT OR IGNORE`, so
/// whichever copy of a transition arrives first owns that key for good. Before
/// the successor signature was checked here, anyone who saw an honest
/// transition could re-publish it with the roster or the shares substituted and
/// take the key: the forged row never verifies at fold time, so it is never
/// *believed* — but it is never *replaced* either, and the honest rotation can
/// then never be recorded on that replica. A revoked device could suppress its
/// own removal from every peer's view that way.
///
/// `next_sig` is checkable without knowing the predecessor, which is what makes
/// the check possible this early. Here the tamper is on `from_identity_id`, so
/// the same `body_hash` no longer exists and neither signature covers what
/// arrived.
#[test]
fn a_transition_edited_after_signing_is_refused_rather_than_stored() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let (inner, _) = build_transition(&ea, &[&eb], true, T0);
    let InnerOp::IdentityTransition(mut p) = inner else {
        unreachable!()
    };
    p.from_identity_id = [0x5e; 16];
    apply_control_at(
        &ea,
        &mut dba,
        &InnerOp::IdentityTransition(p),
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0, "an altered body does not get to occupy the key");
}

/// The same, with the tamper on the part an attacker actually wants to change.
///
/// Substituting the roster is the whole point of taking the key: it is what
/// decides who lands on the successor identity. The recomputed digest then
/// differs from the signed one, so the body differs, so `next_sig` does not
/// verify — without needing to know the predecessor at all.
#[test]
fn a_transition_whose_roster_was_substituted_is_refused_rather_than_stored() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let (inner, _) = build_transition(&ea, &[&ea, &eb], true, T0);
    let InnerOp::IdentityTransition(mut p) = inner else {
        unreachable!()
    };
    assert_eq!(p.roster.len(), 2, "the honest roster carries both devices");
    p.roster.truncate(1);
    apply_control_at(
        &ea,
        &mut dba,
        &InnerOp::IdentityTransition(p),
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0, "a roster the successor did not sign is not stored");
}

/// A vault that has taken no transition has a one-link chain, and the head
/// is the identity it has always had. The base case every other assertion
/// below is a departure from.
#[test]
fn an_unrotated_account_has_a_one_link_chain() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let dba = db_root(ROOT);
    let head = ea.current_identity(dba.conn()).unwrap();
    assert_eq!(head.identity_id, ea.keychain.identity_id());
    assert_eq!(head.id_s_pub, ea.keychain.identity_signing_pub());
    assert_eq!(
        ea.chain_identities(dba.conn()).unwrap(),
        vec![(head.identity_id, head.id_s_pub)]
    );
}

/// The rule, stated without the machinery that will produce it: a device
/// row naming an identity that is not the head is admitted, keeps its cert,
/// and receives nothing.
///
/// The stale `identity_id` is written directly here rather than produced by
/// a rotation, and deliberately: this asserts the *guard*, which is the
/// half that has to exist before anything can emit a transition. The
/// end-to-end version — where the staleness comes from a real rotation the
/// device could not follow — is the flipped `#105` test.
#[test]
fn a_device_certified_under_a_non_head_identity_receives_nothing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);

    // B is a member and a recipient while its row names the head.
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert!(
        !envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a member is a recipient, or this test proves nothing"
    );
    let before = envelopes_to(&ea, &dba, &b_id).len();

    // Its cert is untouched and its row stays; only the identity that
    // certified it is no longer the one in force.
    dba.conn()
        .execute(
            "UPDATE devices SET identity_id = ? WHERE device_id = ?",
            params![&[0x9fu8; 16][..], &b_id[..]],
        )
        .unwrap();
    assert!(
        !ea.is_revoked(dba.conn(), &b_id).unwrap(),
        "nothing revoked it; membership is the only thing that moved"
    );

    // No new epoch reaches it...
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        before,
        "the recipient query bounds every subsequent epoch, which is what a \
             one-shot check at admission could not do"
    );
}

/// Membership is **derived, never stored as a decision** — the property
/// that makes the rule convergent, asserted through the one path that
/// recomputes it.
///
/// Republishing a cert rewrites `devices.identity_id` from whichever chain
/// identity actually verified it. Here that is still the head, because
/// nothing has rotated, so the device becomes current again with no other
/// change: the stale row was a fact about a past publish and not a verdict
/// the account had recorded.
///
/// That is also precisely why the real fix works. In the rotated case the
/// same recomputation writes the *superseded* issuer, so the republish that
/// restores membership here restores nothing there — and the difference is
/// entirely in the cert, which is the one thing a departed device cannot
/// change.
#[test]
fn membership_is_recomputed_from_the_issuer_at_every_publish() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    dba.conn()
        .execute(
            "UPDATE devices SET identity_id = ? WHERE device_id = ?",
            params![&[0x9fu8; 16][..], &b_id[..]],
        )
        .unwrap();
    let stale = envelopes_to(&ea, &dba, &b_id).len();
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert_eq!(envelopes_to(&ea, &dba, &b_id).len(), stale);

    // The cert is republished unchanged. It still verifies under the head,
    // so the row is rewritten to the head and the device is current again.
    trust_at(&ea, &mut dba, &eb, T0 + 1);
    let restored: Vec<u8> = dba
        .conn()
        .query_row(
            "SELECT identity_id FROM devices WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        restored,
        head.to_vec(),
        "the column records which identity verified the cert, recomputed \
             each time rather than carried forward"
    );
    assert!(
        envelopes_to(&ea, &dba, &b_id).len() > stale,
        "and the backfill that runs behind the publish now reaches it"
    );
}

/// A user-requested rotation keeps everybody: every current device is in
/// the roster, adopts, and goes on receiving keys.
///
/// The counterpart to the revocation case, and the one that says the fix is
/// a *rotation* rather than a way to lock devices out. Nothing needs
/// re-pairing and nothing goes quiet.
#[test]
fn a_requested_rotation_keeps_every_current_device() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);
    let before_identity = ea.current_identity(dba.conn()).unwrap().identity_id;
    let before_envelopes = envelopes_to(&ea, &dba, &b_id).len();

    ea.apply(
        &mut dba,
        Command::RotateIdentity {
            keep_recovery_code: true,
        },
    )
    .unwrap();

    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    assert_ne!(head, before_identity, "the identity moved");
    assert_eq!(
        ea.keychain.identity_id(),
        head,
        "and the emitter adopted it"
    );

    // B's row was moved onto the successor by the roster, so it is current
    // and still a recipient.
    let QueryResult::Devices(rows) = ea.query(&dba, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    let b = rows.iter().find(|r| r.device_id == b_id).expect("listed");
    assert!(b.current, "a rotation that keeps everybody keeps everybody");
    assert!(!b.revoked);

    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert!(
        envelopes_to(&ea, &dba, &b_id).len() > before_envelopes,
        "and it goes on receiving keys minted after the rotation"
    );
}

/// Revoking the device that holds `ID_D_priv` **must not** carry the
/// recovery code forward.
///
/// The carry share is sealed to the outgoing `ID_D_pub`, and the only
/// holder of the matching private half is the account's creator. Carrying
/// it while revoking that creator would seal the successor to the very key
/// the rotation exists to exclude: every check would pass and the excluded
/// device would hold the new identity. So the request is overridden, the
/// user's code stops working, and a client has to say so.
///
/// The `identity` row is written by hand because `Keychain::for_test*`
/// builds none; `minted_by_device_id` is the column that names the holder,
/// and it is the one this branch reads.
#[test]
fn revoking_the_recovery_key_holder_refuses_to_carry_the_code() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);

    let id = ea.keychain.identity_id();
    let pk = ea.keychain.identity_signing_pub();
    dba.conn()
        .execute(
            "INSERT INTO identity
                 (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped,
                  id_d_priv_wrapped, created_at_ms, minted_by_device_id,
                  genesis_identity_id, genesis_id_s_pub)
                 VALUES (1, ?1, ?2, X'00', X'00', X'00', 0, ?3, ?1, ?2)",
            params![&id[..], &pk[..], &b_id[..]],
        )
        .unwrap();

    // B holds ID_D_priv, and B is the device being revoked.
    let out = ea.rotate_identity(&mut dba, Some(b_id), true).unwrap();
    assert!(
        !out.carried_recovery_code,
        "carrying it would seal the successor to the key being excluded"
    );

    // The same rotation excluding somebody else does carry it.
    let out = ea
        .rotate_identity(&mut dba, Some([0x77; 16]), true)
        .unwrap();
    assert!(out.carried_recovery_code);
}

/// The same bypass, seen by a **peer** rather than by the device that did
/// the revoking.
///
/// The flipped test above runs on the revoker's own replica, where the
/// rotation and the attacker's cert land in one process. This is the case
/// that actually matters for convergence: B applies the revocation and the
/// rotation as ordinary remote ops, then meets the fresh cert. B has no
/// special knowledge — it did not choose the successor and cannot tell an
/// attacker's fresh id from an honest new pairing — and still gives it
/// nothing, because the only question it asks is which chain identity
/// signed the cert.
#[test]
fn a_peer_gives_nothing_to_a_fresh_id_certified_under_a_retired_identity() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec2 = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let c2_id = ec2.keychain.device_id();

    // B knows A. A rotates, keeping B, and B applies the transition as an
    // ordinary remote control op.
    trust(&eb, &mut dbb, &ea);
    let (inner, to) = build_transition(&ea, &[&eb], true, T0);
    apply_control_at(
        &eb,
        &mut dbb,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );
    assert_eq!(eb.current_identity(dbb.conn()).unwrap().identity_id, to);
    assert_eq!(
        eb.keychain.identity_id(),
        to,
        "B is in the roster, so it adopts"
    );

    // The attacker's fresh id, certified under the identity it kept.
    let before = envelopes_to(&eb, &dbb, &c2_id).len();
    trust_at(&eb, &mut dbb, &ec2, T0 + 1);
    assert_eq!(
        envelopes_to(&eb, &dbb, &c2_id).len(),
        before,
        "B has no way to recognise the attacker and does not need one"
    );
    let QueryResult::Devices(rows) = eb.query(&dbb, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    assert!(
        !rows
            .iter()
            .find(|r| r.device_id == c2_id)
            .expect("listed")
            .current
    );
}

/// Seal `inner` into a real envelope under a key the *attacker* minted.
///
/// The whole point of the epoch-forgery cases: an attacker cannot raise
/// `meta_epoch` by asserting a number, because the number names a key. It
/// can mint a key of its own and claim any epoch it likes — this builds
/// exactly that, with a real signature over real ciphertext, so the
/// receiver's refusal is a cryptographic one and not a parse failure.
fn forged_envelope_at(
    attacker: &Engine,
    attacker_db: &mut Db,
    inner: &InnerOp,
    mints: u32,
    hlc: Hlc,
) -> Vec<u8> {
    let mut minted = (0u32, StreamKey::from_bytes([0u8; 32]));
    for _ in 0..mints {
        minted = attacker_db
            .with_tx(|tx| {
                attacker
                    .keychain
                    .mint_epoch(tx, &META_STREAM, &SystemRng, T0)
            })
            .expect("the attacker mints its own epoch");
    }
    attacker
        .keychain
        .seal_op_at(
            META_STREAM,
            99,
            hlc,
            &encode_inner_op(inner).expect("encode"),
            &SystemRng,
            minted.0,
            &minted.1,
        )
        .expect("seal under the self-minted key")
}

/// A forged transition sealed under a **self-minted `e+1`** is refused, not
/// applied and not parked.
///
/// The revoked device can mint keys — nothing stops it doing arithmetic in
/// its own vault — so it can claim the epoch the honest rotation just
/// reached. What it cannot do is produce *the* key the survivors hold at
/// that epoch, because that one was random and was never sealed to it. So
/// the survivors have a non-empty key list at `e+1` and none of them opens
/// this envelope, which is the one case `apply_remote` treats as an error
/// rather than as a missing key.
///
/// That distinction is the whole test. "No key" parks, because the
/// `key_envelope` carrying it may simply be in flight; "keys, none of which
/// opens it" is ciphertext nobody in this account wrote.
#[test]
fn a_transition_forged_under_a_self_minted_next_epoch_is_refused() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    trust(&ea, &mut dba, &ec);
    trust(&ec, &mut dbc, &ea);
    let c_id = ec.keychain.device_id();

    // The revocation rotates the meta stream, so A now holds a *random*
    // key at the next epoch.
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Compromised,
        },
    )
    .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    let live = dba
        .with_tx(|tx| ea.keychain.current_epoch_tx(tx, &META_STREAM))
        .unwrap()
        .expect("the meta stream has an epoch");
    assert!(
        !ea.keychain.stream_keys_at(&META_STREAM, live).is_empty(),
        "the premise: the survivor holds a key at the epoch being forged"
    );

    // C forges a transition off the identity it kept and seals it under an
    // epoch it minted itself.
    //
    // **Twice**, and that is load-bearing. The vault-meta stream's genesis
    // epoch is *derived* — `meta_genesis_key`, from `ID_D_priv ‖ identity_id`,
    // which is what lets a recovered vault reach the epochs that predate it —
    // so the first mint on any vault holding `ID_D_priv` produces the key every
    // other such vault already has. Sealing there is not forging under a
    // self-minted key at all; it is using the shared one. The second mint is
    // the first that is genuinely C's own.
    let (inner, _) = build_transition(&ec, &[&ec], false, T0);
    let forged = forged_envelope_at(&ec, &mut dbc, &inner, 2, Hlc::at(T0 + 1));
    let err = ea.apply_remote_all(&mut dba, &forged).unwrap_err();
    assert!(
        matches!(&err, EngineError::RemoteOpInvalid(m)
                if m.contains("no key at this (stream, epoch) opens the envelope")),
        "expected a decrypt refusal, got {err:?}"
    );
    assert_eq!(deferred_rows(&dba), 0, "refused, not parked");
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "and the head did not move"
    );
}

/// **The vault-meta genesis epoch is shared by every holder of `ID_D_priv`,
/// and that is the weakest place to seal anything.**
///
/// `meta_genesis_key` derives epoch 1 of the vault-meta stream from
/// `ID_D_priv ‖ identity_id` rather than minting it at random. That is what
/// makes recovery work at all — a restored vault has the identity key and
/// nothing else, so a random genesis would leave every epoch that predates the
/// recovery unreadable — and its cost is that a *revoked* device which held
/// `ID_D_priv` keeps the ability to seal and open at that one epoch forever.
///
/// It does not get such a device anything, and this pins why: `meta_epoch` is
/// the first component of the fold's ordering key, and the genesis is the
/// lowest epoch there is. Anything sealed there sorts below every honest
/// rotation by construction. The envelope opens; the head does not move.
#[test]
fn an_op_sealed_at_the_derived_genesis_epoch_opens_but_outranks_nothing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    trust(&ea, &mut dba, &ec);
    trust(&ec, &mut dbc, &ea);
    let c_id = ec.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Compromised,
        },
    )
    .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    // One mint lands on the derived genesis, which A holds too.
    let (inner, _) = build_transition(&ec, &[&ec], false, T0);
    let at_genesis = forged_envelope_at(&ec, &mut dbc, &inner, 1, Hlc::at(T0 + 1));
    ea.apply_remote_all(&mut dba, &at_genesis)
        .expect("A can open what was sealed under the epoch both derive");

    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "the genesis epoch is the bottom of the ordering key, so it moves nothing"
    );
}

/// The same forgery at **`e+2`** parks instead of erroring, is never
/// applied, and ages out.
///
/// Parking is correct here and is not a weakness: from the receiver's side
/// "I hold no key at epoch 3" is indistinguishable from an honest op that
/// overtook its `key_envelope`, and refusing would lose that op for good —
/// the relay does not redeliver. What matters is that parking is *inert*:
/// the op never applies, the head never moves, and the row is swept at
/// [`DEFERRED_TTL_MS`] rather than sitting there forever waiting for a key
/// that cannot exist, because no honest device ever minted that epoch.
#[test]
fn a_transition_forged_two_epochs_ahead_parks_and_is_never_applied() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], cb.clone());
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    trust(&eb, &mut dbb, &ec);
    trust(&ec, &mut dbc, &ea);
    // B is the replica under attack, and it has to be a *replica* rather
    // than the emitter: the TTL sweep runs inside `drain_deferred`, which
    // only a remotely absorbed key reaches.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    let head = eb.current_identity(dbb.conn()).unwrap().identity_id;

    let (inner, to) = build_transition(&ec, &[&ec], false, T0);
    let forged = forged_envelope_at(&ec, &mut dbc, &inner, 2, Hlc::at(T0 + 1));
    assert!(eb.apply_remote_all(&mut dbb, &forged).unwrap().is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "no key at that epoch, so it parks");
    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        head,
        "parked is not applied"
    );
    let n: i64 = dbb
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
            params![&to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "and it never became a link");

    // A month later some *other* key reaches B — an ordinary new Stream —
    // and the drain it triggers sweeps by age. The forged row is swept
    // rather than released, because the epoch it claims was never minted by
    // anyone in this account and no envelope for it can ever arrive.
    set_clock(&ca, T0 + DEFERRED_TTL_MS + 1);
    set_clock(&cb, T0 + DEFERRED_TTL_MS + 1);
    let stream = ea
        .apply(
            &mut dba,
            Command::CreateStream(StreamDraft {
                name: "later".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "in the new stream".into(),
            stream_id: Some(stream),
            ..Default::default()
        }),
    )
    .unwrap();
    for env in key_envelope_envs(&dba) {
        let _ = eb.apply_remote_all(&mut dbb, &env);
    }
    assert_eq!(deferred_rows(&dbb), 0, "swept at the TTL, not lingering");
    assert_eq!(eb.current_identity(dbb.conn()).unwrap().identity_id, head);
}

/// **A device that was offline across the rotation comes back and catches
/// up, with no recovery code and no re-pairing.**
///
/// This is the half of #143 that decides whether any of it is usable. The
/// transition is sealed under the meta epoch the rotation *minted*, so a
/// device that missed that epoch cannot read the op that would tell it the
/// identity moved — which looks circular and is not, because the
/// `key_envelope` carrying the new meta epoch is itself sealed under the
/// **old** one. The device can always open that.
///
/// So the order is: park the transition, absorb the envelope, drain, apply,
/// and take the re-issued cert and the `ID_S_priv` share out of the roster.
/// Each step is asserted here, because the failure mode is a device that
/// silently never becomes current again and can only be fixed by re-pairing
/// it — which is exactly the outcome a rotation is supposed to avoid.
#[test]
fn a_device_offline_across_a_rotation_catches_up_without_re_pairing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let c_id = ec.keychain.device_id();

    // A knows B and C; B knows A. B then goes offline.
    trust(&ea, &mut dba, &eb);
    trust(&ea, &mut dba, &ec);
    trust(&eb, &mut dbb, &ea);
    // Something has to be written before the vault-meta stream has a key
    // to hand over at all.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before B went away".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    let before = eb.current_identity(dbb.conn()).unwrap().identity_id;

    // A revokes C while B is away. B is in the roster; C is not.
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    assert_ne!(
        head, before,
        "the premise: the identity moved while B was away"
    );

    // B comes back and meets the transition first. It is sealed under the
    // meta epoch the rotation minted, which B does not hold.
    let transition = env_for_kind(&dba, &head, "identity.transition");
    assert!(eb
        .apply_remote_all(&mut dbb, &transition)
        .unwrap()
        .is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "parked, because the epoch is new");
    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        before,
        "and nothing has moved yet"
    );

    // Now the `key_envelope` that carries that epoch. It was sealed under
    // the OLD meta epoch, so B can open it — this is what breaks the
    // apparent circularity.
    let mut drained = false;
    for env in key_envelope_envs(&dba) {
        if eb.apply_remote_all(&mut dbb, &env).is_ok() {
            drained = true;
        }
    }
    assert!(drained, "B absorbed at least one envelope");
    assert_eq!(
        deferred_rows(&dbb),
        0,
        "and the drain released the transition"
    );

    // B is current again, on its own, with everything it needs.
    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        head,
        "B agrees who the account is"
    );
    assert_eq!(
        eb.keychain.identity_id(),
        head,
        "B opened its share and adopted: no recovery code, no re-pairing"
    );
    DeviceCert::from_cbor(&eb.keychain.cert_blob())
        .unwrap()
        .verify_binding(&eb.keychain.identity_signing_pub(), &head)
        .expect("B holds a cert under the identity in force");

    // ...and C, which was excluded, is not current on B's replica either.
    let QueryResult::Devices(rows) = eb.query(&dbb, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    if let Some(c) = rows.iter().find(|r| r.device_id == c_id) {
        assert!(!c.current, "the excluded device is not current anywhere");
    }
}

/// A complete, valid transition whose roster and share list name `devices`
/// synthetic survivors.
///
/// [`build_transition`] can only name engines that exist, and the point of the
/// size cap is a payload far larger than any real account. This mints the
/// successor the same way `rotate_identity` does and issues one genuine cert
/// and one genuine HPKE share per synthetic device, so the only thing wrong
/// with the result is how many there are of them.
fn build_transition_naming(emitter: &Engine, devices: usize, now_ms: u64) -> InnerOp {
    use rand_core::SeedableRng;
    use sunrise_crypto::keys::{DeviceDhKeyPair, DeviceSigningKeyPair};

    let successor = emitter.keychain.mint_successor_identity(&SystemRng);
    // The device keys are throwaway: nothing verifies a signature by them here,
    // only that the HPKE seals target distinct real X25519 points and the certs
    // carry distinct real Ed25519 ones.
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xD06F);
    let mut entries: Vec<RosterEntry> = Vec::with_capacity(devices);
    let mut shares: Vec<KeyShare> = Vec::with_capacity(devices);
    for i in 0..devices {
        let d_s = DeviceSigningKeyPair::generate(&mut rng);
        let d_d = DeviceDhKeyPair::generate(&mut rng);
        let mut device_id = [0u8; 16];
        device_id[..8].copy_from_slice(&(i as u64).to_be_bytes());
        entries.push(RosterEntry {
            cert: Keychain::issue_roster_cert(
                &successor,
                device_id,
                d_s.public_bytes(),
                d_d.public_bytes(),
                "device",
                "test",
                now_ms,
            )
            .expect("roster cert"),
        });
        shares.push(KeyShare {
            device_id,
            hpke_ciphertext: emitter
                .keychain
                .seal_successor_device_share(
                    &successor,
                    &d_d.public_bytes(),
                    &device_id,
                    &SystemRng,
                )
                .expect("device share"),
        });
    }
    let certs: Vec<&[u8]> = entries.iter().map(|e| e.cert.as_slice()).collect();
    let share_refs: Vec<([u8; 16], &[u8])> = shares
        .iter()
        .map(|s| (s.device_id, s.hpke_ciphertext.as_slice()))
        .collect();
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id: emitter.keychain.identity_id(),
        to_identity_id: successor.identity_id(),
        to_id_s_pub: successor.id_s_pub(),
        to_id_d_pub: successor.id_d_pub(),
        roster_digest: sunrise_crypto::roster_digest(&certs).expect("roster digest"),
        shares_digest: sunrise_crypto::shares_digest(&share_refs, None).expect("shares digest"),
    };
    let sigs = emitter
        .keychain
        .sign_transition(&body, &successor)
        .expect("sign");
    InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
        from_identity_id: body.from_identity_id,
        to_identity_id: body.to_identity_id,
        to_id_s_pub: body.to_id_s_pub,
        to_id_d_pub: body.to_id_d_pub,
        roster: entries,
        device_shares: shares,
        identity_share: None,
        prev_sig: sigs.prev_sig,
        next_sig: sigs.next_sig,
    }))
}

/// The state `MAX_TRANSITION_CHAIN = 64` could put an account into and never
/// let it out of.
///
/// The fold walked at most 64 links. Any holder of `ID_S_priv` -- which is
/// every paired device, and every device that ever was one -- could reach that
/// by rotating, and past it the head was pinned forever: every later transition
/// was a row the walk never reached, so no rotation took effect, so no
/// *revocation* took effect either, because a revocation is a rotation. Nothing
/// recovered from it and nothing reported it; the head was a real identity with
/// a real set of current devices under it.
///
/// Seventy links, which is past the old cap by six. Each one is a genuine
/// transition -- a minted successor, a roster signed by it, real HPKE shares,
/// both signatures taken by the keychain -- applied through the same
/// `apply_control_op` arm a peer's delivery reaches, so what is being folded is
/// what production writes.
///
/// The roster names **A as well as B**, because A is the emitter and
/// `rotate_identity` puts this device in its own survivor set unconditionally.
/// Without it A opens its share, finds no cert in the roster to adopt under,
/// and keeps its old `identity_id` -- so every later transition is emitted
/// from the *same* predecessor and what this builds is seventy siblings of
/// genesis rather than a chain of seventy links. That is not the shape the
/// assertions below are about, and it is not a shape production can emit;
/// see `build_transition` for the invariant.
///
/// The assertion that matters is the last one: after seventy rotations the
/// account can still rotate again, and the new head is the one it just moved
/// to. That is "cannot reach a state it cannot leave", stated as the thing a
/// frozen account would fail.
#[test]
fn a_chain_past_the_old_cap_still_folds_and_can_still_be_extended() {
    const LINKS: usize = 70;

    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let genesis = ea.current_identity(dba.conn()).unwrap().identity_id;

    let mut last = genesis;
    for i in 0..LINKS {
        let (inner, to) = build_transition(&ea, &[&ea, &eb], true, T0);
        apply_control_at(
            &ea,
            &mut dba,
            &inner,
            &a_id,
            Hlc::at(T0 + i as u64 + 1),
            u32::try_from(i).unwrap() + 1,
        );
        assert_eq!(
            ea.current_identity(dba.conn()).unwrap().identity_id,
            to,
            "rotation {i} did not move the head; the fold stopped short of it"
        );
        last = to;
    }

    let chain = ea.chain_identities(dba.conn()).unwrap();
    assert_eq!(
        chain.len(),
        LINKS + 1,
        "the fold walked {} of {} links",
        chain.len(),
        LINKS + 1
    );
    assert_eq!(chain.first().unwrap().0, genesis);
    assert_eq!(chain.last().unwrap().0, last);

    // The point of the whole test: the account is not frozen. One more
    // rotation, and it takes effect like the seventy before it.
    let (inner, to) = build_transition(&ea, &[&ea, &eb], true, T0);
    apply_control_at(
        &ea,
        &mut dba,
        &inner,
        &a_id,
        Hlc::at(T0 + LINKS as u64 + 1),
        u32::try_from(LINKS).unwrap() + 1,
    );
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        to,
        "an account past the old cap can no longer rotate: it is in a state it \
         cannot leave, which is the defect this test exists for"
    );
}

/// An oversized roster is refused, and refused *before* the verification it
/// would have paid for.
///
/// `roster` and `device_shares` are `Vec`s in a payload, so their length is
/// chosen by whoever wrote the op, and each roster entry costs a `DeviceCert`
/// decode plus an Ed25519 verification. Nothing bounded either, so one op could
/// buy arbitrary verification work on every peer in the account.
///
/// The oversized transition is **completely valid** apart from its size:
/// [`build_transition_naming`] mints the successor itself, so every roster
/// entry is a distinct cert signed by it, every share is a real HPKE seal to a
/// distinct key, both digests are recomputed over the real contents and both
/// signatures are genuine. Nothing but the length check can refuse it — which
/// is the only way to tell "refused for its size" apart from "refused because a
/// padded payload is malformed", and the only construction under which removing
/// the check turns this test red.
#[test]
fn a_transition_naming_more_devices_than_the_cap_is_refused() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    let oversized = build_transition_naming(&ea, MAX_ROSTER_ENTRIES + 1, T0);
    apply_control_at(&ea, &mut dba, &oversized, &a_id, Hlc::at(T0 + 1), 1);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "the oversized transition moved the head"
    );
    let rows: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0, "the oversized transition was stored");

    // And the cap is a cap rather than a refusal of everything: the same
    // transition at the cap is accepted. Without this the test above would pass
    // against a build that refused every transition.
    let (ok, to) = build_transition(&ea, &[&eb], true, T0);
    apply_control_at(&ea, &mut dba, &ok, &a_id, Hlc::at(T0 + 2), 1);
    assert_eq!(ea.current_identity(dba.conn()).unwrap().identity_id, to);
}

/// The fold is willing to look at every row ingest will store.
///
/// `apply_control_op` is the only writer of `identity_transitions`, and its
/// sibling cap lets a predecessor reach exactly
/// [`MAX_SIBLINGS_PER_PREDECESSOR`] rows. If the fold's `LIMIT` were smaller
/// than that, the rows past it would be ones the walk could never reach on a
/// replica that had already stored them — no rotation recorded there could
/// ever take effect, which is `MAX_TRANSITION_CHAIN = 64` all over again, one
/// level down.
///
/// So the load-bearing property of [`MAX_SIBLING_CANDIDATES`] is not its value
/// but this inequality, and this is the test that makes the constant causal:
/// lowering it below the ingest cap turns this red. Its *behaviour* — scanning
/// past a row that does not verify — is not exercised anywhere, because every
/// row stored under an established predecessor had `prev_sig` checked at
/// ingest and therefore verifies here too. See `MAX_SIBLING_CANDIDATES` for
/// the measurement and the one path that could store a row the fold must skip.
#[test]
fn the_fold_looks_at_every_row_ingest_will_store() {
    let ingest_cap = usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR)
        .expect("the sibling cap is a small positive number");
    assert!(
        MAX_SIBLING_CANDIDATES >= ingest_cap,
        "the fold verifies at most {MAX_SIBLING_CANDIDATES} candidates per link but ingest \
         will store {ingest_cap} rows under one predecessor, so {} of them are rows the walk \
         can never reach and the transitions they carry can never take effect",
        ingest_cap - MAX_SIBLING_CANDIDATES
    );
}

/// One predecessor may accumulate only so many successors, and a re-delivery
/// of a row already held is never what the cap turns away.
///
/// The fold verifies at most `MAX_SIBLING_CANDIDATES` rows per link, so a
/// seventeenth row under one predecessor would be one the walk could never
/// reach -- the same shape the chain cap had, one level down. Ingest holds the
/// two numbers equal so that cannot arise.
#[test]
fn a_predecessor_accumulates_only_as_many_successors_as_the_fold_will_verify() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();

    // Every one of these succeeds the *same* predecessor: `build_transition`
    // reads `emitter.keychain.identity_id()`, and that only moves when this
    // device *adopts*, which since #105 needs a cert for itself in the
    // transition's roster. The roster here is B alone, so A never adopts and
    // every call forks from genesis -- which is exactly the fan this test
    // wants, and is why the roster deliberately does not name A the way
    // `a_chain_past_the_old_cap_still_folds_and_can_still_be_extended` does.
    let mut built = Vec::new();
    for i in 0..MAX_SIBLINGS_PER_PREDECESSOR + 4 {
        let (inner, to) = build_transition(&ea, &[&eb], true, T0);
        built.push((inner, to, i));
    }
    // A fresh replica, so the emitter's own adoption does not move the
    // predecessor between deliveries.
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbc = db_root(ROOT);
    trust(&ec, &mut dbc, &ea);
    trust(&ec, &mut dbc, &eb);
    for (inner, _, i) in &built {
        apply_control_at(
            &ec,
            &mut dbc,
            inner,
            &a_id,
            Hlc::at(T0 + u64::try_from(*i).unwrap() + 1),
            1,
        );
    }

    let rows: i64 = dbc
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        rows, MAX_SIBLINGS_PER_PREDECESSOR,
        "a predecessor accumulated {rows} successors; the fold verifies at most {MAX_SIBLINGS_PER_PREDECESSOR}"
    );

    // Re-delivering a row already held is not a new sibling and must not be
    // turned away, or evict anything, at a full register -- the insert is OR
    // IGNORE and the row occupies no fresh place. The *last* one, because these
    // were delivered by ascending HLC and a full register now keeps the
    // greatest sixteen by the fold's order rather than the first sixteen to
    // arrive (#232): `built[0]` is the one the cap dropped, so re-delivering it
    // would assert nothing about re-delivery.
    let (last, last_to, last_i) = built.last().expect("twenty transitions were built");
    let held_before: i64 = dbc
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
            params![&last_to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        held_before, 1,
        "the row about to be re-delivered is not held"
    );
    apply_control_at(
        &ec,
        &mut dbc,
        last,
        &a_id,
        Hlc::at(T0 + u64::try_from(*last_i).unwrap() + 1),
        1,
    );
    let after: i64 = dbc
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(after, rows, "a re-delivery changed the row count");
    let held_after: i64 = dbc
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
            params![&last_to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        held_after, 1,
        "a re-delivery evicted the row it re-delivered"
    );
}

/// **[`SiblingRank`] and [`SIBLING_ORDER_DESC`] are the same order.**
///
/// Ingest decides in Rust, over a `derive(Ord)` whose field order is the
/// comparison; the fold decides in SQL, over a string. The whole safety
/// argument for [`MAX_SIBLING_CANDIDATES`] is that the row ingest discards is
/// one the fold would never have reached, and that argument is only true while
/// the two agree — a reordered field or a reworded `ORDER BY` would break it
/// with no other symptom than a stored row the walk cannot see.
///
/// Six rows off a predecessor this replica will never establish, differing in
/// one rank component each, sorted by SQLite and by `Ord` and compared. The
/// emitter and the HLC are free parameters of `apply_control_op`, so every
/// component but `to_identity_id` — which is minted inside the fixture — is
/// varied deliberately rather than incidentally.
#[test]
fn the_rank_type_and_the_sql_order_agree() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    // Never a link on this replica, so `prev_sig` is not checked and no row is
    // ever swept: the table keeps exactly what is put in it.
    let absent = [0x7Eu8; 16];

    let variants: [(u32, u64, u32, [u8; 16]); 6] = [
        (1, T0, 0, [0x11; 16]),
        (1, T0, 0, [0x22; 16]),
        (1, T0, 1, [0x11; 16]),
        (1, T0 + 1, 0, [0x11; 16]),
        (2, T0 - 5, 0, [0x00; 16]),
        (2, T0, 0, [0xFF; 16]),
    ];
    for (i, (meta_epoch, physical_ms, logical, emitter)) in variants.iter().enumerate() {
        let (inner, _) = forge_sibling(absent, 0x5A47 + u64::try_from(i).unwrap());
        apply_control_at(
            &ea,
            &mut dba,
            &inner,
            emitter,
            Hlc {
                physical_ms: *physical_ms,
                logical: *logical,
            },
            *meta_epoch,
        );
    }

    let by_sqlite: Vec<SiblingRank> = {
        let conn = dba.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id,
                        to_identity_id
                 FROM identity_transitions
                 WHERE from_identity_id = ?1
                 ORDER BY {SIBLING_ORDER_DESC}"
            ))
            .unwrap();
        let rows = stmt
            .query_map(params![&absent[..]], |r| {
                Ok(SiblingRank {
                    meta_epoch: r.get(0)?,
                    hlc_physical_ms: r.get(1)?,
                    hlc_logical: r.get(2)?,
                    emitter_device_id: r.get(3)?,
                    to_identity_id: r.get(4)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    };
    assert_eq!(
        by_sqlite.len(),
        variants.len(),
        "the fixture did not store one row per variant"
    );

    let mut by_ord = by_sqlite.clone();
    by_ord.sort();
    by_ord.reverse();
    assert_eq!(
        by_sqlite, by_ord,
        "SQLite's `ORDER BY {SIBLING_ORDER_DESC}` and `SiblingRank`'s derived `Ord` \
         disagree, so ingest can discard a row the fold would have read"
    );
}

/// A sibling of `from_identity_id` whose `prev_sig` is **arbitrary** — the row
/// a peer implementing the published wire format can write for a predecessor it
/// holds no key for.
///
/// Deliberately not built through [`build_transition`] or
/// `sunrise_crypto::sign_identity_transition`. Both take the *predecessor's*
/// keypair and refuse a body whose `from_identity_id` is not its derivation, so
/// neither can express the one payload that matters here. Everything else an
/// emitter needs is public: `docs/03-crypto/key-rotation.md` gives the two
/// signing inputs verbatim, and this reconstructs `next_sig`'s from that
/// document rather than from the code under test — so the fixture is not
/// self-consistent with the ingest path it is aimed at, and a change to the
/// domain string turns it red against the format document.
///
/// The roster and the share list are **empty**, which is what makes this the
/// cheapest row an adversary can place: no cert to issue, no HPKE seal to
/// compute, one Ed25519 signature over about two hundred bytes.
fn forge_sibling(from_identity_id: [u8; 16], seed: u64) -> (InnerOp, [u8; 16]) {
    use rand_core::SeedableRng;
    use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};

    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let to_s = IdentitySigningKeyPair::generate(&mut rng);
    let to_d = IdentityDhKeyPair::generate(&mut rng);
    let to_identity_id = sunrise_crypto::identity_id_from_pub(&to_s.public_bytes());
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id,
        to_identity_id,
        to_id_s_pub: to_s.public_bytes(),
        to_id_d_pub: to_d.public_bytes(),
        roster_digest: sunrise_crypto::roster_digest::<&[u8]>(&[]).expect("empty roster digest"),
        shares_digest: sunrise_crypto::shares_digest::<&[u8]>(&[], None)
            .expect("empty shares digest"),
    };
    // Chosen freely. Nothing the receiving replica can check covers it while
    // the predecessor is unknown, and `next_sig` below is taken *over* it, so
    // the payload is internally consistent whatever it says.
    let prev_sig = [0xA5u8; 64];
    let mut input = Vec::from(&b"sunrise.identity_transition.succ.v1"[..]);
    input.extend_from_slice(
        &sunrise_crypto::identity_transition::body_hash(&body).expect("body hash"),
    );
    input.extend_from_slice(&prev_sig);
    let next_sig = to_s.sign(&input);
    (
        InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
            from_identity_id,
            to_identity_id,
            to_id_s_pub: body.to_id_s_pub,
            to_id_d_pub: body.to_id_d_pub,
            roster: Vec::new(),
            device_shares: Vec::new(),
            identity_share: None,
            prev_sig,
            next_sig,
        })),
        to_identity_id,
    )
}

/// A replica two links behind, its predecessor's places filled with forgeries,
/// and the honest successor that must still land.
///
/// Returns `(replica, db, forged_ids, honest_transition, honest_id, establishing
/// _transition, predecessor_id)`. The forged rows are sealed at meta epoch 1 and
/// the honest successor at 2, which is the shape `revoke_device` produces and
/// the only thing that separates them: the cut device holds no key above the
/// epoch it was cut at, so it cannot raise `meta_epoch` however it dates its
/// HLC (ADR-0037 §4, pinned by
/// `a_revocations_transition_is_sealed_above_the_epoch_the_cut_device_holds`).
struct SuppressionFixture {
    replica: Engine,
    db: Db,
    emitter: [u8; 16],
    forged: Vec<[u8; 16]>,
    establishing: InnerOp,
    predecessor: [u8; 16],
    honest: InnerOp,
    honest_id: [u8; 16],
}

/// Build [`SuppressionFixture`], asserting the fixture is the shape it claims.
///
/// The assertions are the point. `build_transition` reads
/// `emitter.keychain.identity_id()`, which only moves when the emitter
/// *adopts*, so a caller that forgets to put the emitter in its own roster gets
/// a fan of siblings off genesis and calls it a chain — the failure its own doc
/// comment warns about and one this suite has shipped before. So this checks
/// what the two transitions actually name rather than what they were meant to.
fn suppression_fixture() -> SuppressionFixture {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let genesis = ea.current_identity(dba.conn()).unwrap().identity_id;

    // G -> P, applied to the emitter so it adopts P and its next transition
    // succeeds P rather than G.
    let (establishing, predecessor) = build_transition(&ea, &[&ea, &eb], true, T0);
    apply_control_at(&ea, &mut dba, &establishing, &a_id, Hlc::at(T0 + 1), 1);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        predecessor,
        "the emitter did not adopt the predecessor, so the fixture is a fan and not a chain"
    );

    // P -> Q, the honest successor. On the revocation path this is the op that
    // makes a cut stick.
    let (honest, honest_id) = build_transition(&ea, &[&ea, &eb], true, T0);
    let InnerOp::IdentityTransition(p) = &honest else {
        unreachable!("build_transition returns an identity transition")
    };
    assert_eq!(
        p.from_identity_id, predecessor,
        "the honest successor does not name the predecessor the forgeries target"
    );
    assert_ne!(predecessor, genesis, "the predecessor is genesis");

    // A replica that has seen neither link. It knows both devices, so their
    // envelopes verify, and it is at genesis, so `P` is a predecessor it cannot
    // check a `prev_sig` against — the ordinary state of a replica catching up
    // on a chain it is receiving out of order.
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbc = db_root(ROOT);
    trust(&ec, &mut dbc, &ea);
    trust(&ec, &mut dbc, &eb);
    assert_eq!(
        ec.current_identity(dbc.conn()).unwrap().identity_id,
        genesis,
        "the receiving replica already knows the predecessor"
    );

    let forged = (0..MAX_SIBLINGS_PER_PREDECESSOR)
        .map(|i| {
            let (inner, to) = forge_sibling(predecessor, 0x5EED + u64::try_from(i).unwrap());
            apply_control_at(
                &ec,
                &mut dbc,
                &inner,
                &a_id,
                Hlc::at(T0 + 100 + u64::try_from(i).unwrap()),
                1,
            );
            to
        })
        .collect::<Vec<_>>();
    let stored: i64 = dbc
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE from_identity_id = ?",
            params![&predecessor[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, MAX_SIBLINGS_PER_PREDECESSOR,
        "the forged siblings did not reach the table, so this fixture is not \
         reproducing the suppression at all"
    );

    SuppressionFixture {
        replica: ec,
        db: dbc,
        emitter: a_id,
        forged,
        establishing,
        predecessor,
        honest,
        honest_id,
    }
}

/// **Sixteen forged siblings of a predecessor this replica has not established
/// cannot turn the honest successor away.**
///
/// The `prev_sig` check at ingest runs only when the predecessor is already on
/// this replica's chain — it has to, because a replica must accept a chain out
/// of order and has no key to check against until it catches up. The sibling
/// cap below it counted *rows*, so every row admitted in that window, verified
/// or not, spent one of a predecessor's sixteen places. Sixteen of the cheapest
/// possible forgeries filled them, and the real successor was refused; the
/// refusal was permanent, because the op is recorded in `ops` and never
/// re-offered, and the account could then never adopt a successor — which, since
/// a revocation *is* a rotation, means it could never cut a device again either.
///
/// What replaces the count is a *rank*: the places are held by the sixteen
/// greatest rows in the fold's own order, and a seventeenth is admitted by
/// displacing the weakest. The honest successor is sealed at the meta epoch its
/// rotation minted, which the forger provably does not hold, so it outranks all
/// sixteen and takes a place whatever order the rows arrive in.
#[test]
fn forged_siblings_cannot_suppress_the_successor_of_an_unestablished_predecessor() {
    let mut f = suppression_fixture();

    // The honest successor, at the epoch its own rotation minted.
    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.honest,
        &f.emitter,
        Hlc::at(T0 + 2),
        2,
    );
    let admitted: i64 =
        f.db.conn()
            .query_row(
                "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
                params![&f.honest_id[..]],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        admitted, 1,
        "the honest successor was refused by a register full of rows nothing had verified; \
         this replica can never adopt it, and never cut a device again"
    );

    // And it is the account's identity once the missing link lands.
    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.establishing,
        &f.emitter,
        Hlc::at(T0 + 3),
        1,
    );
    assert_eq!(
        f.replica.current_identity(f.db.conn()).unwrap().identity_id,
        f.honest_id,
        "the chain did not reach the honest successor"
    );
}

/// **Establishing a predecessor deletes the rows stored under it that verify
/// against nothing, and frees their places.**
///
/// The converging half. A replica that admitted forgeries while it was behind
/// repairs itself the moment it catches up: `from_identity_id` determines the
/// key those rows have to verify under — the id is the derivation of that key —
/// so a row that fails once fails forever, and deleting it loses nothing the
/// fold could ever have used. Nothing survives the catch-up, and a successor
/// delivered *after* it is judged against an empty register.
///
/// Without this the sixteen dead rows would sit under the predecessor for the
/// life of the account, costing the fold sixteen Ed25519 verifications at that
/// link on every call rather than the one an honest link costs.
#[test]
fn establishing_a_predecessor_clears_the_siblings_that_verify_against_nothing() {
    let mut f = suppression_fixture();

    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.establishing,
        &f.emitter,
        Hlc::at(T0 + 3),
        1,
    );
    assert_eq!(
        f.replica.current_identity(f.db.conn()).unwrap().identity_id,
        f.predecessor,
        "the missing link did not establish the predecessor"
    );
    let survivors: Vec<Vec<u8>> = {
        let conn = f.db.conn();
        let mut stmt = conn
            .prepare("SELECT to_identity_id FROM identity_transitions WHERE from_identity_id = ?")
            .unwrap();
        let rows = stmt
            .query_map(params![&f.predecessor[..]], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    };
    for forged in &f.forged {
        assert!(
            !survivors.iter().any(|s| s.as_slice() == &forged[..]),
            "a row that verifies against the established predecessor's key under no \
             signature is still holding one of its places"
        );
    }
    assert!(
        survivors.is_empty(),
        "the predecessor kept {} rows none of which it signed",
        survivors.len()
    );

    // And the register it freed is a register the honest successor fits in.
    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.honest,
        &f.emitter,
        Hlc::at(T0 + 4),
        2,
    );
    assert_eq!(
        f.replica.current_identity(f.db.conn()).unwrap().identity_id,
        f.honest_id,
        "the honest successor could not land after the catch-up that should have \
         freed every place the forgeries took"
    );
}

/// **What ingest keeps is what the fold reads, and neither depends on arrival
/// order.**
///
/// The replacement for a count. `MAX_SIBLING_CANDIDATES >= MAX_SIBLINGS_PER_PREDECESSOR`
/// says the fold is *willing* to look at every row ingest will store, which is
/// necessary and was never sufficient: while ingest kept the first sixteen rows
/// to arrive and the fold read the sixteen greatest, two replicas holding the
/// same ops could hold different rows and fold to different heads. They now use
/// one order, so the set ingest retains is exactly the set the fold reads, and
/// it is a function of the op set rather than of the delivery order.
///
/// Twenty valid transitions off one predecessor, delivered worst-rank-first and
/// then best-rank-first, must leave the same sixteen rows both times — the four
/// weakest gone in both.
#[test]
fn what_ingest_keeps_is_what_the_fold_reads_whatever_order_it_arrives_in() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();

    // A fan off genesis: the roster names B alone, so A never adopts and every
    // call succeeds the same predecessor. Ranked by HLC, which is the component
    // an emitter chooses.
    let offered = usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR).unwrap() + 4;
    let built: Vec<(InnerOp, [u8; 16])> = (0..offered)
        .map(|_| build_transition(&ea, &[&eb], true, T0))
        .collect();

    let retained = |order: Vec<usize>| -> BTreeSet<Vec<u8>> {
        let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dbc = db_root(ROOT);
        trust(&ec, &mut dbc, &ea);
        trust(&ec, &mut dbc, &eb);
        for i in order {
            apply_control_at(
                &ec,
                &mut dbc,
                &built[i].0,
                &a_id,
                Hlc::at(T0 + 1 + u64::try_from(i).unwrap()),
                1,
            );
        }
        let conn = dbc.conn();
        let mut stmt = conn
            .prepare("SELECT to_identity_id FROM identity_transitions")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .unwrap();
        rows
    };

    let ascending = retained((0..offered).collect());
    let descending = retained((0..offered).rev().collect());
    assert_eq!(
        ascending.len(),
        usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR).unwrap(),
        "the predecessor kept a number of rows the fold's LIMIT does not match"
    );
    assert_eq!(
        ascending, descending,
        "two replicas holding the same twenty transitions kept different sixteen of \
         them, so which identity the account folds to depends on delivery order"
    );
    // The four the cap turned away are the four weakest, in both orders.
    let expected: BTreeSet<Vec<u8>> = built
        [offered - usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR).unwrap()..]
        .iter()
        .map(|(_, to)| to.to_vec())
        .collect();
    assert_eq!(
        ascending, expected,
        "the rows kept are not the greatest sixteen in the order the fold reads them"
    );
}

/// A transition carrying another transition's signature pair occupies no row.
///
/// The fifth structural refusal, alongside the four in
/// `a_malformed_transition_is_dropped_and_moves_nothing`: `next_sig` covers
/// `body_hash || prev_sig`, so a pair lifted whole from a different transition
/// is internally consistent and still fits no other body. Without the check the
/// row is occupiable by anyone — `INSERT OR IGNORE` is keyed on
/// `to_identity_id`, so a forged copy published first would discard the honest
/// one for good — and with the sibling cap it would also consume one of the
/// predecessor's sixteen places.
#[test]
fn a_transition_carrying_another_transitions_signatures_is_not_stored() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    let (inner, _) = build_transition(&ea, &[&eb], true, T0);
    let InnerOp::IdentityTransition(mut p) = inner else {
        unreachable!()
    };
    // Both signatures from a *different* transition of the same shape: the pair
    // is internally consistent and belongs to another body, which is the shape
    // a substitution attack takes.
    let (other, _) = build_transition(&ea, &[&eb], true, T0);
    let InnerOp::IdentityTransition(other) = other else {
        unreachable!()
    };
    p.prev_sig = other.prev_sig;
    p.next_sig = other.next_sig;
    let forged = InnerOp::IdentityTransition(p);

    apply_control_at(&ea, &mut dba, &forged, &a_id, Hlc::at(T0 + 1), 1);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "the forged transition moved the head"
    );
    let rows: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        rows, 0,
        "a transition signed for another body was stored anyway"
    );
}

/// A revocation that cannot rotate every stream succeeds **and says which it
/// could not**.
///
/// `rotation_set` drops a `stream_id` column that is not 16 bytes, because
/// there is genuinely no stream to mint an epoch for and padding one out would
/// file a stranger's key against the vault-meta stream. What was wrong is that
/// it dropped it *silently*: the revoked device goes on holding whatever key it
/// was last given for that row, and `revoke_device` returned a plain success.
///
/// Raising is not the alternative — the device being revoked is often the one
/// that is gone, so failing the whole operation is the worse answer. The third
/// option is the one issue #160 already took one level up for the relay half:
/// succeed partially and disclose it.
///
/// So this asserts both halves, and the first is what stops it from passing
/// against a build that simply refuses the revocation.
#[test]
fn a_revocation_that_cannot_rotate_every_stream_says_which_it_could_not() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    // A real stream with a real key, then its id truncated in place. The row
    // survives, names nothing, and is what the rotation cannot reach.
    let doomed = [0x4du8; 16];
    dba.with_tx(|tx| {
        ea.keychain.mint_epoch(tx, &doomed, &SystemRng, T0)?;
        Ok(())
    })
    .expect("mint the stream that is about to be corrupted");
    dba.conn()
        .execute(
            "UPDATE stream_keys SET stream_id = X'00aabb' WHERE stream_id = ?",
            params![&doomed[..]],
        )
        .unwrap();

    let before = meta_epoch_now(&dba);
    let out = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, b_id),
                reason: RevokeReason::Stolen,
            },
        )
        .expect("the revocation must still succeed: the device that is gone is the scenario");

    // Half one: the rest of the revocation really happened.
    assert!(
        meta_epoch_now(&dba) > before,
        "the vault-meta stream was not rotated, so this is not a partial success \
         but a failure wearing one"
    );

    // Half two: and it is not quiet about the row it could not reach.
    assert_eq!(
        out.unrotated_streams,
        vec!["00aabb".to_string()],
        "the revocation reported success without naming the stream it could not \
         rotate, which the revoked device can still read"
    );
}

/// The vault-meta stream's live epoch, as a revocation moves it.
fn meta_epoch_now(db: &Db) -> i64 {
    db.conn()
        .query_row(
            "SELECT COALESCE(MAX(epoch), 0) FROM stream_keys WHERE stream_id = ?",
            params![&META_STREAM[..]],
            |r| r.get(0),
        )
        .unwrap()
}

/// A revocation with nothing wrong reports nothing, so the field above is a
/// signal rather than noise every caller learns to ignore.
#[test]
fn an_ordinary_revocation_reports_no_unrotated_streams() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let out = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
                reason: RevokeReason::Lost,
            },
        )
        .expect("revoke");
    assert!(out.unrotated_streams.is_empty());
    assert!(
        !out.revocation_gated,
        "and an ordinary revocation is not reported as discarded"
    );
}

/// **A revocation the fold discards is not reported as a success.**
///
/// `apply_control_op` returns `Ok(())` whether the op was folded into the
/// register or stored and skipped, so until this read `revoke_device` could
/// not tell the two apart and neither could its caller. The failure that
/// followed was not a missing log line. With no `device_revocations` row,
/// `emit_key_envelopes`' anti-join does not exclude the target, so the fresh
/// epoch this command mints for every stream is sealed *to the device it
/// claims to have revoked* — while the relay intent drained and the relay
/// 401'd it. The user was told it worked, every device list went on showing
/// the device as current, it kept receiving new keys, and the relay refused
/// it. Three surfaces disagreeing about one act.
///
/// The op is still emitted, still stored, and still re-judged whenever another
/// revocation lands, and the rotation still runs — the target is a recipient
/// of the new epochs either way, the register saying it is current. What
/// changes is the claim and the relay's half. `log-events.md`'s own
/// `core.device.revoke_incomplete` row states the rule: an operator's NDJSON
/// is not sufficient, the same facts must reach the caller.
#[test]
fn revoke_device_reports_that_the_fold_discarded_its_own_op() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&ea, &mut dba, &ec);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // This vault is itself revoked, by an op it has absorbed from B.
    revoke(&ea, &mut dba, &eb, a_id, T0);
    assert!(ea.is_revoked(dba.conn(), &a_id).unwrap());

    // And it now tries to revoke C anyway, which is the whole scenario: a
    // compromised device expelling the rest of the account.
    let out = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, c_id),
                reason: RevokeReason::Compromised,
            },
        )
        .expect("the command still succeeds: the op is emitted before the fold judges it");

    assert!(
        out.revocation_gated,
        "a command whose op the account discarded must not report a plain success"
    );
    assert!(
        !ea.is_revoked(dba.conn(), &c_id).unwrap(),
        "and nothing was actually revoked"
    );
    assert!(
        !pending_relay_revocations(&dba).contains(&c_id),
        "telling the relay to cut a device every replica still shows as current is \
         the disclosure failure #160 fixed in the other direction"
    );
}

/// `Command::RotateStreamKey` is the one-stream form of the mint-and-distribute
/// chain `revoke_device` keeps a revoked device out of, and it refuses the same
/// device outright.
///
/// Without the guard, a device its own register calls revoked mints a fresh
/// epoch into its own `stream_keys` and seals it to every unbounded peer, whose
/// `absorb_stream_key` takes it and whose next write is sealed under it. The
/// refusal is checked before anything can mint, so the failed command leaves
/// no key, no op, and no envelope behind. The positive half — an unrevoked
/// device rotates and every current peer is sealed the new epoch — is
/// `a_requested_rotation_keeps_every_current_device` and its neighbours.
#[test]
fn a_revoked_device_cannot_rotate_a_stream_key() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let b_id = eb.keychain.device_id();

    // This vault is itself revoked, by an op it has absorbed from B.
    revoke(&ea, &mut dba, &eb, a_id, T0);
    assert!(ea.is_revoked(dba.conn(), &a_id).unwrap());

    let inbox_keys = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM stream_keys WHERE stream_id = ?",
                params![&INBOX_STREAM_BYTES[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    let ops = |db: &Db| -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap()
    };
    let keys_before = inbox_keys(&dba);
    let ops_before = ops(&dba);
    let to_b_before = envelopes_to(&ea, &dba, &b_id).len();

    let err = ea
        .apply(
            &mut dba,
            Command::RotateStreamKey {
                stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
            },
        )
        .expect_err("a revoked device must not mint an epoch every honest peer adopts");
    assert!(
        matches!(err, EngineError::Invalid(_)),
        "refused as a policy violation, not a storage failure: {err:?}"
    );

    assert_eq!(
        inbox_keys(&dba),
        keys_before,
        "no epoch was minted into this device's own keychain"
    );
    assert_eq!(ops(&dba), ops_before, "and nothing was emitted");
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        to_b_before,
        "so no honest peer was sealed a key this device chose"
    );
}

/// The epoch separation the ordering argument rests on, asserted against the
/// **production** path rather than against hand-chosen epochs.
///
/// `a_higher_meta_epoch_beats_any_hlc` below applies one transition at epoch 1
/// and the other at epoch 2 and shows the fold prefers the higher. That pins
/// the comparison and nothing else: it says what happens *if* the honest
/// rotation outranks the revoked device's, and takes the "if" as given. The
/// "if" is the security claim.
///
/// Nothing in this engine's types produces it. It is an emergent property of
/// two transactions in `revoke_device`: the register is written, every stream
/// in `rotation_set` — the vault-meta stream among them — is minted forward and
/// sealed to the unrevoked devices only, that transaction commits, and *then*
/// `rotate_identity` runs and seals the transition under whatever epoch is live,
/// which is now E+1. Reorder those two and the argument collapses silently: the
/// transition ties the revoked device's own possible rows on `meta_epoch`, and
/// `hlc_physical_ms` decides — a value that device chooses freely inside
/// `MAX_DRIFT_MS`.
///
/// The comment in `rotate_identity` asserted the collapsed version ("sealed
/// under the epoch the departing device still shares") until this test was
/// written, which is exactly how much a comment is worth here.
#[test]
fn a_revocations_transition_is_sealed_above_the_epoch_the_cut_device_holds() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .expect("the revocation runs");

    // Read the epoch each op was sealed under off its own envelope, which is
    // what a peer reads.
    let sealed_at = |kind: &str| -> u32 {
        let env: Vec<u8> = dba
            .conn()
            .query_row(
                "SELECT envelope FROM ops WHERE inner_kind = ?1 AND stream_id = ?2",
                params![kind, &META_STREAM[..]],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("one {kind} op in the vault-meta stream: {e}"));
        sunrise_crypto::decode_envelope(&env)
            .expect("the op this engine just wrote decodes")
            .epoch
    };

    // The revocation itself is deliberately sealed at the epoch the departing
    // device still holds -- it has to be readable by every peer that has not
    // yet received the new one. That is the ceiling on what the cut device can
    // ever seal under.
    let cut_epoch = sealed_at("device.revoke");
    let transition_epoch = sealed_at("identity.transition");
    assert!(
        transition_epoch > cut_epoch,
        "the rotation that makes the revocation stick was sealed at meta epoch \
         {transition_epoch}, which does not outrank the epoch {cut_epoch} the cut device \
         still holds; ADR-0037 §4's ordering argument needs it strictly above"
    );

    // And the ceiling is real: the revoked device was sealed no key at or above
    // the epoch the transition sits at, so it cannot produce a competing row
    // there however it dates its HLC.
    let served_at_or_above: Vec<u32> = key_envelope_envs(&dba)
        .into_iter()
        .filter_map(|env| ea.keychain.open_op(&env).ok())
        .filter_map(|cbor| decode_inner_op(&cbor).ok())
        .filter_map(|inner| match inner {
            InnerOp::KeyEnvelope(p)
                if p.stream_id == META_STREAM
                    && p.recipient == Recipient::Device(b_id)
                    && p.epoch >= transition_epoch =>
            {
                Some(p.epoch)
            }
            _ => None,
        })
        .collect();
    assert!(
        served_at_or_above.is_empty(),
        "the revoked device was handed vault-meta keys at {served_at_or_above:?}, at or \
         above the epoch its own revocation's rotation was sealed under"
    );
}

/// **`meta_epoch` sorts first, and that is the security component.**
///
/// Two transitions succeed the same identity. The attacker's carries an
/// HLC far ahead of anything honest — the shape a forward-dated op takes,
/// bounded only by `MAX_DRIFT_MS` — and is sealed under the meta epoch it
/// still holds. The honest one carries a small HLC and a higher epoch.
///
/// The honest one wins, and not because it is honest: it wins because a
/// revoked device provably cannot raise `meta_epoch`. `revoke_device`
/// writes the revocation register *before* it mints, so
/// `emit_key_envelopes`' anti-join excludes that device from every epoch
/// minted in the same transaction, and it holds no key above the one it was
/// cut at. An HLC is a claim; an epoch is a key you either have or do not.
#[test]
fn a_higher_meta_epoch_beats_any_hlc() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let (attacker, attacker_to) = build_transition(&ea, &[&eb], false, T0);
    let (honest, honest_to) = build_transition(&ea, &[&eb], true, T0);
    let a_id = ea.keychain.device_id();

    // Forward-dated to just inside the drift gate, at the old epoch.
    apply_control_at(
        &ea,
        &mut dba,
        &attacker,
        &a_id,
        Hlc::at(T0 + sunrise_cbor::hlc::MAX_DRIFT_MS - 1),
        1,
    );
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        attacker_to,
        "with nothing to compete against it, it does win"
    );

    // Backdated ten days, at the next epoch. The epoch decides.
    apply_control_at(&ea, &mut dba, &honest, &a_id, Hlc::at(T0 - 864_000_000), 2);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        honest_to,
        "a higher meta_epoch wins however far the loser's clock was pushed"
    );
}

/// Two competing transitions, applied to two fresh replicas in **opposite
/// orders**, converge byte for byte.
///
/// The property the whole design rests on and the reason nothing is refused
/// at apply time: standing is a pure function of the op set, so arrival
/// order cannot decide who the account is.
#[test]
fn competing_transitions_converge_whatever_order_they_arrive_in() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (first, _) = build_transition(&ea, &[&eb], true, T0);
    let (second, _) = build_transition(&ea, &[&eb], true, T0);
    let a_id = ea.keychain.device_id();

    // Each op carries its *own* stamp, fixed before either is delivered.
    // Deriving it from arrival position instead would make the input a
    // function of the order and the test vacuous — which is what the first
    // version of this did, and what it caught.
    let a = (&first, Hlc::at(T0 + 1));
    let b = (&second, Hlc::at(T0 + 2));

    let chain_after = |order: [(&InnerOp, Hlc); 2]| -> Vec<([u8; 16], [u8; 32])> {
        // A fresh engine per run: the keychain adopts as it folds, and a
        // reused one would carry the first run's adoption into the second.
        let e = engine_random_keys(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut db = db_root(ROOT);
        trust(&e, &mut db, &ea);
        trust(&e, &mut db, &eb);
        for (inner, hlc) in order {
            apply_control_at(&e, &mut db, inner, &a_id, hlc, 1);
        }
        e.chain_identities(db.conn()).unwrap()
    };

    let forwards = chain_after([a, b]);
    let backwards = chain_after([b, a]);
    assert_eq!(forwards.len(), 2, "one of the two won, and only one");
    assert_eq!(
        forwards, backwards,
        "the chain is a function of the op set, not of arrival order"
    );
}

/// The read half of revocation: a rotation seals **no** device envelope to
/// the device it just revoked, and there is no identity copy that device
/// could open instead.
///
/// Both halves are asserted, because either alone is vacuous. Excluding the
/// device from the recipient list withholds nothing while a `PairingPayload`
/// hands it `ID_D_priv` — that was #76, and it is why the exclusion was
/// removed rather than repaired. Dropping `ID_D_priv` from the payload
/// withholds nothing while the device is still on the recipient list. The
/// property only exists when both are true at once.
#[test]
fn a_revoked_device_gets_no_key_for_an_epoch_minted_after_its_cut() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    let before = key_envelope_envs(&dba).len();
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    let minted: Vec<InnerOp> = key_envelope_envs(&dba)
        .into_iter()
        .skip(before)
        .filter_map(|env| ea.keychain.open_op(&env).ok())
        .filter_map(|cbor| decode_inner_op(&cbor).ok())
        .collect();
    assert!(
        !minted.is_empty(),
        "the revocation rotated something, or this test proves nothing"
    );

    assert!(
        !minted.iter().any(|inner| matches!(
            inner,
            InnerOp::KeyEnvelope(p) if p.recipient == Recipient::Device(b_id)
        )),
        "the rotation sealed a device envelope to the device it just revoked"
    );

    // The identity copy is still emitted -- recovery depends on it -- and
    // is exactly what the revoked device can no longer open, because
    // `PairingPayload` no longer carries `ID_D_priv`. `Identity::dh_secret`
    // is `None` on every device pairing admitted, so the type system is
    // what holds this rather than a runtime check.
    assert!(
        minted.iter().any(|inner| matches!(
            inner,
            InnerOp::KeyEnvelope(p) if matches!(p.recipient, Recipient::Identity(_))
        )),
        "the identity copy must survive: it is the recovery path"
    );
}

/// The same property, on a replica whose HLC sits ahead of its wall clock.
///
/// This is the deterministic form of the leak the exclusion above only
/// *looked* like it provided. `revoke_device` reads `now_ms` off the wall
/// clock before the transaction opens; the cut it writes is
/// `hlc.send().physical_ms`, and once `observe()` has absorbed a peer up to
/// `MAX_DRIFT_MS` ahead that stamp stays above `now_ms` for as long as the
/// skew lasts. The anti-join asked `cut_ms <= now_ms`, which is then false,
/// so the revoked device was sealed a `key_envelope` for every stream its
/// own revocation rotated -- to its own `D_D_pub`, in ops sealed under the
/// *pre*-rotation meta epoch it still holds, with a
/// `key_envelope_recipients` row filed to say it had been served.
///
/// A frozen clock cannot detect this. `Engine::from_clock` derives the HLC
/// from the same `Clock`, so `hlc.physical_ms == now_ms` by construction
/// and the comparison held by coincidence. The `observe` below is what
/// breaks the coincidence, and it is an ordinary event: any peer whose
/// clock leads by any amount inside the drift gate causes it.
///
/// The repair that survives is not a better comparison but no comparison:
/// a recorded row excludes, full stop. This test is kept because it is the
/// regression the original defect deserves — a cut stamped ahead of local
/// time must be effective — and it now passes for a reason no clock can
/// take away. See `a_recorded_cut_survives_a_restart` for the case that
/// killed the comparison outright.
#[test]
fn a_revoked_device_is_excluded_when_the_hlc_leads_the_wall_clock() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    // A peer 200 s ahead: inside `MAX_DRIFT_MS`, so the drift gate accepts
    // it and A's HLC keeps it. Everything A stamps afterwards sits at
    // T0 + 200 s while `clock.now_ms()` still reads T0.
    ea.hlc.observe(Hlc::at(T0 + 200_000)).unwrap();
    assert!(
        ea.hlc.peek().physical_ms > ea.clock.now_ms(),
        "the premise of this test is an HLC ahead of the wall clock"
    );

    let before = key_envelope_envs(&dba).len();
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    // The cut really did land ahead of the wall clock. Without this the
    // anti-join would exclude B for the ordinary reason and the test would
    // prove nothing about the skewed case.
    let (cut_ms, _, _, _) = revocation_row(&dba, &b_id).expect("the cut was recorded");
    assert!(
        u64::try_from(cut_ms).unwrap() > ea.clock.now_ms(),
        "the cut must be ahead of `now_ms`, or this test is the frozen-clock one again"
    );

    let minted: Vec<InnerOp> = key_envelope_envs(&dba)
        .into_iter()
        .skip(before)
        .filter_map(|env| ea.keychain.open_op(&env).ok())
        .filter_map(|cbor| decode_inner_op(&cbor).ok())
        .collect();
    assert!(
        !minted.is_empty(),
        "the revocation rotated something, or this test proves nothing"
    );
    assert!(
        !minted.iter().any(|inner| matches!(
            inner,
            InnerOp::KeyEnvelope(p) if p.recipient == Recipient::Device(b_id)
        )),
        "a revocation stamped by an HLC ahead of the wall clock sealed the new epoch to \
             the device it was revoking"
    );
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "no envelope in the whole log may name the revoked device"
    );

    // And nothing filed a row claiming it had been served, which is what
    // made the leak silent: the row is what `backfill_key_envelopes` reads
    // to decide a device already has the key.
    let served: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM key_envelope_recipients WHERE recipient = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        served, 0,
        "a recipient row was filed for the device the transaction was revoking"
    );
}

/// The republish route is closed on the same timeline, not only the mint.
///
/// `backfill_key_envelopes` skips a revoked device on the same cut test the
/// recipient query uses, so leaving that one on the wall clock while fixing
/// the anti-join would only move the leak: the revoked device re-sends the
/// cert it already has and is handed every key back in one round trip.
#[test]
fn a_revoked_device_is_not_backfilled_when_the_hlc_leads_the_wall_clock() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    // A cut 200 s beyond the wall clock -- what a peer whose clock leads
    // writes, and what the drift gate admits.
    let cut = T0 + 200_000;
    ea.hlc.observe(Hlc::at(cut)).unwrap();
    revoke(&ea, &mut dba, &ea, b_id, cut);
    assert!(
        u64::try_from(revocation_row(&dba, &b_id).expect("the cut was recorded").0).unwrap()
            > ea.clock.now_ms(),
        "the premise: this cut sits ahead of the wall clock, which is what used to read \
             as 'not revoked yet'"
    );
    assert!(
        ea.is_revoked(dba.conn(), &b_id).unwrap(),
        "and it is effective anyway, because the row is the whole test"
    );

    trust_at(&ea, &mut dba, &eb, T0);
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a revoked device recovered its keys by re-sending its cert, because the cut was \
             compared against a wall clock instead of the timeline it was written on"
    );
}

/// **A recorded cut survives a restart.** This is the case that took the
/// time comparison out entirely.
///
/// Any comparison of `cut_ms` against a local reading needs a local reading
/// that is at or above every cut this replica has absorbed. The HLC is
/// exactly that *within one process* — and only there.
/// [`MonotonicHlc`](crate::config::MonotonicHlc) is deliberately not
/// persisted, [`Engine::from_clock`] builds a fresh one at `Hlc::default()`,
/// and `observe()` runs from one place: `apply_remote_all`. Nothing primes
/// the HLC from the op log or from `device_revocations` at open. So a
/// restart leaves the HLC reading 0 against `cut_ms` rows that survived,
/// and any horizon derived from it collapses to the bare wall clock.
///
/// The scenario is ordinary rather than adversarial: a peer inside
/// `MAX_DRIFT_MS` writes a cut ahead of local time, and a backgrounded
/// mobile app is killed and reopened before local time catches up. The
/// revoked device then re-sends the cert it already holds and
/// `backfill_key_envelopes` seals it the current epoch of every stream —
/// without the revoked device having to restart, or do anything but wait.
///
/// The second engine below is that restart: a new `Engine` over the *same*
/// database, which is what `Core::open` constructs.
#[test]
fn a_recorded_cut_survives_a_restart() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], clock.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // A cut 200 s ahead of the wall clock: inside `MAX_DRIFT_MS`, so an
    // ordinary peer writes it and the drift gate admits it.
    let cut = T0 + 200_000;
    ea.hlc.observe(Hlc::at(cut)).unwrap();
    revoke(&ea, &mut dba, &ea, b_id, cut);
    drop(ea);

    // The restart. Same database, same wall clock, brand-new HLC.
    let ea2 = engine_random_keys(ROOT, [1u8; 32], clock.clone());
    assert_eq!(
        ea2.hlc.peek().physical_ms,
        0,
        "the premise: HLC state is not persisted, so a reopened engine starts at zero"
    );
    assert!(
        u64::try_from(
            revocation_row(&dba, &b_id)
                .expect("the cut is still recorded")
                .0
        )
        .unwrap()
            > ea2.clock.now_ms(),
        "and the persisted cut is still ahead of the wall clock"
    );

    // Every comparison that could have been built out of this engine's own
    // state is now below the cut, so any of them would report "not revoked".
    assert!(
        ea2.is_revoked(dba.conn(), &b_id).unwrap(),
        "a recorded revocation must not become ineffective because the process restarted"
    );

    // And the route that actually exploits it stays shut.
    trust_at(&ea2, &mut dba, &eb, T0);
    assert!(
        envelopes_to(&ea2, &dba, &b_id).is_empty(),
        "a revoked device recovered every key by re-sending its cert after the revoking \
             device restarted"
    );
}

/// The revocation's *own* transaction excludes the revokee even on the
/// path that mints the vault-meta stream's first epoch.
///
/// `revoke_device` calls `ensure_stream_epoch(META)` to find the epoch its
/// ops are sealed under, and on an account whose meta stream has no key yet
/// that call **mints** one and emits a `key_envelope` per recipient. That
/// mint happens inside the revoking transaction and does not go through the
/// rotation loop, so it is a second, easily-missed way for the revoked
/// device to be handed a key by its own revocation.
///
/// What makes it safe is ordering, and only ordering: the register row is
/// written before anything can mint, so the recipient query already sees it.
/// This test exists because that ordering is a property nothing else pins —
/// every other revocation test starts from a vault that already has a meta
/// key, so `ensure_stream_epoch` returns early and the mint never runs.
#[test]
fn the_first_meta_mint_inside_a_revocation_excludes_the_revokee() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    // Admit B and write nothing else, so the meta stream still has no key
    // and the first mint will happen inside the revocation below.
    trust(&ea, &mut dba, &eb);
    let meta_keys: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM stream_keys WHERE stream_id = ?",
            params![&META_STREAM[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        meta_keys, 0,
        "the premise: the vault-meta stream has no key yet, so revoking mints its first"
    );

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    assert!(
        dba.conn()
            .query_row(
                "SELECT count(*) FROM stream_keys WHERE stream_id = ?",
                params![&META_STREAM[..]],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0,
        "the revocation did mint the meta key, or this test proves nothing"
    );
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "the first meta mint inside a revocation sealed an envelope to the device the \
             transaction exists to revoke"
    );
}

/// Revoking a device leaves `devices.revoked_at_ms` NULL, because the
/// register lives in `device_revocations` and that column is superseded.
///
/// Migration 0017 says so in a comment. This makes it a fact: anything that
/// starts writing the old column again fails here rather than creating a
/// second, silently disagreeing source of truth.
#[test]
fn revocation_does_not_touch_the_superseded_devices_column() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let target = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, target),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    assert!(
        revocation_row(&dba, &target).is_some(),
        "the register is where the revocation lives"
    );
    let legacy: Option<i64> = dba
        .conn()
        .query_row(
            "SELECT revoked_at_ms FROM devices WHERE device_id = ?",
            params![&target[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, None, "the 0013 column is superseded and unwritten");
    // And the device list reads the register, not the dead column.
    match ea.query(&dba, Query::DeviceList).unwrap() {
        QueryResult::Devices(rows) => assert!(
            rows.iter().find(|d| d.device_id == target).unwrap().revoked,
            "the device list still shows it revoked"
        ),
        other => panic!("expected Devices, got {other:?}"),
    }
}

/// Two devices revoke each other, and both replicas end up holding both
/// revocations whichever order they see them in.
///
/// This goes through `apply_remote` — real envelopes, the whole delivery
/// path — because the defect lived in a step the direct-`apply_control_op`
/// helpers skip entirely. The revocation gate refused *every* family from a
/// revoked sender, so a replica that applied B's revocation of A first then
/// refused A's revocation of B, and never learned of it: on that replica a
/// compromised device stays a member, silently, forever, while a replica
/// that saw them the other way round holds both.
#[test]
fn two_devices_revoking_each_other_converge_on_both_revocations() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], clock());
    let eb = engine_seeded(ROOT, [2u8; 32], clock());
    let x = engine_seeded(ROOT, [4u8; 32], clock());
    let y = engine_seeded(ROOT, [5u8; 32], clock());
    let (mut dba, mut dbb, mut dbx, mut dby) =
        (db_root(ROOT), db_root(ROOT), db_root(ROOT), db_root(ROOT));
    // Each of A and B knows the other, so each can revoke it; both replicas
    // know both.
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    for (e, db) in [(&x, &mut dbx), (&y, &mut dby)] {
        trust(e, db, &ea);
        trust(e, db, &eb);
    }
    let (a_id, b_id) = (ea.keychain.device_id(), eb.keychain.device_id());

    // The crossing: each revokes the other, neither having seen the other's op.
    let a_revokes_b = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, b_id),
                reason: RevokeReason::Compromised,
            },
        )
        .unwrap();
    let b_revokes_a = eb
        .apply(
            &mut dbb,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, a_id),
                reason: RevokeReason::Compromised,
            },
        )
        .unwrap();
    let env_ab = env_bytes(&dba, &a_revokes_b.op_id);
    let env_ba = env_bytes(&dbb, &b_revokes_a.op_id);

    // X sees B's op first; Y sees A's first.
    x.apply_remote(&mut dbx, &env_ba).unwrap();
    x.apply_remote(&mut dbx, &env_ab).unwrap();
    y.apply_remote(&mut dby, &env_ab).unwrap();
    y.apply_remote(&mut dby, &env_ba).unwrap();

    for (name, e, db) in [("X", &x, &dbx), ("Y", &y, &dby)] {
        assert!(
            e.is_revoked(db.conn(), &a_id).unwrap(),
            "{name} lost the revocation of A"
        );
        assert!(
            e.is_revoked(db.conn(), &b_id).unwrap(),
            "{name} lost the revocation of B"
        );
    }
    assert_eq!(
        revocation_row(&dbx, &a_id),
        revocation_row(&dby, &a_id),
        "and the two replicas agree about A"
    );
    assert_eq!(
        revocation_row(&dbx, &b_id),
        revocation_row(&dby, &b_id),
        "and about B"
    );
}

/// With an identical `(physical, logical)`, the higher revoker id wins —
/// the same direction `lww_wins` breaks a tie on `device`.
///
/// Nothing pinned this. The register's third key is the only thing standing
/// between two revocations minted in the same logical instant and a pair of
/// replicas that disagree about who revoked and why.
#[test]
fn an_exact_tie_is_broken_by_the_higher_revoker_id() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let one = engine_seeded(ROOT, [2u8; 32], clock());
    let two = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    // Device ids are derivations of a signing key, so which of the two is
    // greater is not something the test gets to choose.
    let (lower, higher) = if one.keychain.device_id() < two.keychain.device_id() {
        (&one, &two)
    } else {
        (&two, &one)
    };

    let at = Hlc {
        physical_ms: T0,
        logical: 3,
    };
    revoke_at(&r1, &mut db1, lower, t, at);
    revoke_at(&r1, &mut db1, higher, t, at);
    revoke_at(&r2, &mut db2, higher, t, at);
    revoke_at(&r2, &mut db2, lower, t, at);

    assert_eq!(revocation_row(&db1, &t), revocation_row(&db2, &t));
    assert_eq!(
        revocation_row(&db1, &t).unwrap().2.as_slice(),
        &higher.keychain.device_id()[..],
        "an exact tie resolves upwards on the revoker id, as `lww_wins` does"
    );
}

/// Two revocations inside one millisecond still converge, because the
/// register compares the HLC's logical half too.
#[test]
fn two_revocations_in_one_millisecond_converge_on_the_logical_half() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let a = engine_seeded(ROOT, [2u8; 32], clock());
    let b = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    let lo = Hlc {
        physical_ms: T0,
        logical: 1,
    };
    let hi = Hlc {
        physical_ms: T0,
        logical: 7,
    };
    revoke_at(&r1, &mut db1, &a, t, hi);
    revoke_at(&r1, &mut db1, &b, t, lo);
    revoke_at(&r2, &mut db2, &b, t, lo);
    revoke_at(&r2, &mut db2, &a, t, hi);

    assert_eq!(revocation_row(&db1, &t), revocation_row(&db2, &t));
    assert_eq!(
        revocation_row(&db1, &t).unwrap().2.as_slice(),
        &a.keychain.device_id()[..],
        "the greater logical half wins, so a physical-only compare is not enough"
    );
}

/// **One sender, one HLC, two targets: two ledger rows and two revocations.**
///
/// The ledger's primary key decides this, and it is the reason
/// `revoked_device_id` is in it. An HLC is monotonic per device only where
/// `MonotonicHlc` stamps it — a *peer's* stamp is whatever that peer wrote,
/// and nothing in the envelope ties it to that sender's `seq`, to its meta
/// epoch, or to any earlier stamp it sent. So two `device_revoke` ops from one
/// member at one `(physical, logical)` naming two different devices are two
/// ops, they both reach `apply_device_revoke`, and they must be two rows.
///
/// Keyed on `(op_hlc_ms, op_hlc_logical, sender)` alone they were one:
/// `INSERT OR IGNORE` kept whichever arrived first, so a replica that received
/// them in the other order folded a different register and the two never
/// reconciled. That is the divergence ADR-0034 corollary 3 forbids peer-side
/// enforcement from reintroducing, asserted here rather than deduced from the
/// schema.
#[test]
fn two_revocations_at_one_hlc_from_one_sender_are_two_ledger_rows() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let sender = engine_seeded(ROOT, [1u8; 32], clock());
    let first = engine_seeded(ROOT, [2u8; 32], clock());
    let second = engine_seeded(ROOT, [3u8; 32], clock());
    let er = engine_seeded(ROOT, [4u8; 32], clock());
    let mut db = db_root(ROOT);
    let (one, two) = (first.keychain.device_id(), second.keychain.device_id());

    let at = Hlc {
        physical_ms: T0,
        logical: 5,
    };
    revoke_at(&er, &mut db, &sender, one, at);
    revoke_at(&er, &mut db, &sender, two, at);

    assert_eq!(
        ledger_rows(&db),
        2,
        "two ops naming two devices must not collapse onto one ledger row"
    );
    assert!(
        er.is_revoked(db.conn(), &one).unwrap(),
        "the first target is revoked"
    );
    assert!(
        er.is_revoked(db.conn(), &two).unwrap(),
        "and so is the second: neither op is the other's duplicate"
    );
}

/// A wall clock that steps **backwards** does not move the cut, because the
/// cut is the op's HLC and `Hlc::send` takes `max(now, local)`.
///
/// This is H2's second half, and it was reachable with no attacker at all:
/// the cut defaulted to the wall clock, so a ten-minute NTP step
/// back produced a revocation whose cut every peer refused as out of range
/// — the user was told the stolen laptop was revoked while no other replica
/// agreed and nothing said so.
#[test]
fn a_backward_clock_step_does_not_move_the_cut() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    // Something happens, so this device's HLC is at T0.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before the step".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // The clock steps back ten minutes — twice `MAX_DRIFT_MS`.
    set_clock(&ca, T0 - 10 * 60 * 1000);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap();

    let cut = revocation_row(&dba, &eb.keychain.device_id()).unwrap().0;
    assert!(
        cut >= i64::try_from(T0).unwrap(),
        "the HLC is monotone, so the cut cannot follow the clock backwards: {cut}"
    );
}

/// A revocation whose cut landed in the past is **superseded** by revoking
/// again from a healthy device, in either arrival order.
///
/// A device that has not yet heard from a peer carries whatever HLC its own
/// clock gives it, so a badly-set one can still emit a backdated cut — the
/// register does not prevent that. What LWW decides is which op is the one
/// on record: under the `MIN` join this replaces, the earliest cut won
/// forever and no later op could correct the record; under LWW the later op
/// wins, which is the whole reason for choosing it.
///
/// What is *not* corrected is the fact of revocation. A recorded row is
/// effective whatever its `cut_ms` says (see [`Engine::is_revoked`]), so
/// revoking again replaces the cut, the author and the reason — it does not
/// un-revoke the device. Nothing in this engine ever did: the cut gated
/// which epochs a revoked device was sealed, never whether its past ops
/// were accepted, and a revoked device's writes are unbounded either way.
#[test]
fn a_backdated_revocation_is_superseded_by_a_healthy_one() {
    const YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let mis_set = engine_seeded(ROOT, [2u8; 32], clock());
    let healthy = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    // The damage: a device a year slow revokes T, and its cut refuses
    // everything T ever wrote.
    revoke(&r1, &mut db1, &mis_set, t, T0 - YEAR_MS);
    assert_eq!(
        revocation_row(&db1, &t).unwrap().0,
        i64::try_from(T0 - YEAR_MS).unwrap(),
        "the premise: the mis-set device's backdated cut is the one on record"
    );

    // The repair, and the same two ops in the other order on a second
    // replica.
    revoke(&r1, &mut db1, &healthy, t, T0);
    revoke(&r2, &mut db2, &healthy, t, T0);
    revoke(&r2, &mut db2, &mis_set, t, T0 - YEAR_MS);

    assert_eq!(
        revocation_row(&db1, &t),
        revocation_row(&db2, &t),
        "order decided nothing"
    );
    assert_eq!(
        revocation_row(&db1, &t).unwrap().0,
        i64::try_from(T0).unwrap(),
        "the healthy cut supersedes the backdated one"
    );
    for (e, db) in [(&r1, &db1), (&r2, &db2)] {
        assert_eq!(
            revocation_row(db, &t).unwrap().2.as_slice(),
            &healthy.keychain.device_id()[..],
            "and both replicas record the healthy device as the revoker"
        );
        assert!(
            e.is_revoked(db.conn(), &t).unwrap(),
            "the record is corrected, not withdrawn: T stays revoked"
        );
    }
}

/// The local command writes the same register the remote path does,
/// because it now *is* the remote path.
///
/// There were two writers to these columns and only one took the join:
/// `apply_control_op` resolved concurrent revocations, while
/// `revoke_device` did a bare unconditional `UPDATE` that set neither the
/// HLC's logical half nor any guard. A device revoking one already revoked
/// therefore moved its own cut while every peer kept the other, and it was
/// then the only replica accepting a window of ops.
#[test]
fn a_local_revocation_writes_the_same_register_as_a_remote_one() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let target = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, target),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    // The register holds a full HLC and its sender, not half of one.
    let row = revocation_row(&dba, &target).expect("the local command records it");
    assert_eq!(row.0, i64::try_from(T0).unwrap());
    assert_eq!(
        row.2.as_slice(),
        &ea.keychain.device_id()[..],
        "the bare UPDATE this replaces wrote no HLC logical half and no \
             guard, so the register compared against a default here and a real \
             value on every other replica"
    );

    // And it participates in the register: a staler revocation arriving
    // afterwards does not overwrite it.
    let before = revocation_row(&dba, &target);
    revoke(&ea, &mut dba, &ec, target, T0 - 5_000);
    assert_eq!(
        revocation_row(&dba, &target),
        before,
        "a local write outside the register would have been overwritten here"
    );
}

/// A `device_revoke` naming a device whose cert has not arrived yet is
/// durable, and the cert does not resurrect it.
///
/// Ordinary, not exotic: a cert parked in `deferred_ops` at meta epoch *k*
/// drains after a revocation absorbed at *k+1*. A bare
/// `UPDATE ... WHERE device_id = ?` matched no row and dropped the
/// revocation silently; `DeviceCertPublish` then created the row
/// **unrevoked**. Fail-open, and decided by delivery order.
#[test]
fn a_revocation_that_arrives_before_the_cert_survives_it() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();

    // B has never seen A's cert when the revocation lands.
    revoke(&eb, &mut dbb, &eb, a_id, T0);
    assert!(
        eb.is_revoked(dbb.conn(), &a_id).unwrap(),
        "the revocation must be durable without a devices row to update"
    );
    // And it does not invent one. Upserting into `devices` made every
    // revocation of an unknown id a phantom entry in the user's device
    // list, and a phantom row also satisfies `revoke_device`'s
    // known-device guard.
    assert_eq!(
        device_rows(&dbb),
        0,
        "a revocation must not mint a device nobody has ever seen"
    );

    // A's cert arrives afterwards. It fills the identity columns in and
    // leaves the revocation alone.
    trust(&eb, &mut dbb, &ea);
    assert!(
        eb.is_revoked(dbb.conn(), &a_id).unwrap(),
        "the cert must not resurrect a revoked device"
    );
    assert_eq!(device_rows(&dbb), 1, "and it is one device, not two");
}

/// Revoking a device this vault has never seen is refused, and stays
/// refused however many revocations of unknown ids have gone past.
///
/// The guard reads `devices`, so while a `device_revoke` upserted into that
/// table a revocation of any id at all made its target "known" — and
/// `revoke_device` mints a fresh epoch for every stream in the account
/// before it gets anywhere near checking anything else.
#[test]
fn revoking_a_device_the_vault_has_never_seen_is_refused() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let ghost = [0x9e; 16];
    revoke(&ea, &mut dba, &eb, ghost, T0);
    assert_eq!(device_rows(&dba), 1, "the ghost is not in the device list");

    let err = ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, ghost),
            reason: RevokeReason::Lost,
        },
    );
    assert!(
        matches!(err, Err(EngineError::NotFound(_))),
        "revoking a ghost must not reach the rotation, got {err:?}"
    );
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

proptest::proptest! {
    /// `docs/05-sync/conflict-resolution.md` specifies this comparator as a
    /// deterministic total order over `(hlc, device, seq)` — "memcmp,
    /// higher wins … so every replica picks the same winner". Five
    /// hand-picked examples enforced particular outcomes; nothing asserted
    /// it was an order at all, and each of the three laws fails in its own
    /// way:
    ///
    /// * not antisymmetric — both replicas decide their own write won, and
    ///   the two rows diverge permanently with no error anywhere;
    /// * not transitive — the surviving state depends on the order three
    ///   ops happened to arrive in, which is exactly what LWW exists to
    ///   remove;
    /// * not total — neither op wins, so whichever row each replica
    ///   already had stays.
    ///
    /// Antisymmetry is stated over *distinct* stamps only. Equal stamps
    /// deliberately win in both directions (`incoming.seq >= row.seq`):
    /// an equal stamp from the same device is the same op re-delivered, and
    /// letting the replay rewrite the row unchanged is what keeps
    /// re-delivery a harmless no-op. Totality and transitivity are stated
    /// over everything, equal stamps included.
    #[test]
    fn lww_is_a_total_order_and_antisymmetric_on_distinct_stamps(
        a in arb_stamp(),
        b in arb_stamp(),
        c in arb_stamp(),
    ) {
        let wins = |x: &LwwStamp, y: &LwwStamp| lww_wins(x, &row_of(y));

        proptest::prop_assert!(
            wins(&a, &b) || wins(&b, &a),
            "neither {a:?} nor {b:?} wins, so each replica keeps the row it \
                 already had and the two never converge"
        );

        if a != b {
            proptest::prop_assert_ne!(
                wins(&a, &b),
                wins(&b, &a),
                "both orderings agree for distinct stamps {:?} / {:?}, so the \
                     winner depends on which replica is asking",
                a,
                b
            );
        }

        if wins(&a, &b) && wins(&b, &c) {
            proptest::prop_assert!(
                wins(&a, &c),
                "{a:?} beats {b:?} beats {c:?} but loses to {c:?}, so the \
                     survivor depends on the order the three ops arrived in"
            );
        }
    }

    /// `remote_op_id` had no direct test of any kind, and both of its
    /// invariants are load-bearing:
    ///
    /// * **determinism** is what makes a re-delivered op compute the op-log
    ///   primary key it already has, so the `UNIQUE(stream, device, seq)`
    ///   gate sees it as the same row and the replay is a no-op;
    /// * **injectivity** is the other half — two different ops that derived
    ///   the same id would alias onto one op-log row, and whichever arrived
    ///   second would be silently swallowed as a duplicate.
    ///
    /// Both hold because the three fields are fixed-width (16 + 16 + 8), so
    /// the derivation's message determines the triple; a length-prefixed or
    /// variable-width encoding is where this stops being true.
    #[test]
    fn remote_op_id_is_deterministic_and_injective(
        (sa, da, qa) in arb_op_id_triple(),
        (sb, db_id, qb) in arb_op_id_triple(),
    ) {
        proptest::prop_assert_eq!(
            remote_op_id(&sa, &da, qa),
            remote_op_id(&sa, &da, qa),
            "the same triple derived two different ids, so a re-delivered op \
                 would be recorded twice"
        );

        let same_triple = (sa, da, qa) == (sb, db_id, qb);
        proptest::prop_assert_eq!(
            remote_op_id(&sa, &da, qa) == remote_op_id(&sb, &db_id, qb),
            same_triple,
            "({:?}, {:?}, {}) and ({:?}, {:?}, {}) must share an op id exactly \
                 when they are the same op",
            sa,
            da,
            qa,
            sb,
            db_id,
            qb
        );
    }
}

proptest::proptest! {
    // Each case builds two engines and two databases and delivers up to 18
    // envelopes, so the case count is deliberately low.
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..proptest::prelude::ProptestConfig::with_cases(24)
    })]

    /// `docs/05-sync/wire-protocol.md:447-453` calls the contiguous-prefix
    /// cursor "a correctness invariant": a `MAX(seq)` cursor "converts an
    /// accidental, self-healing data-loss window into a permanent and
    /// silent one", because the relay filters its replay by exactly this
    /// number and would skip the frame carrying the hole forever.
    ///
    /// One three-op example enforced it. This delivers an arbitrary noisy
    /// run — duplicates, repeats, holes at any position — followed by a
    /// shuffled permutation of every op, and asserts after **every single
    /// delivery** that the cursor equals the length of the contiguous
    /// prefix actually applied, never the maximum seq seen.
    #[test]
    fn a_cursor_equals_the_contiguous_prefix_after_every_delivery(
        schedule in delivery_schedule(),
    ) {
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let mut envs = Vec::with_capacity(DELIVERY_OPS);
        for n in 0..DELIVERY_OPS {
            let res = ea
                .apply(
                    &mut dba,
                    Command::CreateTask(TaskDraft {
                        title: format!("op {n}"),
                        ..Default::default()
                    }),
                )
                .unwrap();
            envs.push(env_bytes(&dba, &res.op_id));
        }
        let heads: Vec<_> = envs
            .iter()
            .map(|e| sunrise_cbor::decode_envelope_header(e).unwrap())
            .collect();
        let (stream, device) = (heads[0].stream_id, heads[0].device_id);
        proptest::prop_assert_eq!(
            heads.iter().map(|h| h.seq).collect::<Vec<_>>(),
            (1..=DELIVERY_OPS as u64).collect::<Vec<_>>(),
            "the fixture assumes seqs 1..=N on one stream from one device"
        );

        let mut applied = BTreeSet::new();
        for idx in schedule {
            eb.apply_remote(&mut dbb, &envs[idx]).unwrap();
            applied.insert(heads[idx].seq);

            let mut prefix = 0u64;
            while applied.contains(&(prefix + 1)) {
                prefix += 1;
            }
            proptest::prop_assert_eq!(
                cursor_for(&dbb, &stream, &device),
                prefix,
                "after applying {:?} the cursor must claim {} (a max-based \
                     cursor would claim {})",
                applied,
                prefix,
                applied.iter().copied().max().unwrap_or(0)
            );
        }

        proptest::prop_assert_eq!(
            cursor_for(&dbb, &stream, &device),
            DELIVERY_OPS as u64,
            "every op arrived, so the prefix covers all of them"
        );
    }
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
    let decoded: sunrise_domain::Task =
        sunrise_cbor::decode_lenient(&bytes).expect("an unknown variant must not reject the op");
    assert_eq!(decoded.state, TaskState::Todo);
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
    // And the cursor did not move, which is the half `upsert_sync_cursor`'s
    // doc asserts in prose — "a refused op is *not* decided and does not
    // appear here" — and nothing else in this suite held. This refusal is
    // the pre-insert kind: the drift gate runs before the op-log insert, so
    // `upsert_sync_cursor` is never reached at all. Counting rows for A
    // rather than reading one stream's cursor keeps the assertion true
    // whatever stream the command routed to.
    let cursors_for_a: i64 = dbb
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sync_cursors WHERE device_id = ?",
            params![&ea.keychain.device_id()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursors_for_a, 0,
        "a refused op left this replica claiming to have applied one"
    );
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
            ensure_stream_row(tx, &r.template.stream_id, NOW as u64)?;
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

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..proptest::prelude::ProptestConfig::with_cases(32)
    })]

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
                    scheduled_at: Some(Some(ms_to_ts((REVIEW_MON + 25 * 3_600_000) as i64).into())),
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

// ---------------------------------------------------------------------------
// The read bound is not the register (migration 0028, ADR-0041 §"What a user
// sees" item 5).
// ---------------------------------------------------------------------------

/// **An unwound device is not a key recipient again.**
///
/// The chain from
/// `a_revocation_written_before_the_senders_own_cut_is_unwound_when_the_sender_is_revoked`,
/// asked the question that test does not. That one asserts C leaves the
/// *register*, and the register was also what `emit_key_envelopes`
/// anti-joined — so the unwind released the read bound and C re-entered the
/// recipient set of every epoch the vault minted from then on. No adversary is
/// involved: the sequence is retiring an old laptop from the desktop and,
/// months later, the desktop from the phone, which is what migration 0027's
/// own seed traces.
///
/// The fix is that the four key-distribution sites read `device_read_bounds`,
/// which the fold only ever adds to. Point the anti-join back at
/// `device_revocations` and this goes red.
#[test]
fn an_unwound_revocation_does_not_make_the_device_a_key_recipient_again() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, b_id, c_id) = (
        ea.keychain.device_id(),
        eb.keychain.device_id(),
        ec.keychain.device_id(),
    );
    // Everyone is in the device list, so everyone is a candidate recipient.
    trust(&ea, &mut db, &eb);
    trust(&ea, &mut db, &ec);

    // A retires C.
    revoke(&ea, &mut db, &ea, c_id, T0);
    assert!(revocation_row(&db, &c_id).is_some());
    assert!(
        read_bound_row(&db, &c_id).is_some(),
        "the bound is taken by the same fold that writes the register"
    );

    // Later, B retires A, which unwinds A's revocation of C.
    revoke(&ea, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "the register is a fold, so C is called current again"
    );
    assert!(
        read_bound_row(&db, &c_id).is_some(),
        "but the read bound is a ratchet and C is still bounded"
    );

    // The consequence the register alone could not hold: mint a fresh epoch
    // now and see who it is sealed to.
    let before = envelopes_to(&ea, &db, &c_id).len();
    let stream = [0x3c; 16];
    db.with_tx(|tx| {
        let (epoch, key) = ea
            .keychain
            .mint_epoch(tx, &stream, ea.rng.as_ref(), T0 + 120_000)?;
        ea.emit_key_envelopes(tx, &stream, epoch, &key, T0 + 120_000, None)
    })
    .unwrap();

    assert_eq!(
        envelopes_to(&ea, &db, &c_id).len(),
        before,
        "an unwound device must not be sealed an epoch minted after its bound"
    );
    assert!(
        !envelopes_to(&ea, &db, &b_id).is_empty(),
        "and an unrevoked sibling must still be sealed one, or this asserts nothing"
    );
}

/// The republish route is closed on the bound too.
///
/// `backfill_key_envelopes` is the larger of the two failures, because it does
/// not hand back the epochs minted from now on — it hands back **every epoch
/// this vault holds**. Against the register, one `DeviceCertPublish` from an
/// unwound device pulled all of them.
#[test]
fn an_unwound_device_republishing_its_cert_is_handed_nothing() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    trust(&ea, &mut db, &eb);
    trust(&ea, &mut db, &ec);

    revoke(&ea, &mut db, &ea, c_id, T0);
    revoke(&ea, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(revocation_row(&db, &c_id), None, "C reads current again");

    // An epoch minted *after* the bound, on a stream C has never been served.
    // `key_envelope_recipients` therefore holds no row saying C has it, which
    // is the whole of what makes a backfill emit anything at all — without
    // this the call is a no-op for reasons that have nothing to do with
    // revocation, and the test would pass against either table.
    let stream = [0x7b; 16];
    db.with_tx(|tx| {
        ea.keychain
            .mint_epoch(tx, &stream, ea.rng.as_ref(), T0 + 90_000)
            .map(|_| ())
    })
    .unwrap();
    let served_before: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM key_envelope_recipients
             WHERE stream_id = ? AND recipient = ?",
            params![&stream[..], &c_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(served_before, 0, "nothing has served C this stream yet");

    let before = envelopes_to(&ea, &db, &c_id).len();
    let c_pub = ec.keychain.device_dh_pub();
    db.with_tx(|tx| ea.backfill_key_envelopes(tx, &c_id, &c_pub, T0 + 120_000))
        .unwrap();

    assert_eq!(
        envelopes_to(&ea, &db, &c_id).len(),
        before,
        "the backfill must read the bound, not the register: against the register a \
         republished cert hands an unwound device every held epoch of every stream"
    );
}

/// The starve attack, which is the shape with no honest cause.
///
/// A revoked device X emits one ordinary `device_revoke` naming each current
/// device. Every one is correctly gated, so the register records nothing — and
/// the bound must record nothing either, or X takes the whole account's key
/// access away with N ops and no crafted input at all.
///
/// The bound is written from `register` and not from the ledger, which is what
/// makes this hold: a gated row never reaches `register`, so it never reaches
/// `device_read_bounds`. Seed the bound from `device_revoke_ops` instead and
/// this goes red.
#[test]
fn a_revoked_device_emitting_gated_revocations_bounds_nobody() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, d_id, t_id) = (
        ex.keychain.device_id(),
        ed.keychain.device_id(),
        et.keychain.device_id(),
    );

    // O expels X. X is now gated for every third party.
    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(read_bound_row(&db, &x_id).is_some(), "X is bounded");

    // X names every remaining device, one ordinary op each.
    revoke(&er, &mut db, &ex, d_id, T0 + 1_000);
    revoke(&er, &mut db, &ex, t_id, T0 + 2_000);

    for (name, id) in [("D", &d_id), ("T", &t_id)] {
        assert_eq!(
            revocation_row(&db, id),
            None,
            "{name}'s revocation by a revoked device must be gated"
        );
        assert_eq!(
            read_bound_row(&db, id),
            None,
            "and a gated revocation must not bound {name} either -- otherwise a revoked \
             device takes the account's key access with one op per device"
        );
    }
}

/// The discount rehabilitates the *gate* and never the bound.
///
/// ADR-0041 §Decision 1's discount pass takes a revoker back out of a device's
/// revoker set once somebody else has expelled that revoker, so X stops being
/// gated and revokes third parties again. What it must not do is give X its
/// keys back: the bound was taken when the account believed X was out, and
/// nothing in the tree releases one. This pins the asymmetry the device list's
/// `read_bounded` column exists to show.
#[test]
fn the_discount_rehabilitates_the_gate_and_never_the_read_bound() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id, d_id) = (
        ex.keychain.device_id(),
        eo.keychain.device_id(),
        ed.keychain.device_id(),
    );

    // O revokes X; a third party P then revokes O.
    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(read_bound_row(&db, &x_id).is_some());
    revoke(&er, &mut db, &ep, o_id, T0 + 10_000);

    assert_eq!(
        revocation_row(&db, &x_id),
        None,
        "O's revocation of X is unwound, because O was itself revoked"
    );
    assert!(
        read_bound_row(&db, &x_id).is_some(),
        "and X keeps its read bound, which is the whole of migration 0028"
    );

    // X is no longer gated -- the discount took O out of X's revoker set --
    // so this op lands. That is the recorded residual, not a defect.
    revoke(&er, &mut db, &ex, d_id, T0 + 20_000);
    assert!(
        revocation_row(&db, &d_id).is_some(),
        "the discount ungates X, which is ADR-0041 §\"What a user sees\" item 4's residual"
    );
    assert!(
        read_bound_row(&db, &x_id).is_some(),
        "X revoking somebody else does not give X its own keys back"
    );
}

// ---------------------------------------------------------------------------
// A gated revocation does nothing at all (ADR-0041 §Consequences).
// ---------------------------------------------------------------------------

/// **A revoked device's gated revocation mints no key.**
///
/// The party doing the minting is the revoked one. `revoke_device` ran
/// `rotation_set` → `mint_epoch` → `emit_key_envelopes` whether or not the
/// fold believed its op, so an expelled device running
/// `sunrise devices revoke <anything>` drew a fresh key for every stream in
/// the account, wrote it into its **own** `stream_keys`, and sealed it to every
/// honest peer — whose next writes it could then read, because
/// `Keychain::absorb_stream_key` checks no sender standing and
/// `current_epoch_tx` is `MAX(epoch)`. It was told, correctly, that it had
/// revoked nothing.
///
/// Move the rotation loop back outside the `if effective` block in
/// `revoke_device` and this goes red.
#[test]
fn a_gated_revocation_mints_no_epoch_for_the_revoked_device_that_asked() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // X's own vault: X is the device running the command.
    let mut dbx = db_root(ROOT);
    let (x_id, d_id) = (ex.keychain.device_id(), ed.keychain.device_id());
    trust(&ex, &mut dbx, &eo);
    trust(&ex, &mut dbx, &ed);

    // O expels X, and X's own replica applies it.
    revoke(&ex, &mut dbx, &eo, x_id, T0);
    assert!(ex.is_revoked(dbx.conn(), &x_id).unwrap());

    let before = held_epochs(&dbx);
    let out = ex
        .apply(
            &mut dbx,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, d_id),
                reason: RevokeReason::Stolen,
            },
        )
        .unwrap();

    assert!(
        out.revocation_gated,
        "the fold must discard a revoked device's revocation of a third party"
    );
    assert_eq!(
        revocation_row(&dbx, &d_id),
        None,
        "and D stays current, which is what makes the rotation pointless"
    );
    assert_eq!(
        held_epochs(&dbx),
        before,
        "a gated revocation must mint nothing: every key it drew would land in the \
         revoked device's own stream_keys and be sealed to every honest peer"
    );
    assert!(
        out.unrotated_streams.is_empty(),
        "nothing was left unrotated, because nothing was rotated"
    );
    assert!(
        pending_relay_revocations(&dbx).is_empty(),
        "and the relay is told nothing, which was already true"
    );
}

/// The other side of the same predicate: `effective` is **true** when the
/// target was already revoked by a surviving row, and the rotation still runs.
///
/// This is the case the guard must not swallow. The command's own op is gated
/// — its sender is revoked — while the account does record a revocation of the
/// target, so the command is not a no-op and re-revoking is a legitimate,
/// harmless act. Guarding on "was this op gated" rather than on `effective`
/// would break it.
#[test]
fn a_gated_op_whose_target_is_already_revoked_still_rotates() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbx = db_root(ROOT);
    let (x_id, d_id) = (ex.keychain.device_id(), ed.keychain.device_id());
    trust(&ex, &mut dbx, &eo);
    trust(&ex, &mut dbx, &ed);

    // O expels D first, and then expels X.
    revoke(&ex, &mut dbx, &eo, d_id, T0);
    revoke(&ex, &mut dbx, &eo, x_id, T0 + 1_000);
    assert!(
        revocation_row(&dbx, &d_id).is_some(),
        "D is out on O's word"
    );

    let before = held_epochs(&dbx);
    let out = ex
        .apply(
            &mut dbx,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, d_id),
                reason: RevokeReason::Stolen,
            },
        )
        .unwrap();

    assert!(
        !out.revocation_gated,
        "`effective` reads the register after the fold, and a surviving revocation of \
         the target makes this command's claim true whoever wrote it"
    );
    assert_ne!(
        held_epochs(&dbx),
        before,
        "so the rotation still runs: the predicate is the account's belief about the \
         target, not this op's own fate"
    );
}

/// The unwind reaches the caller, and not only an operator's NDJSON.
///
/// `docs/10-cross-cutting/log-events.md` states the rule on
/// `core.device.revoke_incomplete`: a fact that changes what the account
/// believes comes back on `CommandResult` and a client must disclose it. The
/// unwind has a strictly larger consequence than an unrotated stream and was
/// reaching a `tracing::warn!` and nothing else.
#[test]
fn a_revocation_reports_the_devices_the_account_stopped_calling_revoked() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id, d_id) = (
        ea.keychain.device_id(),
        ec.keychain.device_id(),
        ed.keychain.device_id(),
    );
    trust(&eb, &mut db, &ea);
    trust(&eb, &mut db, &ec);
    trust(&eb, &mut db, &ed);

    // A retires C; then B retires A, unwinding A's op. C is now bounded and
    // not revoked -- invisible to any caller reading the register alone.
    revoke(&eb, &mut db, &ea, c_id, T0);
    revoke(&eb, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(revocation_row(&db, &c_id), None);

    // B's next revocation is where a user finds out.
    let out = eb
        .apply(
            &mut db,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, d_id),
                reason: RevokeReason::Retired,
            },
        )
        .unwrap();

    assert!(
        !out.revocation_gated,
        "B is current and its revocation lands"
    );
    assert!(
        out.revocation_unwound.iter().any(|h| {
            let mut want = String::new();
            for b in &c_id {
                use std::fmt::Write as _;
                let _ = write!(want, "{b:02x}");
            }
            *h == want
        }),
        "the device the account stopped calling revoked must reach the caller; got {:?}",
        out.revocation_unwound
    );
    assert!(
        !out.revocation_unwound
            .iter()
            .any(|h| h.contains(&format!("{:02x}{:02x}", d_id[0], d_id[1]))
                && read_bound_row(&db, &d_id).is_some()
                && revocation_row(&db, &d_id).is_some()),
        "a device that is both bounded and revoked is not unwound and must not be listed"
    );
}

/// The device list carries both facts, because they can disagree.
///
/// `revoked` is the derived register and `read_bounded` is the ratchet. A row
/// reading `revoked: false, read_bounded: true` is a device the account calls
/// current while giving it nothing, and before this column a user had no way
/// to see it at all.
#[test]
fn the_device_list_shows_a_device_that_is_bounded_without_being_revoked() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    trust(&eb, &mut db, &ea);
    trust(&eb, &mut db, &ec);

    revoke(&eb, &mut db, &ea, c_id, T0);
    revoke(&eb, &mut db, &eb, a_id, T0 + 60_000);

    let QueryResult::Devices(rows) = eb.query(&db, Query::DeviceList).unwrap() else {
        panic!("expected a device list");
    };
    let c = rows
        .iter()
        .find(|r| r.device_id == c_id)
        .expect("C is in the list");
    assert!(
        !c.revoked,
        "the register no longer names C, which is the fold working as designed"
    );
    assert!(
        c.read_bounded,
        "and the list must say it receives nothing, or the state is invisible"
    );

    let a = rows
        .iter()
        .find(|r| r.device_id == a_id)
        .expect("A is in the list");
    assert!(a.revoked && a.read_bounded, "A is out on both counts");
}

/// The fourth key-distribution site: the roster `rotate_identity` builds.
///
/// This one hands out HPKE shares of the **successor `ID_S_priv`**, which is a
/// strictly larger grant than a stream key — a device on the roster follows the
/// account's identity chain. Against the derived register, an unwound device
/// was back on the roster of every transition from then on, so a revocation
/// that had already taken effect was undone by a later, unrelated one.
///
/// Point the survivor query back at `device_revocations` and this goes red.
#[test]
fn an_unwound_device_is_not_on_the_next_identity_rotations_roster() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // B's vault: B is the device that will rotate.
    let mut dbb = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    trust(&eb, &mut dbb, &ea);
    trust(&eb, &mut dbb, &ec);

    let baseline = eb.rotate_identity(&mut dbb, None, true).unwrap();
    assert_eq!(
        baseline.devices_kept, 3,
        "B, A and C are all on the roster before anything is revoked"
    );

    // A retires C, then B retires A -- which unwinds A's revocation of C.
    revoke(&eb, &mut dbb, &ea, c_id, T0);
    revoke(&eb, &mut dbb, &eb, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&dbb, &c_id),
        None,
        "the fold no longer calls C revoked"
    );
    assert!(
        read_bound_row(&dbb, &c_id).is_some(),
        "and C is still bounded"
    );

    let after = eb.rotate_identity(&mut dbb, None, true).unwrap();
    assert_eq!(
        after.devices_kept, 1,
        "only B survives: A is revoked and C is read-bounded. A roster built from the \
         register would carry C, and C would hold a share of the successor ID_S_priv"
    );
}

/// Read `devices.(cert_blob, d_d_pub, identity_id)` for one device.
fn device_key_row(db: &Db, id: &[u8; 16]) -> (Vec<u8>, Option<Vec<u8>>, Vec<u8>) {
    db.conn()
        .query_row(
            "SELECT cert_blob, d_d_pub, identity_id FROM devices WHERE device_id = ?",
            params![&id[..]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// **#281.** A device's own republish cannot rebind its `d_d_pub`.
///
/// `a_device_cannot_publish_a_cert_naming_another_device` stops a *sibling*
/// redirecting C's envelopes; it does not stop C's own id doing it, because
/// `device_id` is derived from `D_S_pub` alone. A holder of `ID_S_priv` and
/// C's `D_S_priv` can mint a cert for C's id under a fresh `D_D_pub`, and
/// before this the upsert took it and the backfill on the next line sealed
/// every held epoch to the new key.
#[test]
fn a_republished_cert_cannot_rebind_the_device_key() {
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ec);
    let c_id = ec.keychain.device_id();
    let before = device_key_row(&dbb, &c_id);
    let ops_before = op_count(&dbb);

    let rebound = ec
        .keychain
        .issue_cert_for(
            c_id,
            ec.keychain.device_signing_pub(),
            [0x42; 32],
            "device",
            "macos",
            T0,
        )
        .expect("issue the rebinding cert");
    apply_control_at(
        &eb,
        &mut dbb,
        &InnerOp::DeviceCertPublish(rebound),
        &c_id,
        Hlc::at(T0),
        1,
    );

    assert_eq!(
        device_key_row(&dbb, &c_id),
        before,
        "the whole cert is refused, so cert_blob and d_d_pub still agree"
    );
    assert_eq!(
        before.1.as_deref(),
        Some(&ec.keychain.device_dh_pub()[..]),
        "and C's envelopes are still sealed to C's own key"
    );
    assert_eq!(op_count(&dbb), ops_before, "and nothing was backfilled");

    // The honest republish, same key, still lands.
    trust_at(&eb, &mut dbb, &ec, T0 + 1);
    assert_eq!(device_key_row(&dbb, &c_id).1, before.1);
}

/// A NULL `d_d_pub` is filled rather than refused: that is the legacy-adoption
/// row, and filling it is what `DeviceCertPublish` is for.
#[test]
fn a_cert_fills_a_null_device_key() {
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ec);
    let c_id = ec.keychain.device_id();
    dbb.conn()
        .execute(
            "UPDATE devices SET d_d_pub = NULL WHERE device_id = ?",
            params![&c_id[..]],
        )
        .unwrap();

    trust_at(&eb, &mut dbb, &ec, T0 + 1);
    assert_eq!(
        device_key_row(&dbb, &c_id).1.as_deref(),
        Some(&ec.keychain.device_dh_pub()[..])
    );
}

/// The roster is the other upsert of `d_d_pub`, and it holds the same rule: an
/// entry carrying a different key for a device this vault holds is skipped,
/// which leaves the device on its old row exactly as an omission would.
#[test]
fn a_roster_entry_cannot_rebind_the_device_key() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();
    let before = device_key_row(&dba, &b_id);

    let successor = ea.keychain.mint_successor_identity(&SystemRng);
    let entry = |d_d_pub: [u8; 32]| RosterEntry {
        cert: Keychain::issue_roster_cert(
            &successor,
            b_id,
            eb.keychain.device_signing_pub(),
            d_d_pub,
            "device",
            "test",
            T0,
        )
        .expect("roster cert"),
    };
    let to = successor.identity_id();

    dba.with_tx(|tx| ea.apply_roster(tx, &[entry([0x42; 32])], &to))
        .unwrap();
    assert_eq!(
        device_key_row(&dba, &b_id),
        before,
        "a rebinding entry moves nothing"
    );

    dba.with_tx(|tx| ea.apply_roster(tx, &[entry(eb.keychain.device_dh_pub())], &to))
        .unwrap();
    let after = device_key_row(&dba, &b_id);
    assert_eq!(after.1, before.1);
    assert_eq!(
        after.2,
        to.to_vec(),
        "an honest entry still moves the device"
    );
}
