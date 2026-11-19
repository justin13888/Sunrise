//! Places (issue #1128, ADR-0051): the entity, written only by `Patch`,
//! what crosses between replicas, the gate, and the place requirement a
//! task's constraints state.

use super::*;
use crate::feature::PLACE_FEATURE;
use sunrise_domain::{
    ConstraintSeverity, Place, PlaceDraft, PlacePatch, ScheduleConstraint, TaskPlaceReq, Unknowns,
    WeekdaySet,
};

const ROOT: [u8; 32] = [0x51; 32];

fn pair() -> Pair {
    synced_pair(ROOT, true)
}

fn draft(name: &str) -> PlaceDraft {
    PlaceDraft {
        name: name.into(),
        lat_e7: 437_000_000,
        lon_e7: -794_000_000,
        radius_m: 150,
        address_label: Some("1 Front St".into()),
    }
}

fn create(e: &Engine, db: &mut Db, name: &str) -> EntityRef {
    e.apply(db, Command::CreatePlace(draft(name)))
        .unwrap()
        .entity
}

fn places(e: &Engine, db: &Db) -> Vec<Place> {
    match e.query(db, Query::Places).unwrap() {
        QueryResult::Places(p) => p,
        other => panic!("{other:?}"),
    }
}

fn update(e: &Engine, db: &mut Db, id: EntityRef, patch: PlacePatch) {
    e.apply(db, Command::UpdatePlace { id, patch }).unwrap();
}

fn at_place(refs: Vec<EntityRef>, severity: ConstraintSeverity) -> ScheduleConstraint {
    ScheduleConstraint {
        time_of_day: None,
        days_of_week: WeekdaySet::new(),
        date_range: None,
        at_place: refs,
        severity,
        unknown: Unknowns::new(),
    }
}

fn task_at(e: &Engine, db: &mut Db, title: &str, c: ScheduleConstraint) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            scheduling_constraints: vec![c],
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

#[test]
fn a_place_is_created_listed_edited_deleted_and_restored() {
    let mut db = db();
    let e = engine();
    let id = create(&e, &mut db, "  Gym  ");
    assert_eq!(id.kind(), EntityKind::Place);
    let listed = places(&e, &db);
    assert_eq!(listed.len(), 1);
    let p = &listed[0];
    assert_eq!(
        (p.name.as_str(), p.lat_e7, p.lon_e7, p.radius_m),
        ("Gym", 437_000_000, -794_000_000, 150),
        "the name is trimmed"
    );
    assert_eq!(p.address_label.as_deref(), Some("1 Front St"));

    update(
        &e,
        &mut db,
        id,
        PlacePatch {
            radius_m: Some(300),
            address_label: Some(None),
            ..PlacePatch::default()
        },
    );
    let p = places(&e, &db).remove(0);
    assert_eq!((p.radius_m, p.address_label), (300, None));
    assert_eq!(p.name, "Gym", "an edit writes only what it names");

    e.apply(&mut db, Command::DeletePlace(id)).unwrap();
    assert!(places(&e, &db).is_empty());
    match e.query(&db, Query::EntityById(id)).unwrap() {
        QueryResult::Places(p) => assert!(p[0].deleted, "found by id, deleted"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdatePlace {
                id,
                patch: PlacePatch {
                    name: Some("x".into()),
                    ..PlacePatch::default()
                },
            },
        ),
        Err(EngineError::NotFound(_))
    ));
    e.apply(&mut db, Command::Restore(id)).unwrap();
    assert_eq!(places(&e, &db).len(), 1);
    e.assert_rebuild_identity(&db);
}

#[test]
fn a_place_that_does_not_validate_and_an_empty_edit_write_nothing() {
    let mut db = db();
    let e = engine();
    let id = create(&e, &mut db, "Gym");
    let ops = op_count(&db);
    for bad in [
        PlaceDraft {
            name: "  ".into(),
            ..draft("x")
        },
        PlaceDraft {
            lat_e7: 900_000_001,
            ..draft("x")
        },
        PlaceDraft {
            radius_m: 49,
            ..draft("x")
        },
    ] {
        assert!(
            matches!(
                e.apply(&mut db, Command::CreatePlace(bad.clone())),
                Err(EngineError::Validation(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdatePlace {
                id,
                patch: PlacePatch {
                    radius_m: Some(5001),
                    ..PlacePatch::default()
                },
            },
        ),
        Err(EngineError::Validation(_))
    ));
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdatePlace {
                id,
                patch: PlacePatch::default(),
            },
        ),
        Err(EngineError::Invalid(_))
    ));
    let task = EntityRef::new(EntityKind::Task, [9; 16]);
    assert!(matches!(
        e.apply(&mut db, Command::DeletePlace(task)),
        Err(EngineError::Invalid(_) | EngineError::NotFound(_))
    ));
    assert_eq!(op_count(&db), ops, "nothing refused was written");
}

