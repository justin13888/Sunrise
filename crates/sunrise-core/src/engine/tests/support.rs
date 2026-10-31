//! Shared test support for every test in the engine's test module.
//!
//! A sibling module cannot reach another sibling's private items, so a helper
//! more than one test module draws on lives here. Everything is `pub(super)`,
//! which keeps it reachable from `tests` and from any module nested inside it.

use super::*;

#[derive(Debug)]
pub(super) struct FakeClock(pub(super) PLMutex<u64>);

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock()
    }
}

/// The randomness a test engine is built with, paired with [`FakeClock`].
///
/// A frozen clock makes every ULID an engine mints share its timestamp,
/// so their order is the order of their random tails. With `SystemRng`
/// that order changed from run to run, and any expectation resting on
/// the final id tie-break (search order, the Late group's tie) would
/// have been nondeterministic. Each engine a test builds draws from its
/// own seeded stream instead: the seed is the engine's position among
/// the engines its thread has built, so a test replays exactly and two
/// engines of one test never mint the same id.
///
/// A proptest runs every case and shrink step on one thread, so a
/// property that builds engines calls [`begin_case`] first: its engines
/// then mint the same ids whether the case runs fresh or is replayed
/// from `proptest-regressions`.
#[derive(Debug)]
pub(super) struct SeededRng(PLMutex<rand::rngs::StdRng>);

impl Rng for SeededRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        rand_core::RngCore::fill_bytes(&mut *self.0.lock(), dest);
    }
}

thread_local! {
    static ENGINES_BUILT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The next seeded [`Rng`] of this thread; see [`SeededRng`].
pub(super) fn seeded_rng() -> Arc<SeededRng> {
    let n = ENGINES_BUILT.with(|c| {
        let n = c.get();
        c.set(n + 1);
        n
    });
    Arc::new(SeededRng(PLMutex::new(
        <rand::rngs::StdRng as rand_core::SeedableRng>::seed_from_u64(0x5EED_0000 + n),
    )))
}

/// Restart this thread's [`seeded_rng`] sequence at its first seed, so
/// the engines one proptest case builds do not depend on how many the
/// cases and shrink steps before it built.
pub(super) fn begin_case() {
    ENGINES_BUILT.with(|c| c.set(0));
}

pub(super) fn engine() -> Engine {
    let keychain = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0xab; 32])));
    crate::engine::rebuild::check::enable();
    Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
        seeded_rng(),
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
        time_of_day: Some(sunrise_domain::TimeOfDayRange::new(
            jiff::civil::time(9, 0, 0, 0),
            jiff::civil::time(17, 0, 0, 0),
        )),
        days_of_week: sunrise_domain::WeekdaySet::new(),
        date_range: None,
        severity: sunrise_domain::ConstraintSeverity::Hard,
        unknown: Unknowns::new(),
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
            stream_id: Some(stream),
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        },
        rrule: RRule::parse(rrule).unwrap(),
        timezone: "UTC".into(),
        starts_at: ms_to_ts(starts_ms).into(),
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
                     WHERE routine_id = ? AND deleted = 0 AND planned_at_ms <= ?",
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
        stream_id: None,
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
    SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(12, 0, 0, 0))
        .to_instant(&jiff::tz::TimeZone::UTC)
        .unwrap()
        .as_millisecond() as u64
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
        width: None,
        height: None,
        thumbnail: None,
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

