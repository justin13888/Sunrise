//! Presence at a Place is held in memory and written nowhere (ADR-0051 §3).

use super::*;
use sunrise_domain::{PlaceEval, PlaceId};
use sunrise_id::Ulid;

fn place(n: u8) -> PlaceId {
    let mut bytes = [0u8; 16];
    bytes[15] = n;
    PlaceId::from_ulid(Ulid::from_bytes(bytes))
}

/// The row count of every table in the vault database, by name: the op log,
/// the outbox and every projection and device-local table alike.
fn every_table(core: &Core) -> Vec<(String, i64)> {
    let db = core.db.lock();
    let conn = db.conn();
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names.retain(|n| !n.starts_with("sqlite_"));
    names
        .into_iter()
        .map(|n| {
            let count = conn
                .query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))
                .unwrap();
            (n, count)
        })
        .collect()
}

#[tokio::test]
async fn presence_is_never_written_to_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let mut events = core.changes();
    let before = every_table(&core);

    core.on_place_presence(place(1), true);
    core.on_place_presence(place(2), false);
    core.on_place_presence(place(1), false);
    core.on_place_presence(place(1), true);
    core.forget_place_presence(place(2));

    assert_eq!(
        every_table(&core),
        before,
        "a presence report must add no op, no outbox entry and no row anywhere"
    );
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "a presence report is not a change to the vault"
    );
    assert_eq!(core.evaluate_place(&[place(1)]), PlaceEval::Satisfied);
    assert_eq!(
        core.evaluate_place(&[place(2)]),
        PlaceEval::Unknown,
        "a forgotten place is unknown again"
    );
    core.close().await.unwrap();
}

#[tokio::test]
async fn a_reopened_vault_knows_no_presence() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    core.on_place_presence(place(1), true);
    core.on_place_presence(place(2), false);
    assert_eq!(
        core.evaluate_place(&[place(2), place(1)]),
        PlaceEval::Satisfied
    );
    assert_eq!(core.evaluate_place(&[place(2)]), PlaceEval::Violated);
    core.close().await.unwrap();

    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    for p in [place(1), place(2)] {
        assert_eq!(
            core.evaluate_place(&[p]),
            PlaceEval::Unknown,
            "with no OS report since open, every place is unknown"
        );
    }
    core.close().await.unwrap();
}

#[tokio::test]
async fn revoking_permission_or_closing_leaves_every_place_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    core.on_place_presence(place(1), false);
    assert_eq!(core.evaluate_place(&[place(1)]), PlaceEval::Violated);
    core.clear_place_presence();
    assert_eq!(
        core.evaluate_place(&[place(1)]),
        PlaceEval::Unknown,
        "a denied permission is unknown, never a violation"
    );

    core.on_place_presence(place(1), true);
    core.shutdown().await;
    assert_eq!(core.evaluate_place(&[place(1)]), PlaceEval::Unknown);
    core.on_place_presence(place(1), true);
    assert_eq!(
        core.evaluate_place(&[place(1)]),
        PlaceEval::Unknown,
        "a report after close is dropped"
    );
}

/// Every value of every row of every table, as text: what a presence report
/// must leave exactly as it found it.
fn every_row(core: &Core) -> Vec<(String, Vec<String>)> {
    let names: Vec<String> = every_table(core).into_iter().map(|(n, _)| n).collect();
    let db = core.db.lock();
    let conn = db.conn();
    names
        .into_iter()
        .map(|n| {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{n}\"")).unwrap();
            let width = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    Ok((0..width)
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect::<Vec<_>>()
                        .join("|"))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            (n, rows)
        })
        .collect()
}

async fn new_place(core: &Core, name: &str, lat_e7: i32) -> sunrise_id::EntityRef {
    core.submit(Command::CreatePlace(sunrise_domain::PlaceDraft {
        name: name.into(),
        lat_e7,
        lon_e7: 0,
        radius_m: 150,
        address_label: None,
    }))
    .await
    .unwrap()
    .entity
}

async fn task_at(
    core: &Core,
    title: &str,
    places: Vec<sunrise_id::EntityRef>,
    severity: sunrise_domain::ConstraintSeverity,
) -> sunrise_id::EntityRef {
    core.submit(Command::CreateTask(sunrise_domain::TaskDraft {
        title: title.into(),
        scheduling_constraints: vec![sunrise_domain::ScheduleConstraint {
            time_of_day: None,
            days_of_week: sunrise_domain::WeekdaySet::new(),
            date_range: None,
            at_place: places,
            severity,
            unknown: sunrise_domain::Unknowns::new(),
        }],
        ..Default::default()
    }))
    .await
    .unwrap()
    .entity
}

fn pid(r: sunrise_id::EntityRef) -> PlaceId {
    PlaceId::from_ref(r).unwrap()
}

