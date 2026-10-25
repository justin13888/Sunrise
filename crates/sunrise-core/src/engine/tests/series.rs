//! A routine's series (#331, `docs/02-domain/routines-and-recurrence.md`
//! §Edit scope, §Template propagation, §Catch-up for occurrences already
//! materialized, §Scheduling constraints on occurrences): edit scopes and
//! the split, template propagation to untouched occurrences, catch-up over
//! occurrences generated earlier, and the hard-constraint flag.

use super::*;
use sunrise_domain::{parse_occurrence_key, split_routine_id, EditScope};

const ROOT_331: [u8; 32] = [0x31; 32];

/// The routine every test here starts from: daily at `NOW + 1h` in UTC,
/// with the given catch-up policy, materialized over its 14-day horizon.
fn daily(e: &Engine, db: &mut Db, policy: RoutineCatchupPolicy) -> (EntityRef, Vec<String>) {
    let draft = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        policy,
        Vec::new(),
    );
    let rid = e.apply(db, Command::CreateRoutine(draft)).unwrap().entity;
    let r = routine_of(e, db, rid);
    let keys = r
        .occurrences_in(
            (ms_to_ts(NOW), ms_to_ts(NOW + 14 * DAY_MS)),
            &jiff::tz::TimeZone::UTC,
        )
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    (rid, keys)
}

fn task(db: &Db, id: EntityRef) -> Task {
    read_task(db.conn(), id.bytes())
        .unwrap()
        .expect("projected")
}

fn stored(db: &Db, rid: EntityRef) -> Routine {
    crate::engine::routine::read_routine(db.conn(), rid.bytes())
        .unwrap()
        .expect("projected")
}