pub(super) fn engine_at_ms(now_ms: i64) -> (Engine, Db) {
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

/// Pair `db` with a device that never advertised a feature (ADR-0045
/// §7), as an older build is: the vault then never requires
/// `core.field_ops`, and its commands write the full-state ops of
/// clarification D9 (ADR-0065 §7's legacy mode).
pub(super) fn legacy_peer(db: &Db) {
    db.conn()
            .execute(
                "INSERT OR IGNORE INTO devices (device_id, cert_blob, nickname, platform, created_at_ms)
                 VALUES (?, x'00', 'older build', 'ios', 0)",
                [&LEGACY_PEER[..]],
            )
            .unwrap();
}

/// The device id [`legacy_peer`] pairs.
pub(super) const LEGACY_PEER: [u8; 16] = [0xEE; 16];

pub(super) fn db_root(root: [u8; 32]) -> Db {
    Db::open_memory(&VaultRootKey::from_bytes(root)).unwrap()
}

pub(super) fn engine_seeded(root: [u8; 32], seed: [u8; 32], clock: Arc<FakeClock>) -> Engine {
    let kc = Arc::new(Keychain::for_test_seeded(
        VaultRootKey::from_bytes(root),
        seed,
    ));
    // Every command and delivery on this test's thread then checks that
    // a rebuild of the log lands on the rows it left (ADR-0065 PR 3).
    crate::engine::rebuild::check::enable();
    Engine::from_clock(clock, seeded_rng(), kc)
}

/// Like [`engine_seeded`] but with **real random** Stream keys, so a key
/// this device does not hold is genuinely one it cannot open.
///
/// Every other engine unit test derives its keys from the shared vault root
/// (see [`Keychain::for_test_seeded`]) precisely so two in-memory engines
/// can read each other with no relay to carry `key_envelope` ops. That is
/// the wrong world for the deferral path, whose whole subject is an op
/// arriving before the key that opens it.
pub(super) fn engine_random_keys(root: [u8; 32], seed: [u8; 32], clock: Arc<FakeClock>) -> Engine {
    let kc = Arc::new(Keychain::for_test_random_keys(
        VaultRootKey::from_bytes(root),
        seed,
    ));
    Engine::from_clock(clock, seeded_rng(), kc)
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
pub(super) fn envelopes_to(engine: &Engine, db: &Db, recipient: &[u8; 16]) -> Vec<([u8; 16], u32)> {
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
/// Fold `op` into `db` as an op stamped `lww` and sealed under `stream`
/// would be folded, without logging it: the row, its merge state and its
/// version, with no op in the log behind them. Nothing is sealed, so no
/// key is minted.
pub(super) fn fold_unlogged(db: &mut Db, op: &InnerOp, lww: &LwwStamp, stream: [u8; 16]) {
    db.with_tx(|tx| materialize_remote(tx, op, lww, &stream))
        .unwrap();
}

/// Log `op` under `stream` and fold it, as a command does, at the
/// engine's clock: how a full-state op written by another build, say one
/// carrying fields this build does not model, reaches the projection.
pub(super) fn log_and_fold(e: &Engine, db: &mut Db, op: &InnerOp, stream: &[u8; 16]) {
    let now_ms = e.clock.now_ms();
    db.with_tx(|tx| e.append_and_fold(tx, op, stream, now_ms).map(|_| ()))
        .unwrap();
}

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
        rehomed_to: None,
        deleted: false,
        unknown: Unknowns::new(),
    };
    fold_unlogged(
        db,
        &InnerOp::StreamCreate(stream),
        &seeded_stamp(),
        META_STREAM,
    );
    // A row no op wrote: no rebuild of the log reproduces it.
    crate::engine::rebuild::check::disable();
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
    fold_unlogged(
        db,
        &InnerOp::ContextCreate(ctx),
        &seeded_stamp(),
        META_STREAM,
    );
    // A row no op wrote: no rebuild of the log reproduces it.
    crate::engine::rebuild::check::disable();
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
        split_from: None,
        streak_keys: Vec::new(),
        paused: false,
        paused_until: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    };
    // The fold materializes the template Stream's row for the foreign key
    // (`ensure_stream_row`) without emitting an op, which is exactly the
    // property this helper needs.
    fold_unlogged(
        db,
        &InnerOp::RoutineCreate(Box::new(routine)),
        &seeded_stamp(),
        META_STREAM,
    );
    // A row no op wrote: no rebuild of the log reproduces it.
    crate::engine::rebuild::check::disable();
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

/// The `ev` names of every event emitted while a closure runs, in order.
///
/// Only the `ev` field is kept, which is the only part of a log line this
/// repository treats as a contract — `crates/sunrise-log/tests/event_catalog.rs`
/// is the gate on that name. The capture, and the tracing interest-cache
/// pin it needs to see anything at all, are `sunrise-log`'s.
pub(super) use sunrise_log::test_util::events_emitted_by;

/// How many `device_revoke` ops this replica has kept, believed or not.
///
/// The register is a fold over these, so "stored and skipped" and "never
/// arrived" look identical in `device_revocations` and are told apart only
/// here.
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
    Option<Vec<u8>>,
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
            "SELECT id, title, state, planned_at_ms, hard_due_at_ms, stream_id,
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
            r.get::<_, Option<Vec<u8>>>(5)?,
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
    Engine::from_clock(clock, seeded_rng(), keychain)
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
        .occurrences_in(window, &jiff::tz::TimeZone::UTC)
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
    (Engine::from_clock(clock.clone(), seeded_rng(), kc), clock)
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
    // Written at the epoch, before anything a test stamps: the op moves the
    // HLC, and a later op the test backdates must not be stamped after it.
    let clock = Arc::new(FakeClock(PLMutex::new(1)));
    let e = engine_seeded([0xab; 32], [0x11; 32], clock.clone());
    let mut db = db();
    // The review fixtures are written around a Monday-first week
    // (`REVIEW_MON`). The week starts on the `week_start` preference,
    // whose default is Sunday, so the vault says Monday.
    e.apply(
        &mut db,
        Command::SetPreference {
            key: "week_start".into(),
            value: sunrise_domain::PrefValue::Weekday(sunrise_domain::Weekday::Mo),
            target: sunrise_domain::PrefTarget::Vault,
        },
    )
    .unwrap();
    set_clock(&clock, REVIEW_MON);
    (e, db, clock)
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

/// An engine over `root` whose clock reads `local` in `zone`, and that instant.
pub(super) fn engine_at(
    root: [u8; 32],
    zone: &'static str,
    local: jiff::civil::DateTime,
) -> (Engine, u64) {
    let tz = jiff::tz::TimeZone::get(zone).unwrap();
    let now = u64::try_from(tz.to_zoned(local).unwrap().timestamp().as_millisecond()).unwrap();
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes(root)));
    let e = Engine::from_clock(Arc::new(ZonedClock(now, zone)), Arc::new(SystemRng), kc);
    (e, now)
}

