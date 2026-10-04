//! The feature gate (ADR-0045 §7–§8, issue #324).
//!
//! Two builds are stood up over one vault with [`Engine::with_features`]: a
//! newer one that supports `task.probe`, whose op kind is `TaskProbe`, and an
//! older one that supports nothing. Neither `TaskProbe` nor `task.probe`
//! exists in this build's `InnerOp` or registry, which is exactly an older
//! build's position: the op parks, and the feature is missing.

use super::*;
use crate::control_op::{DeviceFeaturesPayload, VaultRequiresPayload};
use crate::feature::{Feature, FeatureScope, MissingFeature};

static NEWER: &[Feature] = &[Feature {
    id: "task.probe",
    op_kinds: &["TaskProbe"],
    fields: &[],
    field_op_kinds: &[],
    since: 8,
}];

static STRUCTURAL: &[Feature] = &[Feature {
    id: "core.probe",
    op_kinds: &[],
    fields: &[],
    field_op_kinds: &[],
    since: 8,
}];

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock(PLMutex::new(T0)))
}

/// Every envelope in `db`'s log whose op-log kind is `inner_kind`, in seq
/// order.
fn envs_of_kind(db: &Db, inner_kind: &str) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE inner_kind = ? ORDER BY seq")
        .unwrap();
    stmt.query_map(params![inner_kind], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn required(db: &Db) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT feature FROM vault_required_features ORDER BY feature")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Apply a `VaultRequires` from `sender` straight into `db`, as `trust` does
/// for a cert: the receive path past the envelope checks.
fn apply_requires(receiver: &Engine, db: &mut Db, sender: &Engine, ids: &[&str]) {
    let inner = InnerOp::VaultRequires(VaultRequiresPayload {
        features: ids.iter().map(|s| (*s).to_owned()).collect(),
        unknown: Unknowns::new(),
    });
    let sender_id = sender.keychain.device_id();
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, &inner, &sender_id, Hlc::at(1), T0, 1)
            .map(|_| ())
    })
    .unwrap();
}

fn apply_device_features(receiver: &Engine, db: &mut Db, sender: [u8; 16], hlc: Hlc, ids: &[&str]) {
    let inner = InnerOp::DeviceFeatures(DeviceFeaturesPayload {
        features: ids.iter().map(|s| (*s).to_owned()).collect(),
        unknown: Unknowns::new(),
    });
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, &inner, &sender, hlc, T0, 1)
            .map(|_| ())
    })
    .unwrap();
}

/// The issue's acceptance test. An older build receives `VaultRequires` for
/// a feature it lacks: its writes on the feature's scope are refused and
/// leave nothing behind, everything else it can still write, it keeps
/// reading and applying, it parks the feature's op rather than dropping it,
/// and after the upgrade nothing has been lost and the lock is gone.
#[test]
fn an_older_build_that_receives_vault_requires_refuses_writes_and_keeps_reading() {
    let ea = engine_seeded(ROOT, [1u8; 32], clock()).with_features(NEWER);
    let eb = engine_seeded(ROOT, [2u8; 32], clock()).with_features(&[]);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    // B has never said it supports anything, so A may not enable the
    // feature over it without the user's word, and is told which device.
    let refused = ea.require_features(&mut dba, &["task.probe"], false);
    assert!(
        matches!(
            &refused,
            Err(EngineError::FeatureUnsupportedByDevices { feature, devices })
                if feature == "task.probe" && devices == &vec![b_id]
        ),
        "got {refused:?}"
    );
    assert!(required(&dba).is_empty(), "nothing was emitted");
    assert_eq!(
        ea.require_features(&mut dba, &["task.probe"], true)
            .unwrap(),
        vec!["task.probe".to_owned()]
    );
    assert!(
        ea.require_features(&mut dba, &["task.probe"], true)
            .unwrap()
            .is_empty(),
        "a feature already required emits nothing"
    );

    // A writes an ordinary task, then an op of the feature's kind.
    let seen = new_task(&ea, &mut dba, "written before the lock");
    let inbox = decode_envelope(&create_env_for(&dba, seen.bytes()))
        .unwrap()
        .stream_id;
    let probe = emit_raw_inner(&ea, &mut dba, &inbox, &future_kind_inner("TaskProbe"));

    // B receives all three.
    let requires = envs_of_kind(&dba, "vault.requires");
    assert_eq!(requires.len(), 1);
    let events = eb.apply_remote_all(&mut dbb, &requires[0]).unwrap();
    assert!(
        matches!(
            events.as_slice(),
            [DomainEvent::Updated(r)] if *r == EntityRef::new(EntityKind::Stream, META_STREAM)
        ),
        "a client is told to re-read what it may edit, got {events:?}"
    );
    assert!(eb
        .apply_remote(&mut dbb, &create_env_for(&dba, seen.bytes()))
        .unwrap()
        .is_some());
    assert!(eb.apply_remote_all(&mut dbb, &probe).unwrap().is_empty());

    assert_eq!(
        eb.missing_features(&dbb).unwrap(),
        vec![MissingFeature {
            id: "task.probe".into(),
            scope: FeatureScope::Entity(EntityKind::Task),
        }]
    );

    // Writes on the scope are refused, and leave no op behind.
    let ops_before = op_count(&dbb);
    let create = eb.apply(
        &mut dbb,
        Command::CreateTask(TaskDraft {
            title: "refused".into(),
            ..Default::default()
        }),
    );
    assert!(
        matches!(&create, Err(EngineError::FeatureMissing { feature }) if feature == "task.probe"),
        "got {create:?}"
    );
    assert!(matches!(
        eb.apply(&mut dbb, Command::CompleteTask(seen)),
        Err(EngineError::FeatureMissing { .. })
    ));
    assert_eq!(op_count(&dbb), ops_before, "a refused write writes nothing");
    assert_eq!(read_task_t(&eb, &dbb, seen).state, TaskState::Todo);

    // Off the scope, B still writes.
    eb.apply(
        &mut dbb,
        Command::CreateStream(StreamDraft {
            name: "still writable".into(),
            ..Default::default()
        }),
    )
    .expect("a stream is outside task.probe's scope");

    // B keeps reading and syncing: A's task is there, and the feature's op is
    // parked, not dropped.
    assert_eq!(
        read_task_t(&eb, &dbb, seen).title,
        "written before the lock"
    );
    assert_eq!(parked_rows(&dbb).len(), 1);

    // The upgrade: the same device, now on a build that has the feature.
    let upgraded = engine_seeded(ROOT, [2u8; 32], clock()).with_features(NEWER);
    assert!(upgraded.missing_features(&dbb).unwrap().is_empty());
    upgraded
        .apply(&mut dbb, Command::CompleteTask(seen))
        .expect("the lock is gone after the upgrade");
    // Nothing was lost while B was locked: the parked op is still held, for
    // a build that knows its kind to replay.
    let probe_op = decode_envelope(&probe).unwrap();
    let probe_id = remote_op_id(&inbox, &ea.keychain.device_id(), probe_op.seq);
    assert_eq!(op_row(&dbb, &probe_id).0, "unknown");
    assert_eq!(parked_rows(&dbb).len(), 1);
    assert_eq!(
        read_task_t(&upgraded, &dbb, seen).title,
        "written before the lock"
    );
}

