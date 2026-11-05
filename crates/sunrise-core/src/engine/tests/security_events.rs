//! The catalog's reads: security events from the rows each op leaves, the
//! deadline candidates, the outbox's age, and routine reminders
//! (`docs/08-features/notifications.md` #2, #3, #12–#15, #347).
//!
//! Real vaults: `Keychain::open` writes the identity and the certificate a
//! shipping build writes, and a rotation is the command a user runs.

use super::support::*;
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
    let e = Engine::from_clock(c.clone(), seeded_rng(), Arc::new(kc));
    e.ensure_base_epochs(&mut db).unwrap();
    e.publish_device_cert(&mut db).unwrap();
    (e, db, c)
}

fn deliver_cert_first(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            // The certificate first: it makes the device known, and the
            // receiver refuses every other op of a device it does not know.
            .prepare(
                "SELECT envelope FROM ops WHERE device_id = ?
                 ORDER BY inner_kind <> 'device.cert', stream_id, seq",
            )
            .unwrap();
        stmt.query_map(params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        to.apply_remote_all(to_db, &env).unwrap();
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
    deliver_cert_first(&b, &bdb, &a, &mut adb);
    deliver_cert_first(&a, &adb, &b, &mut bdb);
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
    deliver_cert_first(&a, &adb, &b, &mut bdb);
    assert_eq!(kinds(&b, &bdb), vec![(K::RecoveryCodeRetired, head)]);

    // A revocation names its subject on each device.
    *a_clock.0.lock() = T0 + 240_000;
    a.apply(
        &mut adb,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    deliver_cert_first(&a, &adb, &b, &mut bdb);
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
    let soon_ms = T0 as i64 + 600_000;
    let patch = TaskPatch {
        planned_at: Some(Some(SunriseTime::instant(ms_to_ts(soon_ms)))),
        ..TaskPatch::default()
    };
    e.apply(&mut db, Command::UpdateTask { id: hard, patch })
        .unwrap();
    let routine = e
        .apply(
            &mut db,
            Command::CreateRoutine(routine_draft(
                inbox_stream_ref(),
                "FREQ=DAILY",
                soon_ms,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            )),
        )
        .unwrap()
        .entity;
    e.apply(&mut db, Command::MaterializeRoutines { now_ms: T0 })
        .unwrap();
    let (occurrence, occurrence_at) = occurrence_tasks(&routine_of(&e, &db, routine), 1)[0];
    assert_eq!(occurrence_at, soon_ms);
    let plan = e
        .reminder_plan(&db, T0, T0 + 86_400_000, &ReminderSettings::default())
        .unwrap();
    let kind_of = |id| plan.intents.iter().find(|i| i.entity == id).map(|i| i.kind);
    assert_eq!(plan.intents.len(), 2, "{:?}", plan.intents);
    assert_eq!(
        kind_of(occurrence),
        Some(sunrise_domain::ReminderKind::Routine)
    );
    assert_eq!(kind_of(hard), Some(sunrise_domain::ReminderKind::Task));

    // The outbox's oldest unacknowledged op is the first write's.
    assert_eq!(e.oldest_unacked_ms(&db).unwrap(), Some(T0));
    // The relay acknowledges every op, as a sync session records it.
    let pending = Outbox::list_unacked(&db).unwrap();
    assert!(!pending.is_empty());
    db.with_tx(|tx| {
        for entry in &pending {
            Outbox::mark_acked(tx, &entry.op_id, T0)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(e.oldest_unacked_ms(&db).unwrap(), None);
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

// ---- notification reads (issue #9) ----

#[test]
fn the_morning_summary_reads_the_previous_calendar_date() {
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
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
                planned_at: Some(SunriseTime::instant(ms_to_ts(
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
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
    let day = 86_400_000i64;
    let today = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "This evening".into(),
                planned_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 6 * 3_600_000))),
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
                planned_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 3 * day))),
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
                planned_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 30 * day))),
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
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
    let at = NOTIFY_NOON + 2 * 3_600_000;
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "Call the bank".into(),
                planned_at: Some(SunriseTime::instant(ms_to_ts(at))),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                stream_id: None,
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
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
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
                planned_at: Some(SunriseTime::instant(ms_to_ts(at))),
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
                planned_at: Some(SunriseTime::instant(ms_to_ts(at))),
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
                planned_at: Some(SunriseTime::instant(ms_to_ts(at))),
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
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
    let at = NOTIFY_NOON + 3_600_000;
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "Call the bank".into(),
                planned_at: Some(SunriseTime::instant(ms_to_ts(at))),
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
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "Call the bank".into(),
            planned_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 3_600_000))),
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