/// Two replicas of one vault that trust each other, on clocks of their own.
pub(super) struct Pair {
    pub(super) ea: Engine,
    pub(super) eb: Engine,
    pub(super) ca: Arc<FakeClock>,
    pub(super) cb: Arc<FakeClock>,
    pub(super) dba: Db,
    pub(super) dbb: Db,
}

/// A [`Pair`] over `root` that has exchanged nothing.
fn pair_under(root: [u8; 32]) -> Pair {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(root, [1u8; 32], ca.clone());
    let eb = engine_seeded(root, [2u8; 32], cb.clone());
    let mut dba = db_root(root);
    let mut dbb = db_root(root);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    Pair {
        ea,
        eb,
        ca,
        cb,
        dba,
        dbb,
    }
}

/// A [`Pair`] over [`ROOT`] that has exchanged nothing.
pub(super) fn pair() -> Pair {
    pair_under(ROOT)
}

/// A [`Pair`] over `root`, each side having delivered what it wrote to the
/// other, after advertising its features if `advertise`.
pub(super) fn synced_pair(root: [u8; 32], advertise: bool) -> Pair {
    let mut p = pair_under(root);
    if advertise {
        assert!(p.ea.advertise_features(&mut p.dba).unwrap());
        assert!(p.eb.advertise_features(&mut p.dbb).unwrap());
    }
    sync(&mut p);
    p
}

/// Every op `from` logged under its own device, delivered to `to` in each
/// stream's order. Re-delivering one `to` already holds is a no-op.
///
/// `strict` panics on a refused op. A test whose subject is the refusal passes
/// `false` and asserts on what the refusal left behind.
pub(super) fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db, strict: bool) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
            .unwrap();
        stmt.query_map(params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        let applied = to.apply_remote_all(to_db, &env);
        if strict {
            applied.unwrap();
        }
    }
}

/// [`deliver`] both ways across `p`, refusing nothing.
pub(super) fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb, true);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba, true);
}

/// Every envelope `db` holds, in the order a relay would hand them over:
/// by stream, device and seq.
pub(super) fn all_envs(db: &Db) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops ORDER BY stream_id, device_id, seq")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Deliver every envelope of `from` to `to`, in relay order, except the ones
/// `to` wrote itself, which a relay does not echo back.
pub(super) fn relay(from: &Db, to: &Engine, to_db: &mut Db) {
    let me = to.keychain.device_id();
    for env in all_envs(from) {
        if decode_envelope(&env).unwrap().device_id != me {
            to.apply_remote_all(to_db, &env).unwrap();
        }
    }
}