/// Every write is a `Patch` on the vault-meta stream, and the vault requires
/// `place.entity` from the first.
#[test]
fn writes_are_patches_and_require_the_feature() {
    let mut db = db();
    let e = engine();
    let id = create(&e, &mut db, "Gym");
    let required: Vec<String> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT feature FROM vault_required_features")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert!(required.iter().any(|f| f == PLACE_FEATURE), "{required:?}");
    update(
        &e,
        &mut db,
        id,
        PlacePatch {
            name: Some("Pool".into()),
            ..PlacePatch::default()
        },
    );
    let envs: Vec<Vec<u8>> = {
        let mut stmt = db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE inner_kind = 'place.patch' ORDER BY seq")
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
            "address_label",
            "deleted",
            "lat_e7",
            "lon_e7",
            "name",
            "radius_m"
        ]
    );
    let edit = patch(&envs[1]);
    assert_eq!(edit.fields.keys().collect::<Vec<_>>(), ["name"]);
    assert_eq!(edit.parents.map(|p| p.len()), Some(1), "names the create");
}

/// A paired device that has not advertised `place.entity` cannot decode a
/// `Patch` naming `plc_`, so none is written.
#[test]
fn a_place_waits_for_every_device_to_support_it() {
    let mut db = db();
    let e = engine();
    legacy_peer(&db);
    let ops = op_count(&db);
    let err = e
        .apply(&mut db, Command::CreatePlace(draft("Gym")))
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::FeatureUnsupportedByDevices { feature, .. }
            if feature == PLACE_FEATURE),
        "{err:?}"
    );
    assert_eq!(op_count(&db), ops, "the refused create wrote nothing");
    assert!(places(&e, &db).is_empty());
}

/// A place made on one device is on the other, and an edit of its name on
/// one and of its radius on the other, made apart, both survive.
#[test]
fn a_place_syncs_and_concurrent_edits_of_different_fields_both_survive() {
    let mut p = pair();
    let id = create(&p.ea, &mut p.dba, "Gym");
    sync(&mut p);
    assert_eq!(places(&p.eb, &p.dbb), places(&p.ea, &p.dba));
    *p.ca.0.lock() = T0 + 1_000;
    *p.cb.0.lock() = T0 + 1_000;
    update(
        &p.ea,
        &mut p.dba,
        id,
        PlacePatch {
            name: Some("Pool".into()),
            ..PlacePatch::default()
        },
    );
    update(
        &p.eb,
        &mut p.dbb,
        id,
        PlacePatch {
            radius_m: Some(500),
            ..PlacePatch::default()
        },
    );
    sync(&mut p);
    let a = places(&p.ea, &p.dba);
    assert_eq!(a, places(&p.eb, &p.dbb), "one place on both");
    assert_eq!((a[0].name.as_str(), a[0].radius_m), ("Pool", 500));
    p.ea.apply(&mut p.dba, Command::DeletePlace(id)).unwrap();
    sync(&mut p);
    assert!(places(&p.eb, &p.dbb).is_empty(), "the tombstone crosses");
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// A place requirement never rejects a write, at any severity, and a
/// deleted place stays named on the task that names it.
#[test]
fn a_place_requirement_never_rejects_and_survives_the_places_deletion() {
    let mut db = db();
    let e = engine();
    let gym = create(&e, &mut db, "Gym");
    let id = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "lift".into(),
                planned_at: Some(SunriseTime::Instant {
                    at: jiff::Timestamp::from_millisecond(i64::try_from(T0).unwrap()).unwrap(),
                }),
                scheduling_constraints: vec![at_place(vec![gym], ConstraintSeverity::Hard)],
                ..Default::default()
            }),
        )
        .expect("a hard place requirement is not a time window")
        .entity;
    e.apply(&mut db, Command::DeletePlace(gym)).unwrap();
    let t = match e.query(&db, Query::EntityById(id)).unwrap() {
        QueryResult::Task(t) => *t,
        other => panic!("{other:?}"),
    };
    assert_eq!(t.scheduling_constraints[0].at_place, vec![gym]);
    // A ref that is not a place is refused by validation, like any other
    // malformed constraint.
    let task = EntityRef::new(EntityKind::Task, [3; 16]);
    assert!(matches!(
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "bad".into(),
                scheduling_constraints: vec![at_place(vec![task], ConstraintSeverity::Soft)],
                ..Default::default()
            }),
        ),
        Err(EngineError::Validation(_))
    ));
}

/// The requirements `Core` evaluates are every open task's that names a
/// place, and no other.
#[test]
fn place_requirements_are_every_open_task_naming_a_place() {
    let mut db = db();
    let e = engine();
    let gym = create(&e, &mut db, "Gym");
    let home = create(&e, &mut db, "Home");
    let lift = task_at(
        &e,
        &mut db,
        "lift",
        at_place(vec![gym], ConstraintSeverity::Hard),
    );
    let done = task_at(
        &e,
        &mut db,
        "done",
        at_place(vec![home], ConstraintSeverity::Soft),
    );
    e.apply(&mut db, Command::CompleteTask(done)).unwrap();
    task_at(&e, &mut db, "anywhere", sample_constraint());
    let reqs = e.place_requirements(&db).unwrap();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].0.id, lift);
    assert_eq!(
        reqs[0].1,
        TaskPlaceReq {
            places: vec![sunrise_domain::PlaceId::from_ref(gym).unwrap()],
            hard: true,
        }
    );
    assert_eq!(e.live_places(&db).unwrap().len(), 2);
}
