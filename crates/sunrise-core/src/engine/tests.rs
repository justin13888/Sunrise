//! The engine's unit tests.
//!
//! Kept as one module across the `engine/` split. The suite is grouped by
//! theme in reading order — tasks, contexts, streams, routines, blocks,
//! attachments, focus, sync/LWW, revocation, reviews — and every test in it
//! draws on the `testutil` helpers below, so moving it whole is what keeps the
//! test diff at zero. Redistributing it module by module is a later change.

use super::ids::*;
use super::lww::*;
use super::oplog::*;
use super::*;
use crate::commands::{Command, CommandResult, FocusStartDraft};
use crate::config::Clock;
use crate::config::SystemRng;
use crate::control_op::{DeviceRevokePayload, KeyEnvelopePayload, Recipient, RevokeReason};
use crate::events::DomainEvent;
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp};
use crate::keychain::{to16, KeySource, Keychain};
use crate::queries::{ActionableTask, BlockRow, ContextRow, FocusSessionRow, Query, QueryResult};
use parking_lot::Mutex as PLMutex;
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;
use std::sync::Arc;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_crypto::{stream_key_id, StreamKey};
use sunrise_domain::sort_order;
use sunrise_domain::time::SunriseTime;
use sunrise_domain::ActivityKind;
use sunrise_domain::EffectiveTaskState;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    imported_block_id, inbox_stream_ref, occurrence_key_at, occurrence_task_id, ActivityEvent,
    Attachment, AttachmentDraft, BlockDraft, BlockPatch, Chunk, ContextDraft, ContextPatch, Energy,
    ExportDataset, ExportFormat, FocusEnd, FocusKind, FocusStart, InterruptionReason, NoteBody,
    ReminderSettings, RoutinePatch, ScheduleConstraint, SessionLength, StreamColor, StreamDraft,
    StreamPatch, StreamReviewCadence, Task, TaskDraft, TaskPatch, TaskState, Trends,
    ValidationError, WeeklyReview, INBOX_STREAM_BYTES, POMODORO_MS,
};
use sunrise_domain::{EndOfDayPlan, MorningSummary};
use sunrise_domain::{RRule, Routine, RoutineCatchupPolicy, RoutineDraft, TaskTemplate};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;
use sunrise_storage::{OpLog, Outbox};
use testutil::*;

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
                .apply_control_op(tx, &inner, &sender_id, hlc, hlc.physical_ms)
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
        let cert = sender.keychain.cert_blob().to_vec();
        let sender_id = sender.keychain.device_id();
        db.with_tx(|tx| {
            receiver
                .apply_control_op(
                    tx,
                    &InnerOp::DeviceCertPublish(cert),
                    &sender_id,
                    Hlc::at(0),
                    0,
                )
                .map(|_| ())
        })
        .unwrap();
    }

    /// [`trust`] at a chosen wall clock, for tests that care when the cert
    /// landed relative to a revocation cut.
    pub(super) fn trust_at(receiver: &Engine, db: &mut Db, sender: &Engine, now_ms: u64) {
        let cert = sender.keychain.cert_blob().to_vec();
        let sender_id = sender.keychain.device_id();
        db.with_tx(|tx| {
            receiver
                .apply_control_op(
                    tx,
                    &InnerOp::DeviceCertPublish(cert),
                    &sender_id,
                    Hlc::at(now_ms),
                    now_ms,
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
    let cert = ea.keychain.issue_cert_for(
        c_id,
        ec.keychain.device_signing_pub(),
        ea.keychain.device_dh_pub(),
        T0,
    );
    dbb.with_tx(|tx| {
        eb.apply_control_op(
            tx,
            &InnerOp::DeviceCertPublish(cert),
            &a_id,
            Hlc::at(T0),
            T0,
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
    let own = ec.keychain.cert_blob().to_vec();
    dbb.with_tx(|tx| {
        eb.apply_control_op(
            tx,
            &InnerOp::DeviceCertPublish(own.clone()),
            &c_id,
            Hlc::at(T0),
            T0,
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
        dbb.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0))
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
        db.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0))
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

/// A revoked device's ops still **apply** at a receiving replica, and its
/// cursor still advances.
///
/// This half is deliberately not a gate, and there is no write bound
/// elsewhere for it to defer to. Nothing bounds a revoked device's writes
/// today: the relay would have to be told out of band and cannot be,
/// because `DELETE /api/v1/devices/{device_id}` names the **relay's** id
/// for a device — a ULID it minted at registration — while a vault knows
/// only its own 16-byte device id and no peer's relay id
/// ([#80](https://github.com/justin13888/Sunrise/issues/80)).
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
/// #78; #82 is where a convergent form belongs, after the relay bound.
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

/// **A revoked device rejoins under a fresh device id, and every replica
/// lets it.** This test asserts the bypass, not a defence against it.
///
/// [#105](https://github.com/justin13888/Sunrise/issues/105). Revocation
/// names a *device id*; the capability it needs to take away is
/// `ID_S_priv`, which every paired device holds and no revocation touches.
/// So the revoked device mints a fresh keypair, signs a `DeviceCert` for it
/// under the account identity, publishes it — sealed under the pre-rotation
/// vault-meta epoch it still has — and `backfill_key_envelopes` hands the
/// new id every key the revocation had just rotated away.
///
/// Both engines below are built from the same vault root, which is what
/// gives them the same account identity, and that models the situation
/// exactly: the revoked device *is* holding the key that signs certs. The
/// new id is not a forgery. It verifies.
///
/// It is pinned here because it was a claim in three documents and an
/// assertion in none, and because it is the test that has to flip when the
/// fix lands. ADR-0032 records why the fix is identity rotation and not any
/// of the narrower checks that were considered here.
#[test]
fn a_revoked_device_rejoins_under_a_fresh_device_id() {
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

    let mut got = envelopes_to(&ea, &dba, &c2_id);
    got.sort_unstable();
    let mut want = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    want.sort_unstable();
    assert!(!want.is_empty(), "the revocation rotated something");
    assert_eq!(
        got, want,
        "every key the revocation rotated away is handed back to the same \
             device under a name the register does not list"
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
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(24))]

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
