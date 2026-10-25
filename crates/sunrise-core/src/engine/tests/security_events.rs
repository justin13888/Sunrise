//! The catalog's reads: security events from the rows each op leaves, the
//! deadline candidates, the outbox's age, and routine reminders
//! (`docs/08-features/notifications.md` #2, #3, #12–#15, #347).
//!
//! Real vaults: `Keychain::open` writes the identity and the certificate a
//! shipping build writes, and a rotation is the command a user runs.

use super::testutil::*;
use super::*;
use crate::alerts::SecurityEventKind as K;
use crate::unlock::IdentitySeed;

const ROOT_A: [u8; 32] = [0x61; 32];
const ROOT_B: [u8; 32] = [0x62; 32];
const SEED: [u8; 32] = [0x6a; 32];

/// A vault opened at `at`, as `Core::open` leaves one.
fn open_at(root: [u8; 32], seed: &IdentitySeed, at: u64) -> (Engine, Db, Arc<FakeClock>) {
    let mut db = db_root(root);
    let c = Arc::new(FakeClock(PLMutex::new(at)));
    let kc = Keychain::open(
        &mut db,
        VaultRootKey::from_bytes(root),
        c.as_ref(),
        &SystemRng,
        seed,
    )
    .unwrap();
    let e = Engine::from_clock(c.clone(), Arc::new(SystemRng), Arc::new(kc));
    e.ensure_base_epochs(&mut db).unwrap();
    e.publish_device_cert(&mut db).unwrap();
    (e, db, c)
}

fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
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
        let _ = to.apply_remote_all(to_db, &env);
    }
}

fn kinds(e: &Engine, db: &Db) -> Vec<(K, [u8; 16])> {
    e.security_events(db)
        .unwrap()
        .into_iter()
        .map(|ev| (ev.kind, ev.subject))
        .collect()
}