/// Every live task of the series rooted at `root`, by id.
fn live_series(db: &Db, routines: &[EntityRef]) -> Vec<Task> {
    let mut out = Vec::new();
    for r in routines {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM tasks WHERE routine_id = ? AND deleted = 0 ORDER BY id")
            .unwrap();
        let ids: Vec<Vec<u8>> = stmt
            .query_map(params![&r.bytes()[..]], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for raw in ids {
            out.push(task(db, EntityRef::new(EntityKind::Task, blob16(&raw))));
        }
    }
    out.sort_by_key(|t| t.id);
    out
}

fn title_patch(r: &Routine, title: &str) -> RoutinePatch {
    let mut template = r.template.clone();
    template.title = title.into();
    RoutinePatch {
        template: Some(template),
        ..Default::default()
    }
}

fn edit(
    e: &Engine,
    db: &mut Db,
    rid: EntityRef,
    scope: EditScope,
    patch: RoutinePatch,
) -> CommandResult {
    e.apply(
        db,
        Command::UpdateRoutine {
            id: rid,
            scope,
            patch,
        },
    )
    .unwrap()
}

fn retitle_task(e: &Engine, db: &mut Db, id: EntityRef, title: &str) {
    e.apply(
        db,
        Command::UpdateTask {
            id,
            patch: TaskPatch {
                title: Some(title.into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
}

fn titles(e: &Engine, db: &Db, q: Query) -> Vec<String> {
    match e.query(db, q).unwrap() {
        QueryResult::Tasks(ts) | QueryResult::StreamTasks(ts) => {
            ts.into_iter().map(|t| t.title).collect()
        }
        other => panic!("{other:?}"),
    }
}

fn group_members(db: &Db) -> Vec<(Vec<u8>, i64)> {
    let mut stmt = db
        .conn()
        .prepare("SELECT group_id, group_size FROM pending_groups ORDER BY group_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// ---- edit scope: this and future ----

/// An edit "this and future" splits the series at the key (the issue's
/// series-split test): the old routine ends before it, a new routine named
/// by the root and the key continues it with the edit, and the occurrences
/// at or after the key are the same tasks, adopted, with the new title. A
/// touched one is left as the user left it, and nothing is generated twice.
#[test]
fn a_this_and_future_edit_splits_the_series() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let before = live_series(&db, &[rid]);
    assert_eq!(before.len(), keys.len());
    // The user edits the fifth occurrence before the split.
    let touched = r.occurrence_task_id(&keys[4]);
    retitle_task(&e, &mut db, touched, "Water plants, twice");
    let groups_before = group_members(&db).len();

    let pivot = keys[3].clone();
    let res = edit(
        &e,
        &mut db,
        rid,
        EditScope::ThisAndFuture { key: pivot.clone() },
        title_patch(&r, "Water the garden"),
    );
    let next_id = split_routine_id(&rid, &pivot);
    assert_eq!(res.entity, next_id, "the result names the continuation");

    let old = stored(&db, rid);
    let next = stored(&db, next_id);
    assert_eq!(next.split_from, Some(rid));
    assert_eq!(next.template.title, "Water the garden");
    assert_eq!(old.template.title, "Water plants");
    assert!(old.rrule.until.is_some());
    assert!(old.has_occurrence(&keys[2]) && !old.has_occurrence(&pivot));

    for (i, k) in keys.iter().enumerate() {
        let t = task(&db, r.occurrence_task_id(k));
        assert!(!t.deleted, "{k}: an occurrence is never lost to a split");
        if i < 3 {
            assert_eq!(
                (t.title.as_str(), t.routine_id),
                ("Water plants", Some(rid))
            );
        } else if t.id == touched {
            assert_eq!(
                t.title, "Water plants, twice",
                "a touched one is left alone"
            );
            assert_eq!(t.routine_id, Some(rid));
        } else {
            assert_eq!(t.title, "Water the garden", "{k}");
            assert_eq!(t.routine_id, Some(next_id), "{k} is adopted");
        }
    }
    // Every key once: the continuation names its occurrences by the root, so
    // it adopted the ones it found and generated none of them again.
    let after = live_series(&db, &[rid, next_id]);
    assert_eq!(after.len(), before.len());
    // The ended routine, the continuation and the adopted occurrences are
    // one group (ADR-0065 §6).
    let members = &group_members(&db)[groups_before..];
    let adopted = keys.len() - 3 - 1;
    assert_eq!(members.len(), 2 + adopted);
    assert!(members.iter().all(|m| m == &members[0]));
    assert_eq!(members[0].1, i64::try_from(2 + adopted).unwrap());
    e.assert_rebuild_identity(&db);
}

/// A split that moves the time re-keys the continuation: the untouched old
/// occurrences at or after the key are tombstoned, the continuation
/// generates its own at the new time, and a touched one stays.
#[test]
fn a_split_that_moves_the_time_tombstones_what_the_continuation_lacks() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let touched = r.occurrence_task_id(&keys[5]);
    retitle_task(&e, &mut db, touched, "Mine");
    let pivot = keys[3].clone();
    let civil = parse_occurrence_key(&pivot).unwrap();
    let later = civil.checked_add(jiff::Span::new().minutes(30)).unwrap();
    let res = edit(
        &e,
        &mut db,
        rid,
        EditScope::ThisAndFuture { key: pivot },
        RoutinePatch {
            starts_at: Some(SunriseTime::zoned(later, "UTC")),
            ..Default::default()
        },
    );
    for k in &keys[3..] {
        let t = task(&db, r.occurrence_task_id(k));
        assert_eq!(t.deleted, t.id != touched, "{k}");
    }
    let next = stored(&db, res.entity);
    let fresh = live_series(&db, &[res.entity]);
    assert!(!fresh.is_empty());
    for t in &fresh {
        let key = next
            .occurrence_key_of(&t.id, t.routine_occurrence.unwrap())
            .expect("a continuation key");
        assert!(key.ends_with(&format!("{:02}:{:02}", later.hour(), later.minute())));
        assert_eq!(t.id, next.occurrence_task_id(&key));
    }
    e.assert_rebuild_identity(&db);
}

/// Splitting where the series was already split edits the continuation.
#[test]
fn splitting_again_at_the_same_key_edits_the_continuation() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let scope = EditScope::ThisAndFuture {
        key: keys[2].clone(),
    };
    let first = edit(&e, &mut db, rid, scope.clone(), title_patch(&r, "A"));
    let second = edit(&e, &mut db, rid, scope, title_patch(&r, "B"));
    assert_eq!(first.entity, second.entity);
    assert_eq!(stored(&db, first.entity).template.title, "B");
    assert_eq!(
        task(&db, r.occurrence_task_id(&keys[6])).title,
        "B",
        "the continuation's edit propagates"
    );
}

/// A key the series does not produce is refused, and nothing is written.
#[test]
fn a_split_at_a_key_the_series_lacks_is_refused() {
    let e = engine();
    let mut db = db();
    let (rid, _) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let ops = db
        .conn()
        .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get::<_, i64>(0))
        .unwrap();
    let err = e
        .apply(
            &mut db,
            Command::UpdateRoutine {
                id: rid,
                scope: EditScope::ThisAndFuture {
                    key: "2023-11-20T07:00".into(),
                },
                patch: title_patch(&r, "x"),
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    let after = db
        .conn()
        .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert_eq!(ops, after);
}

/// Two replicas that fold field ops.
struct Pair {
    ea: Engine,
    eb: Engine,
    dba: Db,
    dbb: Db,
}

fn pair(advertise: bool) -> Pair {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT_331, [1u8; 32], ca);
    let eb = engine_seeded(ROOT_331, [2u8; 32], cb);
    let mut dba = db_root(ROOT_331);
    let mut dbb = db_root(ROOT_331);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    if advertise {
        assert!(ea.advertise_features(&mut dba).unwrap());
        assert!(eb.advertise_features(&mut dbb).unwrap());
    }
    let mut p = Pair { ea, eb, dba, dbb };
    sync(&mut p);
    p
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
        to.apply_remote(to_db, &env).unwrap();
    }
}

fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba);
}

/// A split syncs whole: the peer holds the same two routines and the same
/// occurrences, and materializing there generates nothing twice.
#[test]
fn a_split_converges_on_a_peer_and_generates_nothing_twice() {
    let mut p = pair(true);
    let (rid, keys) = daily(&p.ea, &mut p.dba, RoutineCatchupPolicy::Skip);
    sync(&mut p);
    let r = stored(&p.dba, rid);
    let res = edit(
        &p.ea,
        &mut p.dba,
        rid,
        EditScope::ThisAndFuture {
            key: keys[4].clone(),
        },
        title_patch(&r, "Water the garden"),
    );
    sync(&mut p);
    p.eb.apply(&mut p.dbb, Command::MaterializeRoutines { now_ms: T0 })
        .unwrap();
    sync(&mut p);
    let a = live_series(&p.dba, &[rid, res.entity]);
    let b = live_series(&p.dbb, &[rid, res.entity]);
    assert_eq!(a, b);
    assert_eq!(a.len(), keys.len());
    assert_eq!(stored(&p.dba, res.entity), stored(&p.dbb, res.entity));
    assert_eq!(
        stored(&p.dbb, rid).rrule.until,
        stored(&p.dba, rid).rrule.until
    );
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// Two devices that split one series at one key create one routine: its id
/// is a function of the root and the key.
#[test]
fn two_devices_splitting_at_one_key_make_one_routine() {
    let mut p = pair(true);
    let (rid, keys) = daily(&p.ea, &mut p.dba, RoutineCatchupPolicy::Skip);
    sync(&mut p);
    let r = stored(&p.dba, rid);
    let scope = EditScope::ThisAndFuture {
        key: keys[4].clone(),
    };
    let a = edit(&p.ea, &mut p.dba, rid, scope.clone(), title_patch(&r, "A"));
    let b = edit(&p.eb, &mut p.dbb, rid, scope, title_patch(&r, "B"));
    assert_eq!(a.entity, b.entity);
    sync(&mut p);
    assert_eq!(stored(&p.dba, a.entity), stored(&p.dbb, a.entity));
    let live = live_series(&p.dba, &[rid, a.entity]);
    assert_eq!(live.len(), keys.len());
    assert_eq!(live, live_series(&p.dbb, &[rid, a.entity]));
}

/// A device that has not said it reads `split_from` would key the
/// continuation's occurrences on the wrong id, so the split is refused.
#[test]
fn a_split_is_refused_while_a_paired_device_lacks_routine_series() {
    let mut p = pair(false);
    let (rid, keys) = daily(&p.ea, &mut p.dba, RoutineCatchupPolicy::Skip);
    sync(&mut p);
    let r = stored(&p.dba, rid);
    let err =
        p.ea.apply(
            &mut p.dba,
            Command::UpdateRoutine {
                id: rid,
                scope: EditScope::ThisAndFuture {
                    key: keys[2].clone(),
                },
                patch: title_patch(&r, "x"),
            },
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::FeatureUnsupportedByDevices { .. }),
        "{err:?}"
    );
    assert!(stored(&p.dba, rid).rrule.until.is_none());
}

// ---- edit scope: this ----

/// An edit of one occurrence is written onto its task only, as a user's
/// edit, so a later template edit of the series leaves it alone.
#[test]
fn an_edit_of_this_occurrence_touches_only_its_task() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let res = edit(
        &e,
        &mut db,
        rid,
        EditScope::This {
            key: keys[1].clone(),
        },
        title_patch(&r, "Just this once"),
    );
    let one = r.occurrence_task_id(&keys[1]);
    assert_eq!(res.entity, one);
    assert_eq!(task(&db, one).title, "Just this once");
    assert_eq!(
        task(&db, r.occurrence_task_id(&keys[2])).title,
        "Water plants"
    );
    assert_eq!(stored(&db, rid).template.title, "Water plants");

    edit(&e, &mut db, rid, EditScope::All, title_patch(&r, "Water"));
    assert_eq!(task(&db, one).title, "Just this once");
    assert_eq!(task(&db, r.occurrence_task_id(&keys[2])).title, "Water");
    e.assert_rebuild_identity(&db);
}

