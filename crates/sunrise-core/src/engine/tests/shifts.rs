//! Context shifts (issue #1244, ADR-0073): the entity, written only by
//! `Patch`, what crosses between replicas, the gate, and what a shift does to
//! a routine's generation, streak and the "While you were away" read.

use super::*;
use crate::feature::{ROUTINE_SHIFT_POLICY_FEATURE, SHIFT_FEATURE};
use std::collections::BTreeSet;
use sunrise_domain::{
    occurrence_task_id, AwaySummary, EditScope, RoutinePatch, Shift, ShiftDraft, ShiftPatch,
    ShiftPolicy,
};

const ROOT: [u8; 32] = [0x73; 32];

fn draft(name: &str, starts_ms: Option<i64>, ends_ms: Option<i64>) -> ShiftDraft {
    ShiftDraft {
        name: name.into(),
        starts_at: starts_ms.map(ms_to_ts),
        ends_at: ends_ms.map(ms_to_ts),
        zone: None,
        place: None,
        primary_streams: BTreeSet::new(),
        primary_contexts: BTreeSet::new(),
    }
}

fn start(e: &Engine, db: &mut Db, d: ShiftDraft) -> EntityRef {
    e.apply(db, Command::CreateShift(d)).unwrap().entity
}

fn shifts(e: &Engine, db: &Db) -> Vec<Shift> {
    match e.query(db, Query::Shifts).unwrap() {
        QueryResult::Shifts(s) => s,
        other => panic!("{other:?}"),
    }
}

fn active(e: &Engine, db: &Db) -> Option<Shift> {
    match e.query(db, Query::ActiveShift).unwrap() {
        QueryResult::ActiveShift(s) => s,
        other => panic!("{other:?}"),
    }
}

fn away(e: &Engine, db: &Db, id: EntityRef) -> AwaySummary {
    match e.query(db, Query::AwaySummary(id)).unwrap() {
        QueryResult::AwaySummary(s) => s,
        other => panic!("{other:?}"),
    }
}

fn required(db: &Db) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT feature FROM vault_required_features")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A daily routine at `NOW + 1h`, in UTC, and its id.
fn daily(e: &Engine, db: &mut Db) -> EntityRef {
    let d = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Queue,
        Vec::new(),
    );
    e.apply(db, Command::CreateRoutine(d)).unwrap().entity
}

fn set_policy(e: &Engine, db: &mut Db, rid: EntityRef, policy: ShiftPolicy) {
    e.apply(
        db,
        Command::UpdateRoutine {
            id: rid,
            scope: EditScope::All,
            patch: RoutinePatch {
                shift_policy: Some(policy),
                ..RoutinePatch::default()
            },
        },
    )
    .unwrap();
}

/// Whether the task of occurrence `day` (0 is the anchor's) of `rid` is live.
fn generated(e: &Engine, db: &Db, rid: EntityRef, day: i64) -> Option<Task> {
    let at = ms_to_ts(NOW + 3_600_000 + day * DAY_MS);
    let key = sunrise_domain::occurrence_key_at("UTC", at).unwrap();
    let id = occurrence_task_id(&rid, &key);
    match e.query(db, Query::EntityById(id)) {
        Ok(QueryResult::Task(t)) if !t.deleted => Some(*t),
        _ => None,
    }
}

#[test]
fn a_shift_is_started_listed_edited_ended_deleted_and_restored() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let id = start(&e, &mut db, draft("  Travel: Tokyo  ", None, None));
    assert_eq!(id.kind(), EntityKind::Shift);
    let s = shifts(&e, &db).remove(0);
    assert_eq!(s.name, "Travel: Tokyo", "the name is trimmed");
    assert_eq!(s.starts_at, ms_to_ts(NOW), "it starts now");
    assert_eq!(s.ends_at, None);
    assert_eq!(active(&e, &db).map(|a| a.id), Some(id));

    e.apply(
        &mut db,
        Command::UpdateShift {
            id,
            patch: ShiftPatch {
                zone: Some(Some("Asia/Tokyo".into())),
                ..ShiftPatch::default()
            },
        },
    )
    .unwrap();
    let s = shifts(&e, &db).remove(0);
    assert_eq!(s.zone.as_deref(), Some("Asia/Tokyo"));
    assert_eq!(s.name, "Travel: Tokyo", "an edit writes only what it names");

    set_clock(&clock, T0 + 3_600_000);
    e.apply(&mut db, Command::EndShift { id, at: None })
        .unwrap();
    let s = shifts(&e, &db).remove(0);
    assert_eq!(s.ends_at, Some(ms_to_ts(NOW + 3_600_000)), "it ends now");
    assert_eq!(active(&e, &db), None, "an ended shift is not in force");

    e.apply(&mut db, Command::DeleteShift(id)).unwrap();
    assert!(shifts(&e, &db).is_empty());
    match e.query(&db, Query::EntityById(id)).unwrap() {
        QueryResult::Shifts(s) => assert!(s[0].deleted, "found by id, deleted"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        e.apply(&mut db, Command::EndShift { id, at: None }),
        Err(EngineError::NotFound(_))
    ));
    e.apply(&mut db, Command::Restore(id)).unwrap();
    assert_eq!(shifts(&e, &db).len(), 1);
    e.assert_rebuild_identity(&db);
}