/// The headline of ADR-0051 §3, once Place ops exist: with places and tasks
/// requiring them in the vault, presence reports and every read of them
/// change no op, no envelope and no row, and the Place record has no field
/// that could carry one.
#[tokio::test]
async fn no_op_envelope_or_row_carries_presence_once_places_exist() {
    use sunrise_domain::ConstraintSeverity::Hard;
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let gym = new_place(&core, "Gym", 1).await;
    task_at(&core, "lift", vec![gym], Hard).await;
    let before = every_row(&core);

    core.on_place_presence(pid(gym), true);
    let _ = core.here_now().unwrap();
    let _ = core.place_requirements().unwrap();
    let _ = core.select_regions(Some((10, 10)), 20).unwrap();
    core.on_place_presence(pid(gym), false);
    let _ = core.here_now().unwrap();
    assert_eq!(
        every_row(&core),
        before,
        "presence and the reads of it wrote nothing anywhere"
    );

    // The record's fields are the user's entry and the merge's bookkeeping,
    // and nothing else.
    let spec = sunrise_id::registry::ENTITIES
        .iter()
        .find(|e| e.kind == sunrise_id::EntityKind::Place)
        .unwrap();
    let fields: Vec<&str> = spec.records[0].fields.iter().map(|f| f.name).collect();
    assert_eq!(
        fields,
        [
            "id",
            "created_at",
            "updated_at",
            "name",
            "lat_e7",
            "lon_e7",
            "radius_m",
            "address_label",
            "deleted"
        ]
    );
    // A write after a report carries the field it names and no other.
    core.on_place_presence(pid(gym), true);
    let n = every_table(&core)
        .into_iter()
        .find(|(t, _)| t == "ops")
        .unwrap()
        .1;
    core.submit(Command::UpdatePlace {
        id: gym,
        patch: sunrise_domain::PlacePatch {
            name: Some("Pool".into()),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let after = every_table(&core)
        .into_iter()
        .find(|(t, _)| t == "ops")
        .unwrap()
        .1;
    assert_eq!(after, n + 1, "one op, the edit");
    core.close().await.unwrap();
}

/// "Here now" is every open task whose requirement is satisfied; "not
/// actionable here" is a hard requirement known violated; unknown is
/// neither.
#[tokio::test]
async fn here_now_and_not_actionable_here_follow_presence() {
    use sunrise_domain::ConstraintSeverity::{Hard, Soft};
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let gym = new_place(&core, "Gym", 1).await;
    let home = new_place(&core, "Home", 2).await;
    let lift = task_at(&core, "lift", vec![gym], Hard).await;
    let nap = task_at(&core, "nap", vec![home], Soft).await;

    let status = |core: &Core, task| {
        core.place_requirements()
            .unwrap()
            .into_iter()
            .find(|s| s.task == task)
            .unwrap()
    };
    assert!(core.here_now().unwrap().is_empty(), "nothing known");
    assert_eq!(status(&core, lift).eval, PlaceEval::Unknown);
    assert!(!status(&core, lift).not_actionable_here);

    core.on_place_presence(pid(gym), false);
    core.on_place_presence(pid(home), true);
    let here: Vec<_> = core.here_now().unwrap().into_iter().map(|t| t.id).collect();
    assert_eq!(here, vec![nap]);
    let s = status(&core, lift);
    assert_eq!(
        (s.eval, s.hard, s.not_actionable_here),
        (PlaceEval::Violated, true, true)
    );
    core.on_place_presence(pid(home), false);
    assert!(
        !status(&core, nap).not_actionable_here,
        "a soft requirement is never flagged"
    );

    core.clear_place_presence();
    assert!(core
        .place_requirements()
        .unwrap()
        .iter()
        .all(|s| s.eval == PlaceEval::Unknown));
    core.close().await.unwrap();
}

/// Deleting a place forgets what the OS said about it, here and for a place
/// deleted on another replica alike: the task naming it reads unknown.
#[tokio::test]
async fn a_deleted_place_is_unknown_and_its_presence_forgotten() {
    use sunrise_domain::ConstraintSeverity::Hard;
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let gym = new_place(&core, "Gym", 1).await;
    let lift = task_at(&core, "lift", vec![gym], Hard).await;
    core.on_place_presence(pid(gym), false);
    assert!(core.place_requirements().unwrap()[0].not_actionable_here);

    core.submit(Command::DeletePlace(gym)).await.unwrap();
    assert_eq!(
        core.evaluate_place(&[pid(gym)]),
        PlaceEval::Unknown,
        "the delete forgot it"
    );
    // A report that races the delete still reads unknown: the place is not
    // live.
    core.on_place_presence(pid(gym), false);
    let s = core.place_requirements().unwrap();
    assert_eq!((s[0].task, s[0].eval), (lift, PlaceEval::Unknown));
    assert!(
        !s[0].not_actionable_here,
        "a deleted place never flags a task"
    );
    assert!(core.select_regions(None, 20).unwrap().is_empty());
    core.close().await.unwrap();
}

/// The region selection reads the vault's places and open tasks, ranks hard
/// demand first and leaves one slot for the client's trigger.
#[tokio::test]
async fn select_regions_ranks_the_vaults_places_by_open_demand() {
    use sunrise_domain::ConstraintSeverity::{Hard, Soft};
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let gym = new_place(&core, "Gym", 1).await;
    let home = new_place(&core, "Home", 2).await;
    let _idle = new_place(&core, "Idle", 3).await;
    task_at(&core, "a", vec![home], Soft).await;
    task_at(&core, "b", vec![home], Soft).await;
    task_at(&core, "c", vec![gym], Hard).await;
    assert_eq!(
        core.select_regions(None, 20).unwrap(),
        vec![pid(gym), pid(home)],
        "hard first, then demand; a place no open task needs is not monitored"
    );
    assert_eq!(core.select_regions(None, 2).unwrap(), vec![pid(gym)]);
    assert!(core.select_regions(None, 1).unwrap().is_empty());
    core.close().await.unwrap();
}