/// An edit of one occurrence moves its plan and keeps its key; one not
/// generated yet is generated first.
#[test]
fn an_edit_of_a_far_occurrence_generates_it_and_moves_its_plan() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let last = parse_occurrence_key(keys.last().unwrap()).unwrap();
    let far = last.checked_add(jiff::Span::new().days(30)).unwrap();
    let far_key = format!("{}T{:02}:{:02}", far.date(), far.hour(), far.minute());
    let moved = far.checked_add(jiff::Span::new().hours(2)).unwrap();
    let res = edit(
        &e,
        &mut db,
        rid,
        EditScope::This {
            key: far_key.clone(),
        },
        RoutinePatch {
            starts_at: Some(SunriseTime::zoned(moved, "UTC")),
            ..Default::default()
        },
    );
    assert_eq!(res.entity, r.occurrence_task_id(&far_key));
    let t = task(&db, res.entity);
    assert_eq!(t.planned_at, Some(SunriseTime::zoned(moved, "UTC")));
    assert_eq!(t.routine_id, Some(rid));
}

/// An edit of one occurrence carries only what one task can: a rule change
/// is refused.
#[test]
fn an_edit_of_this_occurrence_refuses_a_rule_change() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let err = e
        .apply(
            &mut db,
            Command::UpdateRoutine {
                id: rid,
                scope: EditScope::This {
                    key: keys[1].clone(),
                },
                patch: RoutinePatch {
                    rrule: Some(RRule::parse("FREQ=WEEKLY").unwrap()),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
}

// ---- template propagation ----

/// A template edit of the whole series reaches every untouched open
/// occurrence and leaves the edited, started and completed ones as they
/// are. A propagation is the routine engine's own write, so an occurrence it
/// reached is still untouched and a second edit reaches it again.
#[test]
fn a_template_edit_reaches_untouched_occurrences_and_leaves_edited_ones() {
    let e = engine();
    let mut db = db();
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let ids: Vec<EntityRef> = keys.iter().map(|k| r.occurrence_task_id(k)).collect();
    retitle_task(&e, &mut db, ids[1], "Mine");
    e.apply(&mut db, Command::CompleteTask(ids[2])).unwrap();

    let mut patch = title_patch(&r, "Water the garden");
    patch.template.as_mut().unwrap().priority = Some(2);
    edit(&e, &mut db, rid, EditScope::All, patch);

    let edited = task(&db, ids[1]);
    assert_eq!((edited.title.as_str(), edited.priority), ("Mine", None));
    let done = task(&db, ids[2]);
    assert_eq!((done.title.as_str(), done.priority), ("Water plants", None));
    for id in [ids[0], ids[3], ids[9]] {
        let t = task(&db, id);
        assert_eq!(
            (t.title.as_str(), t.priority),
            ("Water the garden", Some(2))
        );
    }

    let again = title_patch(&stored(&db, rid), "Water");
    edit(&e, &mut db, rid, EditScope::All, again);
    assert_eq!(task(&db, ids[3]).title, "Water");
    assert_eq!(task(&db, ids[1]).title, "Mine");
    e.assert_rebuild_identity(&db);
}

/// A propagation is generated: a concurrent user edit of the same
/// occurrence on another device beats it (ADR-0065 §3).
#[test]
fn a_concurrent_user_edit_beats_a_propagation() {
    let mut p = pair(true);
    let (rid, keys) = daily(&p.ea, &mut p.dba, RoutineCatchupPolicy::Skip);
    sync(&mut p);
    let r = stored(&p.dba, rid);
    let id = r.occurrence_task_id(&keys[3]);
    edit(
        &p.ea,
        &mut p.dba,
        rid,
        EditScope::All,
        title_patch(&r, "Template"),
    );
    retitle_task(&p.eb, &mut p.dbb, id, "User");
    sync(&mut p);
    assert_eq!(task(&p.dba, id).title, "User");
    assert_eq!(task(&p.dbb, id).title, "User");
}

// ---- catch-up over occurrences generated earlier ----

/// The routine's first five occurrences are past their grace window at
/// `late`, generated while they were still ahead and left open.
fn late() -> u64 {
    (NOW + 5 * DAY_MS + 2 * 3_600_000) as u64
}

fn today(e: &Engine, db: &Db) -> Vec<Task> {
    match e
        .query(
            db,
            Query::Today {
                now_ms: late(),
                contexts: Vec::new(),
            },
        )
        .unwrap()
    {
        QueryResult::Tasks(ts) => ts,
        other => panic!("{other:?}"),
    }
}

fn shown(tasks: &[Task], id: EntityRef) -> Option<&str> {
    tasks.iter().find(|t| t.id == id).map(|t| t.title.as_str())
}

fn lapsed(e: &Engine, db: &Db) -> Vec<EntityRef> {
    match e
        .query(db, Query::LapsedOccurrences { now_ms: late() })
        .unwrap()
    {
        QueryResult::Tasks(ts) => ts.into_iter().map(|t| t.id).collect(),
        other => panic!("{other:?}"),
    }
}

/// `skip`: every occurrence generated earlier and left open past its grace
/// window lapses, in Today and in the stream's list; an occurrence the user
/// edited is not open in this sense and stays.
#[test]
fn catch_up_skip_lapses_past_open_occurrences_generated_earlier() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT_331, [3u8; 32], clock.clone());
    let mut db = db_root(ROOT_331);
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Skip);
    let r = stored(&db, rid);
    let ids: Vec<EntityRef> = keys.iter().map(|k| r.occurrence_task_id(k)).collect();
    retitle_task(&e, &mut db, ids[1], "Kept");
    set_clock(&clock, late());

    let lapsed = lapsed(&e, &db);
    assert_eq!(lapsed, vec![ids[0], ids[2], ids[3], ids[4]]);
    let now = today(&e, &db);
    assert_eq!(shown(&now, ids[1]), Some("Kept"));
    assert_eq!(
        shown(&now, ids[5]),
        Some("Water plants"),
        "inside its grace window"
    );
    for id in &lapsed {
        assert_eq!(shown(&now, *id), None);
    }
    // Nor are they in the triage queue (ADR-0047 §3), though they are late.
    let QueryResult::Triage(rows) = e.query(&db, Query::Triage).unwrap() else {
        panic!("Triage returns rows");
    };
    for id in &lapsed {
        assert!(rows.iter().all(|r| r.task.id != *id), "{id} is lapsed");
    }
    let listed = titles(&e, &db, Query::StreamTasks(stream_ref(5)));
    assert_eq!(listed.len(), keys.len() - 4);
    // Nothing was written: the read derived it.
    assert!(!task(&db, ids[0]).deleted);
}