/// A structural feature locks every entity write, and leaves the control
/// writes a vault needs for safety alone: rotating a key still works.
#[test]
fn a_structural_feature_locks_every_entity_write_but_not_key_rotation() {
    let ea = engine_seeded(ROOT, [1u8; 32], clock()).with_features(STRUCTURAL);
    let eb = engine_random_keys(ROOT, [2u8; 32], clock()).with_features(&[]);
    let mut dbb = db_root(ROOT);
    eb.ensure_base_epochs(&mut dbb).unwrap();
    apply_requires(&eb, &mut dbb, &ea, &["core.probe"]);
    assert_eq!(
        eb.missing_features(&dbb).unwrap(),
        vec![MissingFeature {
            id: "core.probe".into(),
            scope: FeatureScope::Structural,
        }]
    );

    for cmd in [
        Command::CreateStream(StreamDraft {
            name: "s".into(),
            ..Default::default()
        }),
        Command::CreateContext(ContextDraft {
            name: "c".into(),
            ..Default::default()
        }),
        Command::CreateTask(TaskDraft {
            title: "t".into(),
            ..Default::default()
        }),
    ] {
        assert!(
            matches!(
                eb.apply(&mut dbb, cmd.clone()),
                Err(EngineError::FeatureMissing { ref feature }) if feature == "core.probe"
            ),
            "{cmd:?} must be refused"
        );
    }
    eb.apply(
        &mut dbb,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, META_STREAM),
        },
    )
    .expect("key rotation is a control write and stays allowed");
}

/// Routine materialization writes tasks, so a task lock refuses it with the
/// typed error `Core::open` treats as the read-only state working rather than
/// a failed open, and it writes none of them.
#[test]
fn routine_materialization_is_refused_with_the_typed_error_under_a_task_lock() {
    let mut db = db();
    let e = engine();
    let other = engine_seeded(ROOT, [9u8; 32], clock());
    e.apply(
        &mut db,
        Command::CreateRoutine(routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        )),
    )
    .unwrap();
    apply_requires(&e, &mut db, &other, &["task.probe"]);
    let tasks_before = live_task_ids(&db);
    let later = NOW as u64 + 30 * DAY_MS as u64;
    assert!(matches!(
        e.apply(&mut db, Command::MaterializeRoutines { now_ms: later }),
        Err(EngineError::FeatureMissing { ref feature }) if feature == "task.probe"
    ));
    assert_eq!(
        live_task_ids(&db),
        tasks_before,
        "no occurrence was written"
    );
}