/// A device that joins after this one is news here and not there; a
/// rotation that keeps the code is no event, one that retires it is one on
/// every device; a revocation is one on every device, its subject's
/// included.
#[test]
fn security_events_are_what_happened_after_this_device_joined() {
    let (a, mut adb, a_clock) = open_at(ROOT_A, &IdentitySeed::Own, T0);
    let blob = a.seal_recovery_blob(&adb, &SEED).unwrap();
    let (genesis, _) = a.keychain.genesis_pair(&adb).unwrap();
    let words = sunrise_crypto::unseal_recovery_blob(&blob, &SEED, &genesis).unwrap();
    assert!(kinds(&a, &adb).is_empty(), "a new vault has no news");

    let (b, mut bdb, _) = open_at(
        ROOT_B,
        &IdentitySeed::Recovered(Box::new(words)),
        T0 + 60_000,
    );
    let b_id = b.keychain.device_id();
    deliver(&b, &bdb, &a, &mut adb);
    deliver(&a, &adb, &b, &mut bdb);
    assert_eq!(kinds(&a, &adb), vec![(K::DeviceJoined, b_id)]);
    let joined = &a.security_events(&adb).unwrap()[0];
    assert_eq!(joined.at_ms, T0 + 60_000, "the certificate's own time");
    assert!(joined.nickname.is_some() && !joined.this_device);
    assert!(
        kinds(&b, &bdb).is_empty(),
        "a device that was there first is not news to the one that joined"
    );

    // A rotation that keeps the code is no event; one that retires it is.
    *a_clock.0.lock() = T0 + 120_000;
    a.apply(
        &mut adb,
        Command::RotateIdentity {
            keep_recovery_code: true,
        },
    )
    .unwrap();
    assert_eq!(kinds(&a, &adb).len(), 1, "a carried code still works");
    *a_clock.0.lock() = T0 + 180_000;
    a.apply(
        &mut adb,
        Command::RotateIdentity {
            keep_recovery_code: false,
        },
    )
    .unwrap();
    let head = a.current_identity(adb.conn()).unwrap().identity_id;
    assert_eq!(
        kinds(&a, &adb),
        vec![(K::DeviceJoined, b_id), (K::RecoveryCodeRetired, head)]
    );
    deliver(&a, &adb, &b, &mut bdb);
    assert_eq!(kinds(&b, &bdb), vec![(K::RecoveryCodeRetired, head)]);

    // A revocation, as the fold leaves it, names its subject on each device.
    for db in [&adb, &bdb] {
        db.conn()
            .execute(
                "INSERT INTO device_revocations
                 (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
                 VALUES (?, ?, 0, X'', 'Lost', ?)",
                params![&b_id[..], T0 + 240_000, T0 + 240_000],
            )
            .unwrap();
    }
    let on_b = b.security_events(&bdb).unwrap();
    assert_eq!(
        on_b.last().map(|e| (e.kind, e.this_device)),
        Some((K::DeviceRevoked, true))
    );
    let on_a = a.security_events(&adb).unwrap();
    assert_eq!(
        on_a.last().map(|e| (e.kind, e.this_device)),
        Some((K::DeviceRevoked, false))
    );
    assert_eq!(on_a.last().unwrap().at_ms, T0 + 240_000);
}

#[test]
fn deadlines_are_the_open_tasks_with_a_hard_one_and_routines_plan_as_routines() {
    let e = engine();
    let mut db = db();
    let due = SunriseTime::instant(ms_to_ts(T0 as i64 + 3_600_000));
    let hard = new_task(&e, &mut db, "file taxes");
    let soft = new_task(&e, &mut db, "tidy desk");
    let done = new_task(&e, &mut db, "paid rent");
    for (id, patch) in [
        (
            hard,
            TaskPatch {
                hard_due_at: Some(Some(due.clone())),
                ..TaskPatch::default()
            },
        ),
        (
            soft,
            TaskPatch {
                target_at: Some(Some(due.clone())),
                ..TaskPatch::default()
            },
        ),
        (
            done,
            TaskPatch {
                hard_due_at: Some(Some(due.clone())),
                state: Some(TaskState::Done),
                ..TaskPatch::default()
            },
        ),
    ] {
        e.apply(&mut db, Command::UpdateTask { id, patch }).unwrap();
    }
    let got = e.deadline_candidates(&db).unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        (got[0].task, got[0].title.as_str(), &got[0].hard_due_at),
        (hard, "file taxes", &due)
    );

    // A routine occurrence's reminder is the routine's, never also a task's.
    let soon = SunriseTime::instant(ms_to_ts(T0 as i64 + 600_000));
    for id in [soft, hard] {
        let patch = TaskPatch {
            planned_at: Some(Some(soon.clone())),
            ..TaskPatch::default()
        };
        e.apply(&mut db, Command::UpdateTask { id, patch }).unwrap();
    }
    db.conn()
        .execute(
            "UPDATE tasks SET routine_id = ? WHERE id = ?",
            params![&[7u8; 16][..], &soft.bytes()[..]],
        )
        .unwrap();
    let plan = e
        .reminder_plan(&db, T0, T0 + 86_400_000, &ReminderSettings::default())
        .unwrap();
    let kind_of = |id| plan.intents.iter().find(|i| i.entity == id).map(|i| i.kind);
    assert_eq!(plan.intents.len(), 2);
    assert_eq!(kind_of(soft), Some(sunrise_domain::ReminderKind::Routine));
    assert_eq!(kind_of(hard), Some(sunrise_domain::ReminderKind::Task));

    // The outbox's oldest unacknowledged op is the first write's.
    let oldest = e.oldest_unacked_ms(&db).unwrap();
    assert!(
        oldest.is_some_and(|ms| ms <= e.clock.now_ms()),
        "{oldest:?}"
    );
    db.conn()
        .execute("UPDATE outbox SET acked_at_ms = 1", [])
        .unwrap();
    assert_eq!(e.oldest_unacked_ms(&db).unwrap(), None);
}