/// `merge`: the latest past open occurrence stands for all of them, titled
/// with how many; the others lapse.
#[test]
fn catch_up_merge_shows_the_latest_for_all_of_them() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT_331, [4u8; 32], clock.clone());
    let mut db = db_root(ROOT_331);
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Merge);
    let r = stored(&db, rid);
    let ids: Vec<EntityRef> = keys.iter().map(|k| r.occurrence_task_id(k)).collect();
    set_clock(&clock, late());

    assert_eq!(lapsed(&e, &db), ids[..4].to_vec());
    let now = today(&e, &db);
    assert_eq!(shown(&now, ids[4]), Some("Water plants (x5 catch-up)"));
    assert_eq!(shown(&now, ids[5]), Some("Water plants"));
    for id in &ids[..4] {
        assert_eq!(shown(&now, *id), None);
    }
    // The title is derived at read time; the stored one is the template's.
    assert_eq!(task(&db, ids[4]).title, "Water plants");
}

/// `queue`: every past open occurrence stays, as an ordinary task.
#[test]
fn catch_up_queue_keeps_every_past_open_occurrence() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT_331, [5u8; 32], clock.clone());
    let mut db = db_root(ROOT_331);
    let (rid, keys) = daily(&e, &mut db, RoutineCatchupPolicy::Queue);
    let r = stored(&db, rid);
    set_clock(&clock, late());
    assert!(lapsed(&e, &db).is_empty());
    let now = today(&e, &db);
    for k in &keys[..6] {
        assert_eq!(
            shown(&now, r.occurrence_task_id(k)),
            Some("Water plants"),
            "{k}"
        );
    }
}