#[test]
fn a_shift_that_does_not_validate_and_an_empty_edit_write_nothing() {
    let mut db = db();
    let e = engine();
    let id = start(&e, &mut db, draft("Trip", Some(NOW), None));
    let ops = op_count(&db);
    for bad in [
        draft("  ", None, None),
        draft("Trip", Some(NOW), Some(NOW)),
        ShiftDraft {
            zone: Some("Mars/Olympus".into()),
            ..draft("Trip", None, None)
        },
        ShiftDraft {
            place: Some(stream_ref(1)),
            ..draft("Trip", None, None)
        },
    ] {
        assert!(
            matches!(
                e.apply(&mut db, Command::CreateShift(bad.clone())),
                Err(EngineError::Validation(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdateShift {
                id,
                patch: ShiftPatch::default(),
            },
        ),
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        e.apply(
            &mut db,
            Command::EndShift {
                id,
                at: Some(ms_to_ts(NOW - 1)),
            },
        ),
        Err(EngineError::Validation(_))
    ));
    assert_eq!(op_count(&db), ops, "nothing refused was written");
}

/// Every write is a `Patch` on the vault-meta stream naming what it changes,
/// and the vault requires `shift.entity` from the first; ending one writes
/// `ends_at` alone.
#[test]
fn writes_are_patches_and_require_the_feature() {
    let mut db = db();
    let e = engine();
    let id = start(&e, &mut db, draft("Trip", Some(NOW - 1_000), None));
    assert!(required(&db).iter().any(|f| f == SHIFT_FEATURE));
    e.apply(&mut db, Command::EndShift { id, at: None })
        .unwrap();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE inner_kind = 'shift.patch' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(envs.len(), 2);
    let patch = |env: &[u8]| match decode_inner_op(&e.keychain.open_op(env).unwrap()).unwrap() {
        InnerOp::Patch(p) => *p,
        other => panic!("{other:?}"),
    };
    let first = patch(&envs[0]);
    assert!(first.create);
    let mut keys: Vec<&String> = first.fields.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "deleted",
            "name",
            "primary_contexts",
            "primary_streams",
            "starts_at"
        ]
    );
    let end = patch(&envs[1]);
    assert_eq!(end.fields.keys().collect::<Vec<_>>(), ["ends_at"]);
}

/// A paired device that has not advertised `shift.entity` cannot decode a
/// `Patch` naming `shf_`, so none is written; nor is a routine's policy
/// other than `pause` while one lacks `routine.shift_policy`.
#[test]
fn a_shift_and_a_policy_wait_for_every_device_to_support_them() {
    let mut db = db();
    let e = engine();
    let rid = daily(&e, &mut db);
    legacy_peer(&db);
    let ops = op_count(&db);
    let err = e
        .apply(&mut db, Command::CreateShift(draft("Trip", None, None)))
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::FeatureUnsupportedByDevices { feature, .. }
            if feature == SHIFT_FEATURE),
        "{err:?}"
    );
    let err = e
        .apply(
            &mut db,
            Command::UpdateRoutine {
                id: rid,
                scope: EditScope::All,
                patch: RoutinePatch {
                    shift_policy: Some(ShiftPolicy::Skip),
                    ..RoutinePatch::default()
                },
            },
        )
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::FeatureUnsupportedByDevices { feature, .. }
            if feature == ROUTINE_SHIFT_POLICY_FEATURE),
        "{err:?}"
    );
    assert_eq!(op_count(&db), ops, "the refused writes wrote nothing");
    assert!(shifts(&e, &db).is_empty());
}

