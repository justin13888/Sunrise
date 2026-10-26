//! Saved views (issue #341): the entity, written only by `Patch`, its
//! evaluation, and what crosses between replicas.

use super::*;
use crate::engine::saved_view::SAVED_VIEW_FEATURE;
use crate::queries::SavedViewResult;
use sunrise_domain::{
    ContextDraft, ContextPatch, SavedView, SavedViewDraft, SavedViewPatch, ViewFilter, ViewKind,
    ViewSort,
};

const ROOT: [u8; 32] = [0x41; 32];

struct Pair {
    ea: Engine,
    eb: Engine,
    ca: Arc<FakeClock>,
    cb: Arc<FakeClock>,
    dba: Db,
    dbb: Db,
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
        to.apply_remote_all(to_db, &env).unwrap();
    }
}

fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba);
}

/// Two trusting replicas, each having advertised what it supports.
fn pair() -> Pair {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    assert!(ea.advertise_features(&mut dba).unwrap());
    assert!(eb.advertise_features(&mut dbb).unwrap());
    let mut p = Pair {
        ea,
        eb,
        ca,
        cb,
        dba,
        dbb,
    };
    sync(&mut p);
    p
}

fn draft(name: &str, view: ViewKind, filter: ViewFilter) -> SavedViewDraft {
    SavedViewDraft {
        name: name.into(),
        view,
        filter,
        sort: ViewSort::Default,
    }
}

fn save(e: &Engine, db: &mut Db, d: SavedViewDraft) -> EntityRef {
    e.apply(db, Command::CreateSavedView(d)).unwrap().entity
}

fn views(e: &Engine, db: &Db) -> Vec<SavedView> {
    match e.query(db, Query::SavedViews).unwrap() {
        QueryResult::SavedViews(v) => v,
        other => panic!("{other:?}"),
    }
}

fn run(e: &Engine, db: &Db, id: EntityRef) -> SavedViewResult {
    match e.query(db, Query::SavedView(id)).unwrap() {
        QueryResult::SavedView(r) => *r,
        other => panic!("{other:?}"),
    }
}

fn titles(r: &SavedViewResult) -> Vec<String> {
    r.tasks.iter().map(|t| t.title.clone()).collect()
}