// ---- the hard-constraint flag ----

fn window(from: i8, to: (i8, i8)) -> ScheduleConstraint {
    ScheduleConstraint {
        time_of_day: Some(sunrise_domain::TimeOfDayRange::new(
            jiff::civil::time(from, 0, 0, 0),
            jiff::civil::time(to.0, to.1, 0, 0),
        )),
        days_of_week: sunrise_domain::WeekdaySet::new(),
        date_range: None,
        severity: sunrise_domain::ConstraintSeverity::Hard,
        unknown: Unknowns::new(),
    }
}

/// An occurrence planned outside a `hard` window its routine carries is
/// generated, never dropped, and carries the persisted flag; a re-plan the
/// user makes inside the window clears it, and a constraint edit that the
/// occurrences keep clears it on the untouched ones.
#[test]
fn an_occurrence_breaking_a_hard_constraint_is_generated_and_flagged() {
    let e = engine();
    let mut db = db();
    // The occurrences are at 23:13 UTC; the window is the morning.
    let draft = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        vec![window(6, (8, 0))],
    );
    let rid = e
        .apply(&mut db, Command::CreateRoutine(draft))
        .unwrap()
        .entity;
    let r = stored(&db, rid);
    let occ = live_series(&db, &[rid]);
    assert_eq!(occ.len(), 14, "every occurrence is generated");
    assert!(occ.iter().all(|t| t.hard_constraint_violated));

    let first = occ[0].id;
    let civil = match &occ[0].planned_at {
        Some(SunriseTime::Zoned { civil, .. }) => *civil,
        other => panic!("{other:?}"),
    };
    let morning = civil.date().at(7, 0, 0, 0);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: first,
            patch: TaskPatch {
                planned_at: Some(Some(SunriseTime::zoned(morning, "UTC"))),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert!(!task(&db, first).hard_constraint_violated);

    edit(
        &e,
        &mut db,
        rid,
        EditScope::All,
        RoutinePatch {
            scheduling_constraints: Some(vec![window(22, (23, 59))]),
            ..Default::default()
        },
    );
    let untouched = live_series(&db, &[rid])
        .into_iter()
        .filter(|t| t.id != first)
        .collect::<Vec<_>>();
    assert!(!untouched.is_empty());
    assert!(untouched.iter().all(|t| !t.hard_constraint_violated));
    assert_eq!(r.scheduling_constraints, vec![window(6, (8, 0))]);
    e.assert_rebuild_identity(&db);
}

/// An occurrence inside every window is not flagged, and the flag never
/// reaches the wire when false.
#[test]
fn an_occurrence_within_its_constraints_is_not_flagged() {
    let e = engine();
    let mut db = db();
    let draft = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        vec![window(22, (23, 59))],
    );
    let rid = e
        .apply(&mut db, Command::CreateRoutine(draft))
        .unwrap()
        .entity;
    let occ = live_series(&db, &[rid]);
    assert!(occ.iter().all(|t| !t.hard_constraint_violated));
    let wire = sunrise_cbor::encode_canonical(&occ[0]).unwrap();
    let text = String::from_utf8_lossy(&wire);
    assert!(!text.contains("hard_constraint_violated"));
}