/// A routine's policy is written once the vault requires the feature, reads
/// back, and an unknown one is refused before anything is written.
#[test]
fn a_routines_policy_is_set_and_read_back() {
    let mut db = db();
    let e = engine();
    let rid = daily(&e, &mut db);
    assert_eq!(routine_of(&e, &db, rid).shift_policy, ShiftPolicy::Pause);
    assert!(!required(&db)
        .iter()
        .any(|f| f == ROUTINE_SHIFT_POLICY_FEATURE));
    set_policy(&e, &mut db, rid, ShiftPolicy::ShiftTz);
    assert_eq!(routine_of(&e, &db, rid).shift_policy, ShiftPolicy::ShiftTz);
    assert!(required(&db)
        .iter()
        .any(|f| f == ROUTINE_SHIFT_POLICY_FEATURE));
    let ops = op_count(&db);
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdateRoutine {
                id: rid,
                scope: EditScope::All,
                patch: RoutinePatch {
                    shift_policy: Some(ShiftPolicy::from_raw("follow_place")),
                    ..RoutinePatch::default()
                },
            },
        ),
        Err(EngineError::Validation(_))
    ));
    assert_eq!(op_count(&db), ops);
    e.assert_rebuild_identity(&db);
}

/// Two devices that start shifts apart, at the same instant, converge on the
/// same shift in force once each holds both (ADR-0073 §1).
#[test]
fn concurrent_shifts_on_two_replicas_converge_on_one_active_shift() {
    let mut p = synced_pair(ROOT, true);
    let a = start(&p.ea, &mut p.dba, draft("Conference", Some(NOW), None));
    let b = start(&p.eb, &mut p.dbb, draft("Trip", Some(NOW), None));
    assert_eq!(active(&p.ea, &p.dba).map(|s| s.id), Some(a));
    assert_eq!(active(&p.eb, &p.dbb).map(|s| s.id), Some(b));
    sync(&mut p);
    let on_a = active(&p.ea, &p.dba).expect("one in force");
    let on_b = active(&p.eb, &p.dbb).expect("one in force");
    assert_eq!(on_a, on_b, "the same shift on both");
    assert_eq!(on_a.id, a.max(b), "a tie in start goes to the greater id");
    assert_eq!(shifts(&p.ea, &p.dba), shifts(&p.eb, &p.dbb));
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// Ending a shift on one device while the other renames it keeps both.
#[test]
fn an_end_and_a_rename_made_apart_both_survive() {
    let mut p = synced_pair(ROOT, true);
    let id = start(&p.ea, &mut p.dba, draft("Trip", Some(NOW), None));
    sync(&mut p);
    set_clock(&p.ca, T0 + 1_000);
    set_clock(&p.cb, T0 + 1_000);
    p.ea.apply(&mut p.dba, Command::EndShift { id, at: None })
        .unwrap();
    p.eb.apply(
        &mut p.dbb,
        Command::UpdateShift {
            id,
            patch: ShiftPatch {
                name: Some("Tokyo".into()),
                ..ShiftPatch::default()
            },
        },
    )
    .unwrap();
    sync(&mut p);
    let a = shifts(&p.ea, &p.dba);
    assert_eq!(a, shifts(&p.eb, &p.dbb), "one shift on both");
    assert_eq!(a[0].name, "Tokyo");
    assert_eq!(a[0].ends_at, Some(ms_to_ts(NOW + 1_000)));
}

/// Under `pause`, an occurrence a shift covers is not generated and never
/// becomes a catch-up; ending the shift early gives back the ones after the
/// new end (ADR-0073 §4).
#[test]
fn pause_holds_back_what_the_shift_covers_until_it_ends() {
    let mut db = db();
    let e = engine();
    start(
        &e,
        &mut db,
        draft("Trip", Some(NOW + DAY_MS), Some(NOW + 4 * DAY_MS)),
    );
    let rid = daily(&e, &mut db);
    assert!(generated(&e, &db, rid, 0).is_some());
    for day in 1..=3 {
        assert!(generated(&e, &db, rid, day).is_none(), "day {day} is held");
    }
    assert!(generated(&e, &db, rid, 4).is_some());

    // The shift ends early: days 2 and 3 come back, day 1 stays held.
    let id = shifts(&e, &db)[0].id;
    e.apply(
        &mut db,
        Command::EndShift {
            id,
            at: Some(ms_to_ts(NOW + 2 * DAY_MS)),
        },
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::MaterializeRoutines {
            now_ms: NOW.cast_unsigned(),
        },
    )
    .unwrap();
    assert!(generated(&e, &db, rid, 1).is_none());
    assert!(generated(&e, &db, rid, 2).is_some());
    assert!(generated(&e, &db, rid, 3).is_some());

    // Past the held day, with `queue` catch-up: it is never queued.
    let later = NOW + 6 * DAY_MS;
    e.apply(
        &mut db,
        Command::MaterializeRoutines {
            now_ms: later.cast_unsigned(),
        },
    )
    .unwrap();
    assert!(generated(&e, &db, rid, 1).is_none(), "never a catch-up");
}

/// `skip` generates nothing the shift covers, as `pause` does, and
/// `unchanged` generates everything.
#[test]
fn skip_and_unchanged_policies() {
    for (policy, held) in [(ShiftPolicy::Skip, true), (ShiftPolicy::Unchanged, false)] {
        let mut db = db();
        let e = engine();
        start(
            &e,
            &mut db,
            draft("Trip", Some(NOW + DAY_MS), Some(NOW + 2 * DAY_MS)),
        );
        let rid = daily(&e, &mut db);
        // The create materialized under `pause`; the policy changes, and the
        // held day is generated again or not.
        set_policy(&e, &mut db, rid, policy.clone());
        e.apply(
            &mut db,
            Command::MaterializeRoutines {
                now_ms: NOW.cast_unsigned(),
            },
        )
        .unwrap();
        assert_eq!(
            generated(&e, &db, rid, 1).is_none(),
            held,
            "{policy:?} on the covered day"
        );
        assert!(generated(&e, &db, rid, 2).is_some(), "{policy:?}");
    }
}

/// `shift_tz` plans each occurrence the shift covers at its wall clock in
/// the shift's zone, with the key, and so the task id, unchanged.
#[test]
fn shift_tz_plans_the_occurrence_in_the_shifts_zone() {
    let mut db = db();
    let e = engine();
    start(
        &e,
        &mut db,
        ShiftDraft {
            zone: Some("Asia/Tokyo".into()),
            ..draft("Tokyo", Some(NOW + DAY_MS / 2), Some(NOW + 3 * DAY_MS))
        },
    );
    let rid = daily(&e, &mut db);
    set_policy(&e, &mut db, rid, ShiftPolicy::ShiftTz);
    e.apply(
        &mut db,
        Command::MaterializeRoutines {
            now_ms: NOW.cast_unsigned(),
        },
    )
    .unwrap();
    let t = generated(&e, &db, rid, 2).expect("the covered day is generated");
    assert!(
        matches!(&t.planned_at, Some(SunriseTime::Zoned { tz, .. }) if tz == "Asia/Tokyo"),
        "{:?}",
        t.planned_at
    );
    let t = generated(&e, &db, rid, 5).expect("after the shift");
    assert!(
        matches!(&t.planned_at, Some(SunriseTime::Zoned { tz, .. }) if tz == "UTC"),
        "{:?}",
        t.planned_at
    );
}

/// "While you were away": an `unchanged` routine missed what it generated
/// during the shift, a `pause` one missed nothing, and a task made during
/// the shift is a capture.
#[test]
fn the_away_summary_lists_misses_and_captures() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let paused = daily(&e, &mut db);
    let kept = daily(&e, &mut db);
    set_policy(&e, &mut db, kept, ShiftPolicy::Unchanged);
    let id = start(&e, &mut db, draft("Trip", Some(NOW), None));
    set_clock(&clock, (NOW + 2 * DAY_MS).cast_unsigned());
    let captured = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "souvenirs".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    set_clock(&clock, (NOW + 2 * DAY_MS + 3_600_000).cast_unsigned());
    let s = away(&e, &db, id);
    assert!(s.missed.iter().all(|m| m.routine == kept), "{s:?}");
    assert!(!s.missed.iter().any(|m| m.routine == paused));
    assert_eq!(s.missed.len(), 2, "days 0 and 1 of the kept routine: {s:?}");
    assert_eq!(s.captured, vec![captured]);
}

/// The streak a read shows excuses what a `skip` shift held back: a trip
/// does not end it.
#[test]
fn a_skip_shift_keeps_the_streak() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let rid = daily(&e, &mut db);
    set_policy(&e, &mut db, rid, ShiftPolicy::Skip);
    let day0 = generated(&e, &db, rid, 0).unwrap().id;
    set_clock(&clock, (NOW + 2 * 3_600_000).cast_unsigned());
    e.apply(&mut db, Command::CompleteTask(day0)).unwrap();
    assert_eq!(routine_of(&e, &db, rid).streak_counter, 1);
    // Away for days 1..=5, none of them done.
    start(
        &e,
        &mut db,
        draft("Trip", Some(NOW + DAY_MS), Some(NOW + 6 * DAY_MS)),
    );
    set_clock(&clock, (NOW + 6 * DAY_MS).cast_unsigned());
    let r = routine_of(&e, &db, rid);
    assert_eq!(r.streak_counter, 1, "the trip excused every day it covered");
    assert_eq!(r.forgivenesses_in_window, 0, "and spent no forgiveness");
}