/// A feature whose prefix names an entity this build does not have locks
/// nothing here, because nothing here writes that entity, and is still
/// reported so the user is told to update.
#[test]
fn a_feature_of_an_entity_this_build_lacks_is_reported_and_locks_nothing() {
    let ea = engine_seeded(ROOT, [1u8; 32], clock());
    let eb = engine_seeded(ROOT, [2u8; 32], clock());
    let mut dbb = db_root(ROOT);
    apply_requires(&eb, &mut dbb, &ea, &["place.entity", "Not An Id", "core"]);
    assert_eq!(
        required(&dbb),
        vec!["place.entity".to_owned()],
        "malformed ids are skipped"
    );
    assert_eq!(
        eb.missing_features(&dbb).unwrap(),
        vec![MissingFeature {
            id: "place.entity".into(),
            scope: FeatureScope::UnknownEntity("place".into()),
        }]
    );
    new_task(&eb, &mut dbb, "still writable");
}

/// The required set only grows, and it converges whatever order two enables
/// arrive in.
#[test]
fn the_required_set_is_a_grow_only_union() {
    let ea = engine_seeded(ROOT, [1u8; 32], clock());
    let eb = engine_seeded(ROOT, [2u8; 32], clock());
    let mut one = db_root(ROOT);
    let mut two = db_root(ROOT);
    apply_requires(&eb, &mut one, &ea, &["task.a"]);
    apply_requires(&eb, &mut one, &ea, &["stream.b", "task.a"]);
    apply_requires(&eb, &mut two, &ea, &["stream.b", "task.a"]);
    apply_requires(&eb, &mut two, &ea, &["task.a"]);
    assert_eq!(required(&one), required(&two));
    assert_eq!(
        required(&one),
        vec!["stream.b".to_owned(), "task.a".to_owned()]
    );
}

/// A feature's op cannot be sealed before the vault requires the feature:
/// forgetting `require_features` is a failing command, never an older
/// device overwriting data it was not warned about.
#[test]
fn an_op_that_uses_a_feature_the_vault_does_not_require_is_refused() {
    let ea = engine_seeded(ROOT, [1u8; 32], clock()).with_features(NEWER);
    let mut dba = db_root(ROOT);
    let device = ea.keychain.device_id();
    let stream = INBOX_STREAM_BYTES;
    let err = dba
        .with_tx(|tx| {
            let seq = ea.next_seq_tx(tx, &stream)?;
            ea.ops_insert(
                tx,
                &remote_op_id(&stream, &device, seq),
                &stream,
                seq,
                ea.hlc.send(),
                &future_kind_inner("TaskProbe"),
                "future.kind",
                "future",
                None,
                Some(T0),
                None,
                T0,
                &[],
            )
        })
        .map_err(EngineError::from)
        .map_err(EngineError::lift_seal_refusal)
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains("task.probe")),
        "got {err:?}"
    );
    assert_eq!(op_count(&dba), 0, "the transaction rolled back");
}

/// A build may only promise features it has.
#[test]
fn requiring_a_feature_this_build_lacks_is_invalid() {
    let e = engine_seeded(ROOT, [1u8; 32], clock());
    let mut db = db_root(ROOT);
    assert!(matches!(
        e.require_features(&mut db, &["task.probe"], true),
        Err(EngineError::Invalid(_))
    ));
}

/// `DeviceFeatures` is read as the latest per device, whatever order the ops
/// arrive in, and a device that advertises a feature no longer stands in the
/// way of enabling it.
#[test]
fn device_features_keeps_each_devices_latest_and_clears_the_enable_guard() {
    let ea = engine_seeded(ROOT, [1u8; 32], clock()).with_features(NEWER);
    let eb = engine_seeded(ROOT, [2u8; 32], clock());
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    apply_device_features(&ea, &mut dba, b_id, Hlc::at(20), &["task.probe"]);
    apply_device_features(&ea, &mut dba, b_id, Hlc::at(10), &["task.other"]);
    let stored: String = dba
        .conn()
        .query_row(
            "SELECT features FROM device_features WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, "task.probe",
        "an older op does not replace a newer one"
    );

    assert_eq!(
        ea.require_features(&mut dba, &["task.probe"], false)
            .unwrap(),
        vec!["task.probe".to_owned()],
        "every other device supports it, so no confirmation is needed"
    );
}

/// A build advertises once per change of what it supports, and a build that
/// supports nothing never advertises.
#[test]
fn a_build_advertises_its_features_once_per_change() {
    let mut db = db_root(ROOT);
    let none = engine_seeded(ROOT, [1u8; 32], clock());
    assert!(!none.advertise_features(&mut db).unwrap());
    assert!(envs_of_kind(&db, "device.features").is_empty());

    let newer = engine_seeded(ROOT, [1u8; 32], clock()).with_features(NEWER);
    assert!(newer.advertise_features(&mut db).unwrap());
    assert!(!newer.advertise_features(&mut db).unwrap());
    let envs = envs_of_kind(&db, "device.features");
    assert_eq!(envs.len(), 1);
    let inner = newer.open_op_row(&db, &{
        let id: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT op_id FROM ops WHERE inner_kind = 'device.features'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        to16(&id).unwrap()
    });
    let InnerOp::DeviceFeatures(p) = decode_inner_op(&inner.unwrap()).unwrap() else {
        panic!("a DeviceFeatures op");
    };
    assert_eq!(p.features, vec!["task.probe".to_owned()]);
}