fn context(e: &Engine, db: &mut Db, name: &str) -> EntityRef {
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

fn tagged(e: &Engine, db: &mut Db, title: &str, contexts: Vec<EntityRef>) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            contexts,
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

#[test]
fn a_view_is_saved_listed_evaluated_and_never_stores_its_result() {
    let mut p = pair();
    let errands = context(&p.ea, &mut p.dba, "errands");
    tagged(&p.ea, &mut p.dba, "milk", vec![errands]);
    tagged(&p.ea, &mut p.dba, "report", vec![]);
    let id = save(
        &p.ea,
        &mut p.dba,
        draft(
            "Errands",
            ViewKind::Inbox,
            ViewFilter {
                contexts: vec![errands],
                ..ViewFilter::default()
            },
        ),
    );
    let listed = views(&p.ea, &p.dba);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Errands");
    assert!(listed[0].sort_order.is_some(), "placed last in the list");
    let r = run(&p.ea, &p.dba, id);
    assert_eq!(titles(&r), ["milk"]);
    assert!(r.complete && r.missing_contexts.is_empty());
    // A task added later is in the next read: nothing was stored.
    tagged(&p.ea, &mut p.dba, "stamps", vec![errands]);
    assert_eq!(run(&p.ea, &p.dba, id).tasks.len(), 2);
    p.ea.assert_rebuild_identity(&p.dba);
}

/// The headline of #341: a view saved on one device runs on the other, and
/// renaming a context keeps it working there.
#[test]
fn a_view_saved_on_one_device_runs_on_the_other_through_a_rename() {
    let mut p = pair();
    let errands = context(&p.ea, &mut p.dba, "errands");
    tagged(&p.ea, &mut p.dba, "milk", vec![errands]);
    let id = save(
        &p.ea,
        &mut p.dba,
        draft(
            "Out",
            ViewKind::Inbox,
            ViewFilter {
                contexts: vec![errands],
                ..ViewFilter::default()
            },
        ),
    );
    sync(&mut p);
    assert_eq!(titles(&run(&p.eb, &p.dbb, id)), ["milk"]);
    p.eb.apply(
        &mut p.dbb,
        Command::UpdateContext {
            id: errands,
            patch: ContextPatch {
                name: Some("shops".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    sync(&mut p);
    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        let r = run(e, db, id);
        assert_eq!(titles(&r), ["milk"], "the rename broke nothing");
        assert!(r.missing_contexts.is_empty());
    }
    // A deleted context is reported, not silently matched by nothing.
    p.ea.apply(&mut p.dba, Command::DeleteContext(errands))
        .unwrap();
    let r = run(&p.ea, &p.dba, id);
    assert_eq!(r.missing_contexts, vec![errands]);
    assert!(r.tasks.is_empty());
}

/// Each field is its own register: a rename on one device and a filter edit
/// on the other, made apart, both survive, and neither is a conflict.
#[test]
fn concurrent_edits_to_different_fields_both_survive() {
    let mut p = pair();
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Mine", ViewKind::Today, ViewFilter::default()),
    );
    sync(&mut p);
    *p.ca.0.lock() = T0 + 1_000;
    *p.cb.0.lock() = T0 + 1_000;
    p.ea.apply(
        &mut p.dba,
        Command::UpdateSavedView {
            id,
            patch: SavedViewPatch {
                name: Some("Renamed".into()),
                ..SavedViewPatch::default()
            },
        },
    )
    .unwrap();
    p.eb.apply(
        &mut p.dbb,
        Command::UpdateSavedView {
            id,
            patch: SavedViewPatch {
                filter: Some(ViewFilter {
                    text: "milk".into(),
                    ..ViewFilter::default()
                }),
                sort: Some(ViewSort::Title),
                ..SavedViewPatch::default()
            },
        },
    )
    .unwrap();
    sync(&mut p);
    let a = views(&p.ea, &p.dba);
    assert_eq!(a, views(&p.eb, &p.dbb), "one view on both");
    assert_eq!(a[0].name, "Renamed");
    assert_eq!(a[0].filter.text, "milk");
    assert_eq!(a[0].sort, ViewSort::Title);
    let QueryResult::Conflicts(c) =
        p.ea.query(
            &p.dba,
            Query::Conflicts {
                entity: Some(id),
                include_quiet: true,
            },
        )
        .unwrap()
    else {
        panic!("conflicts");
    };
    assert!(c.is_empty(), "{c:?}");
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// Two writes of one field made apart are a tracked conflict, one winner on
/// both, and resolving it settles it everywhere.
#[test]
fn concurrent_renames_are_a_conflict_that_resolves() {
    let mut p = pair();
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Mine", ViewKind::Today, ViewFilter::default()),
    );
    sync(&mut p);
    for (e, db, name) in [(&p.ea, &mut p.dba, "A"), (&p.eb, &mut p.dbb, "B")] {
        e.apply(
            db,
            Command::UpdateSavedView {
                id,
                patch: SavedViewPatch {
                    name: Some(name.into()),
                    ..SavedViewPatch::default()
                },
            },
        )
        .unwrap();
    }
    sync(&mut p);
    let conflicts = |e: &Engine, db: &Db| match e
        .query(
            db,
            Query::Conflicts {
                entity: Some(id),
                include_quiet: false,
            },
        )
        .unwrap()
    {
        QueryResult::Conflicts(c) => c,
        other => panic!("{other:?}"),
    };
    let on_a = conflicts(&p.ea, &p.dba);
    assert_eq!(on_a.len(), 1);
    assert_eq!(on_a[0].field, "name");
    assert_eq!(on_a, conflicts(&p.eb, &p.dbb));
    p.ea.apply(
        &mut p.dba,
        Command::ResolveConflict {
            entity: id,
            field: "name".into(),
            value: sunrise_domain::CborValue(ciborium::value::Value::Text("Both".into())),
        },
    )
    .unwrap();
    sync(&mut p);
    assert!(conflicts(&p.ea, &p.dba).is_empty());
    assert!(conflicts(&p.eb, &p.dbb).is_empty());
    assert_eq!(views(&p.eb, &p.dbb)[0].name, "Both");
}

#[test]
fn a_delete_is_restored_and_an_edit_of_a_deleted_view_is_refused() {
    let mut p = pair();
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Mine", ViewKind::Today, ViewFilter::default()),
    );
    p.ea.apply(&mut p.dba, Command::DeleteSavedView(id))
        .unwrap();
    assert!(views(&p.ea, &p.dba).is_empty());
    assert!(matches!(
        p.ea.apply(
            &mut p.dba,
            Command::UpdateSavedView {
                id,
                patch: SavedViewPatch {
                    name: Some("x".into()),
                    ..SavedViewPatch::default()
                },
            },
        ),
        Err(EngineError::NotFound(_))
    ));
    p.ea.apply(&mut p.dba, Command::Restore(id)).unwrap();
    sync(&mut p);
    assert_eq!(views(&p.eb, &p.dbb).len(), 1);
    assert!(matches!(
        p.ea.apply(&mut p.dba, Command::Restore(id)),
        Err(EngineError::Invalid(_))
    ));
}

/// Every write is a `Patch` naming the fields it writes, with the entity's
/// heads as parents, and the vault requires the feature from the first.
#[test]
fn writes_are_patches_with_parents_and_require_the_feature() {
    let mut p = pair();
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Mine", ViewKind::Today, ViewFilter::default()),
    );
    let required: Vec<String> = {
        let mut stmt = p
            .dba
            .conn()
            .prepare("SELECT feature FROM vault_required_features")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert!(
        required.iter().any(|f| f == SAVED_VIEW_FEATURE),
        "{required:?}"
    );
    p.ea.apply(
        &mut p.dba,
        Command::UpdateSavedView {
            id,
            patch: SavedViewPatch {
                sort: Some(ViewSort::Manual),
                ..SavedViewPatch::default()
            },
        },
    )
    .unwrap();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = p
            .dba
            .conn()
            .prepare("SELECT envelope FROM ops WHERE inner_kind = 'saved_view.patch' ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(envs.len(), 2);
    let patch = |env: &[u8]| match decode_inner_op(&p.ea.keychain.open_op(env).unwrap()).unwrap() {
        InnerOp::Patch(p) => *p,
        other => panic!("{other:?}"),
    };
    let create = patch(&envs[0]);
    assert!(create.create);
    assert_eq!(create.parents.as_deref(), Some(&[][..]));
    let edit = patch(&envs[1]);
    assert!(!edit.create);
    assert_eq!(edit.fields.keys().collect::<Vec<_>>(), ["sort"]);
    assert_eq!(edit.parents.map(|p| p.len()), Some(1), "names the create");
}

#[test]
fn a_search_view_runs_the_search_and_other_views_filter() {
    let mut p = pair();
    tagged(&p.ea, &mut p.dba, "renew passport", vec![]);
    tagged(&p.ea, &mut p.dba, "buy milk", vec![]);
    let search = save(
        &p.ea,
        &mut p.dba,
        draft(
            "pp",
            ViewKind::Search,
            ViewFilter {
                text: "passport".into(),
                ..ViewFilter::default()
            },
        ),
    );
    assert_eq!(titles(&run(&p.ea, &p.dba, search)), ["renew passport"]);
    let calendar = save(
        &p.ea,
        &mut p.dba,
        draft("cal", ViewKind::Calendar, ViewFilter::default()),
    );
    assert!(
        run(&p.ea, &p.dba, calendar).tasks.is_empty(),
        "a screen, not a list"
    );
    let all = save(
        &p.ea,
        &mut p.dba,
        SavedViewDraft {
            sort: ViewSort::Title,
            ..draft("all", ViewKind::Stream, ViewFilter::default())
        },
    );
    assert_eq!(
        titles(&run(&p.ea, &p.dba, all)),
        ["buy milk", "renew passport"]
    );
}

/// A Today view starts from Today and an Inbox view from the Inbox view
/// (ADR-0046 §3): the planned task is in one and not the other.
#[test]
fn a_today_view_and_an_inbox_view_start_from_their_screens() {
    let mut p = pair();
    tagged(&p.ea, &mut p.dba, "loose", vec![]);
    p.ea.apply(
        &mut p.dba,
        Command::CreateTask(TaskDraft {
            title: "planned".into(),
            planned_at: Some(SunriseTime::instant(crate::engine::ids::ms_to_ts(
                i64::try_from(T0).unwrap() + 3_600_000,
            ))),
            ..Default::default()
        }),
    )
    .unwrap();
    let today = save(
        &p.ea,
        &mut p.dba,
        draft("t", ViewKind::Today, ViewFilter::default()),
    );
    let inbox = save(
        &p.ea,
        &mut p.dba,
        draft("i", ViewKind::Inbox, ViewFilter::default()),
    );
    assert_eq!(titles(&run(&p.ea, &p.dba, today)), ["planned"]);
    assert_eq!(titles(&run(&p.ea, &p.dba, inbox)), ["loose"]);
    // Naming no stream twice, as the flag and as the legacy reference,
    // lists its tasks once.
    let twice = save(
        &p.ea,
        &mut p.dba,
        draft(
            "n",
            ViewKind::Stream,
            ViewFilter {
                streams: vec![sunrise_domain::inbox_stream_ref()],
                no_stream: true,
                ..ViewFilter::default()
            },
        ),
    );
    assert_eq!(titles(&run(&p.ea, &p.dba, twice)), ["loose", "planned"]);
}

/// A filter part a newer build added survives this build's edit of another
/// field, and the view says it is incomplete.
#[test]
fn an_unknown_filter_part_survives_an_edit_of_another_field() {
    let mut p = pair();
    let mut filter = ViewFilter::default();
    filter.unknown.insert(
        "place".into(),
        sunrise_domain::CborValue(ciborium::value::Value::Text("plc_x".into())),
    );
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Near", ViewKind::Inbox, filter.clone()),
    );
    p.ea.apply(
        &mut p.dba,
        Command::UpdateSavedView {
            id,
            patch: SavedViewPatch {
                name: Some("Nearby".into()),
                ..SavedViewPatch::default()
            },
        },
    )
    .unwrap();
    sync(&mut p);
    let v = &views(&p.eb, &p.dbb)[0];
    assert_eq!(v.filter, filter);
    assert!(!run(&p.eb, &p.dbb, id).complete);
}

/// A view becomes searchable by its name, and leaves the index when deleted.
#[test]
fn a_view_is_found_by_its_name() {
    let mut p = pair();
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Groceries", ViewKind::Today, ViewFilter::default()),
    );
    let hits = |e: &Engine, db: &Db| match e
        .query(
            db,
            Query::SearchGrouped {
                text: "groceries".into(),
                now_ms: T0 as u64,
                prefix: false,
                per_group: None,
            },
        )
        .unwrap()
    {
        QueryResult::Search(r) => r
            .groups
            .iter()
            .filter(|g| g.kind == sunrise_domain::search::SearchKind::View)
            .flat_map(|g| g.hits.iter().map(|h| h.entity))
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(hits(&p.ea, &p.dba), vec![id]);
    p.ea.apply(&mut p.dba, Command::DeleteSavedView(id))
        .unwrap();
    assert!(hits(&p.ea, &p.dba).is_empty());
    p.ea.assert_rebuild_identity(&p.dba);
}

#[test]
fn a_draft_that_does_not_validate_writes_nothing() {
    let mut p = pair();
    let ops = op_count(&p.dba);
    assert!(p
        .ea
        .apply(
            &mut p.dba,
            Command::CreateSavedView(draft(" ", ViewKind::Today, ViewFilter::default()))
        )
        .is_err());
    assert!(views(&p.ea, &p.dba).is_empty());
    assert_eq!(op_count(&p.dba), ops, "no op was logged");
}

/// An edit that names no field is refused as `Invalid`, and writes nothing.
#[test]
fn an_edit_that_changes_nothing_is_refused_and_writes_nothing() {
    let mut p = pair();
    let id = save(
        &p.ea,
        &mut p.dba,
        draft("Errands", ViewKind::Today, ViewFilter::default()),
    );
    let before = views(&p.ea, &p.dba);
    let ops = op_count(&p.dba);
    let err =
        p.ea.apply(
            &mut p.dba,
            Command::UpdateSavedView {
                id,
                patch: SavedViewPatch::default(),
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    assert_eq!(op_count(&p.dba), ops);
    assert_eq!(views(&p.ea, &p.dba), before);
}

/// A view this replica never held, and one it holds deleted, is not found
/// to an edit or a delete, and neither writes anything.
#[test]
fn an_edit_or_delete_of_a_view_not_held_is_not_found() {
    let mut p = pair();
    let gone = save(
        &p.ea,
        &mut p.dba,
        draft("Gone", ViewKind::Today, ViewFilter::default()),
    );
    p.ea.apply(&mut p.dba, Command::DeleteSavedView(gone))
        .unwrap();
    let never = EntityRef::new(EntityKind::SavedView, [0x5a; 16]);
    let ops = op_count(&p.dba);
    for id in [never, gone] {
        let rename = SavedViewPatch {
            name: Some("Renamed".into()),
            ..Default::default()
        };
        for cmd in [
            Command::UpdateSavedView { id, patch: rename },
            Command::DeleteSavedView(id),
        ] {
            let err = p.ea.apply(&mut p.dba, cmd).unwrap_err();
            assert!(matches!(err, EngineError::NotFound(_)), "{err:?}");
        }
    }
    assert_eq!(op_count(&p.dba), ops);
    assert!(views(&p.ea, &p.dba).is_empty());
}
