//! A task bound to one occurrence of a recurring block
//! (`docs/02-domain/time-blocks.md` §Recurring blocks, #1162).
//!
//! The series is `block_recurrence::weekly`: a "Standup" every Wednesday from
//! 2026-03-04, whose occurrence keys are `key(4)`, `key(11)`, `key(18)` and
//! so on.

use super::block_recurrence::{at, key, march, recur, scoped_update, weekly};
use super::*;
use crate::feature::{BLOCK_OCCURRENCE_TASKS_FEATURE, BLOCK_RECURRENCE_FEATURE};
use sunrise_domain::{split_block_id, EditScope, OccurrenceBinding};

fn bind(block: EntityRef, task: EntityRef, occurrence: Option<String>) -> Command {
    Command::BindTask {
        block,
        task,
        occurrence,
    }
}

fn unbind(block: EntityRef, task: EntityRef, occurrence: Option<String>) -> Command {
    Command::UnbindTask {
        block,
        task,
        occurrence,
    }
}

/// `weekly`, in a vault that also requires `block.occurrence_tasks`.
fn bindable(e: &Engine, db: &mut Db) -> EntityRef {
    let id = weekly(e, db);
    e.require_features(db, &[BLOCK_OCCURRENCE_TASKS_FEATURE], true)
        .unwrap();
    id
}

/// The titles of the tasks the day grid of 2026-03-`day` lists on its one
/// row.
fn titles_on(e: &Engine, db: &Db, day: i8) -> Vec<String> {
    let mut rows = day_rows(e, db, march(day));
    assert_eq!(rows.len(), 1, "one standup on 2026-03-{day}");
    rows.remove(0).task_titles
}

fn blocks_of(db: &Db, task: EntityRef) -> BTreeSet<EntityRef> {
    read_task(db.conn(), task.bytes()).unwrap().unwrap().blocks
}

/// A task bound to one week is listed on that week's row alone, and names
/// the block once however many weeks it is bound to; unbinding one week
/// leaves the others.
#[test]
fn a_task_bound_to_one_occurrence_is_not_bound_to_the_others() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");

    e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap();
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);
    assert!(titles_on(&e, &db, 4).is_empty());
    assert!(titles_on(&e, &db, 18).is_empty());
    assert_eq!(blocks_of(&db, task), BTreeSet::from([id]));
    let block = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert!(block.tasks.is_empty(), "nothing is bound to the series");
    assert_eq!(
        block.occurrence_tasks,
        BTreeSet::from([OccurrenceBinding::new(key(11), task)])
    );

    e.apply(&mut db, bind(id, task, Some(key(18)))).unwrap();
    assert_eq!(titles_on(&e, &db, 18), ["Prepare notes"]);
    assert_eq!(blocks_of(&db, task), BTreeSet::from([id]), "named once");

    e.apply(&mut db, unbind(id, task, Some(key(11)))).unwrap();
    assert!(titles_on(&e, &db, 11).is_empty());
    assert_eq!(titles_on(&e, &db, 18), ["Prepare notes"]);
    assert_eq!(blocks_of(&db, task), BTreeSet::from([id]));

    e.apply(&mut db, unbind(id, task, Some(key(18)))).unwrap();
    assert!(blocks_of(&db, task).is_empty());
    e.assert_rebuild_identity(&db);
}

/// A binding to the whole series, made before occurrence bindings existed,
/// is listed on every week beside one week's own, and unbinding with no key
/// removes it.
#[test]
fn a_whole_series_binding_shows_on_every_occurrence_beside_one_weeks_own() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let every = new_task(&e, &mut db, "Agenda");
    let one = new_task(&e, &mut db, "Demo");
    super::block_recurrence::bound_to_the_series(&e, &mut db, id, every);
    e.apply(&mut db, bind(id, one, Some(key(11)))).unwrap();

    assert_eq!(titles_on(&e, &db, 4), ["Agenda"]);
    let mut eleventh = titles_on(&e, &db, 11);
    eleventh.sort();
    assert_eq!(eleventh, ["Agenda", "Demo"]);

    e.apply(&mut db, unbind(id, every, None)).unwrap();
    assert!(titles_on(&e, &db, 4).is_empty());
    assert_eq!(titles_on(&e, &db, 11), ["Demo"]);
    e.assert_rebuild_identity(&db);
}

/// Unbinding with no key a task the block binds only to some weeks is
/// refused and writes nothing: there is no whole-block binding to remove,
/// and the task would stay bound. With the week's key it is unbound.
#[test]
fn unbinding_with_no_key_a_task_bound_only_to_weeks_is_refused() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");
    e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap();
    let ops = op_count(&db);

    invalid(
        e.apply(&mut db, unbind(id, task, None)),
        "name the occurrence",
    );
    assert_eq!(op_count(&db), ops, "a refused unbind writes nothing");
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);

    e.apply(&mut db, unbind(id, task, Some(key(11)))).unwrap();
    assert!(blocks_of(&db, task).is_empty());
}

/// A re-import keeps the occurrence bindings the block holds, and one that
/// moves the anchor moves each binding with the week it named, as an edit of
/// the whole block does.
#[test]
fn a_reimport_keeps_the_occurrence_bindings_and_moves_them_with_the_anchor() {
    let mut db = db();
    let e = engine();
    legacy_peer(&db);
    let id = e
        .apply(
            &mut db,
            import("ics", "standup", block_draft(9, 1, Some("Standup"))),
        )
        .unwrap()
        .entity;
    e.require_features(
        &mut db,
        &[BLOCK_RECURRENCE_FEATURE, BLOCK_OCCURRENCE_TASKS_FEATURE],
        true,
    )
    .unwrap();
    super::block_recurrence::recur(&e, &mut db, id, "FREQ=WEEKLY");
    let task = new_task(&e, &mut db, "Prepare notes");
    e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap();
    let bound_at = |db: &Db| read_block(db.conn(), id.bytes()).unwrap().unwrap();

    // The re-imported item states its rule, which is the calendar's.
    e.apply(
        &mut db,
        import_series(
            "ics",
            "standup",
            block_draft(9, 1, Some("Standup")),
            "FREQ=WEEKLY",
        ),
    )
    .unwrap();
    assert_eq!(
        bound_at(&db).occurrence_tasks,
        BTreeSet::from([OccurrenceBinding::new(key(11), task)])
    );
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);

    e.apply(
        &mut db,
        import_series(
            "ics",
            "standup",
            block_draft(10, 1, Some("Standup")),
            "FREQ=WEEKLY",
        ),
    )
    .unwrap();
    assert_eq!(
        bound_at(&db).occurrence_tasks,
        BTreeSet::from([OccurrenceBinding::new("2026-03-11T10:00", task)])
    );
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);
    assert!(titles_on(&e, &db, 18).is_empty());
    assert_eq!(blocks_of(&db, task), BTreeSet::from([id]));
    e.assert_rebuild_identity(&db);
}

/// A binding that does not name one occurrence is refused, and writes
/// nothing: no key on a recurring block, a key on a one-off, a key that is
/// not one, a week the rule skips, and a cancelled week.
#[test]
fn a_binding_that_names_no_occurrence_of_the_series_is_refused() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");
    let one_off = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(14, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::This { key: key(25) },
        },
    )
    .unwrap();
    let ops = op_count(&db);

    invalid(e.apply(&mut db, bind(id, task, None)), "recurs");
    invalid(
        e.apply(&mut db, bind(one_off, task, Some(key(11)))),
        "does not recur",
    );
    invalid(
        e.apply(&mut db, unbind(one_off, task, Some(key(11)))),
        "does not recur",
    );
    assert!(matches!(
        e.apply(&mut db, bind(id, task, Some("wednesday".into()))),
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        e.apply(&mut db, bind(id, task, Some("2026-03-12T09:00".into()))),
        Err(EngineError::NotFound(_))
    ));
    assert!(matches!(
        e.apply(&mut db, bind(id, task, Some(key(25)))),
        Err(EngineError::NotFound(m)) if m.contains("cancelled")
    ));
    assert_eq!(op_count(&db), ops, "a refused binding writes nothing");
}

/// An occurrence binding is sealed only once the vault requires
/// `block.occurrence_tasks`: a build without it would show the week with no
/// task.
#[test]
fn an_occurrence_binding_is_sealed_only_once_the_vault_requires_its_feature() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");
    let ops = op_count(&db);
    let err = e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains(BLOCK_OCCURRENCE_TASKS_FEATURE)),
        "{err:?}"
    );
    assert_eq!(op_count(&db), ops);

    e.require_features(&mut db, &[BLOCK_OCCURRENCE_TASKS_FEATURE], true)
        .unwrap();
    e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap();
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);
}

/// A split *this and future* carries the bindings at or after the key to
/// the continuation, and the ones before stay; the task bound after the key
/// then names the continuation. The edit moves the continuation an hour, and
/// each binding moves with the week it named.
#[test]
fn a_split_carries_the_bindings_at_or_after_the_key_to_the_continuation() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let before = new_task(&e, &mut db, "Agenda");
    let at_key = new_task(&e, &mut db, "Demo");
    let after = new_task(&e, &mut db, "Retro notes");
    for (task, week) in [(before, 11), (at_key, 18), (after, 25)] {
        e.apply(&mut db, bind(id, task, Some(key(week)))).unwrap();
    }

    let move_later = BlockPatch {
        starts_at: Some(at(18, 10)),
        ends_at: Some(at(18, 11)),
        ..BlockPatch::default()
    };
    let next = e
        .apply(
            &mut db,
            scoped_update(id, EditScope::ThisAndFuture { key: key(18) }, move_later),
        )
        .unwrap()
        .entity;
    assert_eq!(next, split_block_id(&id, &key(18)));

    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(
        ended.occurrence_tasks,
        BTreeSet::from([OccurrenceBinding::new(key(11), before)])
    );
    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert_eq!(
        continuation.occurrence_tasks,
        BTreeSet::from([
            OccurrenceBinding::new("2026-03-18T10:00", at_key),
            OccurrenceBinding::new("2026-03-25T10:00", after),
        ])
    );
    assert_eq!(blocks_of(&db, before), BTreeSet::from([id]));
    assert_eq!(blocks_of(&db, at_key), BTreeSet::from([next]));
    assert_eq!(blocks_of(&db, after), BTreeSet::from([next]));
    assert_eq!(titles_on(&e, &db, 11), ["Agenda"]);
    assert_eq!(titles_on(&e, &db, 18), ["Demo"]);
    assert_eq!(titles_on(&e, &db, 25), ["Retro notes"]);

    // Binding again on the continuation names its own occurrences.
    e.apply(&mut db, bind(next, before, Some("2026-04-01T09:00".into())))
        .unwrap_err();
    e.apply(&mut db, bind(next, before, Some("2026-04-01T10:00".into())))
        .unwrap();
    assert_eq!(blocks_of(&db, before), BTreeSet::from([id, next]));
    e.assert_rebuild_identity(&db);
}

/// Deleting *this and future* ends the series before the key, and the
/// bindings of the weeks it removed go with them.
#[test]
fn deleting_this_and_future_drops_the_bindings_of_the_weeks_it_removed() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let kept = new_task(&e, &mut db, "Agenda");
    let gone = new_task(&e, &mut db, "Retro notes");
    e.apply(&mut db, bind(id, kept, Some(key(11)))).unwrap();
    e.apply(&mut db, bind(id, gone, Some(key(25)))).unwrap();

    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::ThisAndFuture { key: key(18) },
        },
    )
    .unwrap();
    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(
        ended.occurrence_tasks,
        BTreeSet::from([OccurrenceBinding::new(key(11), kept)])
    );
    assert_eq!(blocks_of(&db, kept), BTreeSet::from([id]));
    assert!(blocks_of(&db, gone).is_empty());
    e.assert_rebuild_identity(&db);
}

/// Two paired devices, A and B, each with its own clock and vault, sharing
/// a weekly "Standup" that A created, with one task made on each: "Agenda"
/// on A and "Demo" on B.
struct Pair {
    ca: Arc<FakeClock>,
    cb: Arc<FakeClock>,
    ea: Engine,
    eb: Engine,
    dba: Db,
    dbb: Db,
    id: EntityRef,
    on_a: EntityRef,
    on_b: EntityRef,
}

impl Pair {
    fn new() -> Self {
        let ca = Arc::new(FakeClock(PLMutex::new(T0)));
        let cb = Arc::new(FakeClock(PLMutex::new(T0)));
        let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
        let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);
        trust(&ea, &mut dba, &eb);
        ea.advertise_features(&mut dba).unwrap();
        eb.advertise_features(&mut dbb).unwrap();
        sync(&ea, &mut dba, &eb, &mut dbb);
        let id = ea
            .apply(
                &mut dba,
                Command::CreateBlock(block_draft(9, 1, Some("Standup"))),
            )
            .unwrap()
            .entity;
        ea.require_features(
            &mut dba,
            &[BLOCK_RECURRENCE_FEATURE, BLOCK_OCCURRENCE_TASKS_FEATURE],
            false,
        )
        .unwrap();
        set_clock(&ca, T0 + 1_000);
        super::block_recurrence::recur(&ea, &mut dba, id, "FREQ=WEEKLY");
        let on_a = new_task(&ea, &mut dba, "Agenda");
        let on_b = new_task(&eb, &mut dbb, "Demo");
        sync(&ea, &mut dba, &eb, &mut dbb);
        Self {
            ca,
            cb,
            ea,
            eb,
            dba,
            dbb,
            id,
            on_a,
            on_b,
        }
    }

    /// Each device applies every envelope the other holds, twice, so what
    /// one applied reaches the other.
    fn sync(&mut self) {
        for _ in 0..2 {
            sync(&self.ea, &mut self.dba, &self.eb, &mut self.dbb);
        }
    }

    /// Both devices hold `want` as the series' occurrence bindings, and each
    /// rebuilds its projection identically.
    fn both_hold(&self, want: &BTreeSet<OccurrenceBinding>) {
        for (e, db) in [(&self.ea, &self.dba), (&self.eb, &self.dbb)] {
            let block = read_block(db.conn(), self.id.bytes()).unwrap().unwrap();
            assert_eq!(&block.occurrence_tasks, want);
            e.assert_rebuild_identity(db);
        }
    }
}

/// B applies every envelope A holds, then A every one B holds.
fn sync(ea: &Engine, dba: &mut Db, eb: &Engine, dbb: &mut Db) {
    for env in all_envelopes(dba) {
        let _ = eb.apply_remote(dbb, &env);
    }
    for env in all_envelopes(dbb) {
        let _ = ea.apply_remote(dba, &env);
    }
}

/// Two devices binding two tasks to the same week at once both keep their
/// binding: `occurrence_tasks` merges as an OR-set.
#[test]
fn two_devices_binding_the_same_week_both_keep_their_binding() {
    let mut p = Pair::new();
    set_clock(&p.ca, T0 + 2_000);
    p.ea.apply(&mut p.dba, bind(p.id, p.on_a, Some(key(11))))
        .unwrap();
    set_clock(&p.cb, T0 + 3_000);
    p.eb.apply(&mut p.dbb, bind(p.id, p.on_b, Some(key(11))))
        .unwrap();
    p.sync();

    p.both_hold(&BTreeSet::from([
        OccurrenceBinding::new(key(11), p.on_a),
        OccurrenceBinding::new(key(11), p.on_b),
    ]));
    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        let mut titles = titles_on(e, db, 11);
        titles.sort();
        assert_eq!(titles, ["Agenda", "Demo"]);
        assert!(titles_on(e, db, 18).is_empty());
    }
}

/// A known gap (`docs/02-domain/time-blocks.md` §Recurring blocks): a week
/// bound on one device while the
/// other moves the series is kept under the key it named before the move.
/// The move re-keys only the bindings its device held, so the concurrent
/// one survives the OR-set merge under a key no occurrence of the moved
/// series has: no week lists the task, though it still names the block.
#[test]
fn a_week_bound_while_another_device_moves_the_series_keeps_its_old_key() {
    let mut p = Pair::new();
    p.ea.apply(&mut p.dba, bind(p.id, p.on_a, Some(key(11))))
        .unwrap();
    p.sync();

    set_clock(&p.cb, T0 + 2_000);
    p.eb.apply(&mut p.dbb, bind(p.id, p.on_b, Some(key(18))))
        .unwrap();
    set_clock(&p.ca, T0 + 3_000);
    let an_hour_later = BlockPatch {
        starts_at: Some(at(4, 10)),
        ends_at: Some(at(4, 11)),
        ..BlockPatch::default()
    };
    p.ea.apply(
        &mut p.dba,
        scoped_update(p.id, EditScope::All, an_hour_later),
    )
    .unwrap();
    p.sync();

    p.both_hold(&BTreeSet::from([
        OccurrenceBinding::new("2026-03-11T10:00", p.on_a),
        OccurrenceBinding::new(key(18), p.on_b),
    ]));
    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        assert_eq!(titles_on(e, db, 11), ["Agenda"]);
        assert!(titles_on(e, db, 18).is_empty(), "the moved week lost it");
        assert_eq!(blocks_of(db, p.on_b), BTreeSet::from([p.id]));
    }
}

/// Moving the whole series moves every occurrence key, and each binding
/// moves with the week it named.
#[test]
fn moving_the_series_moves_its_bindings_with_their_weeks() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");
    e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap();

    let an_hour_later = BlockPatch {
        starts_at: Some(at(4, 10)),
        ends_at: Some(at(4, 11)),
        ..BlockPatch::default()
    };
    e.apply(&mut db, scoped_update(id, EditScope::All, an_hour_later))
        .unwrap();
    let block = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(
        block.occurrence_tasks,
        BTreeSet::from([OccurrenceBinding::new("2026-03-11T10:00", task)])
    );
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);
    assert!(titles_on(&e, &db, 18).is_empty());
    e.assert_rebuild_identity(&db);
}

/// A rule that holds the weekday (`BYDAY=WE`) does not follow an anchor
/// moved to Thursday, so neither could a week's binding: the move is
/// refused, as one that would strand an exception is, and the week keeps
/// its task (#1275).
#[test]
fn a_move_that_would_strand_a_binding_is_refused() {
    let mut db = db();
    let e = engine();
    let id = bindable(&e, &mut db);
    recur(&e, &mut db, id, "FREQ=WEEKLY;BYDAY=WE");
    let task = new_task(&e, &mut db, "Prepare notes");
    e.apply(&mut db, bind(id, task, Some(key(11)))).unwrap();
    let before = read_block(db.conn(), id.bytes()).unwrap().unwrap();

    let to_thursday = BlockPatch {
        starts_at: Some(at(5, 9)),
        ends_at: Some(at(5, 10)),
        ..BlockPatch::default()
    };
    let refused = e.apply(&mut db, scoped_update(id, EditScope::All, to_thursday));
    assert!(
        matches!(&refused, Err(EngineError::Invalid(why)) if why.contains(&key(11))),
        "{refused:?}"
    );
    assert_eq!(read_block(db.conn(), id.bytes()).unwrap().unwrap(), before);
    assert_eq!(titles_on(&e, &db, 11), ["Prepare notes"]);
    assert_eq!(blocks_of(&db, task), BTreeSet::from([id]));

    // A split's continuation moved to Thursday would strand week 25's
    // binding the same way, so the split is refused and writes nothing.
    let later = new_task(&e, &mut db, "Retro notes");
    e.apply(&mut db, bind(id, later, Some(key(25)))).unwrap();
    let before = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    let to_thursday = BlockPatch {
        starts_at: Some(at(19, 9)),
        ends_at: Some(at(19, 10)),
        ..BlockPatch::default()
    };
    assert!(matches!(
        e.apply(
            &mut db,
            scoped_update(id, EditScope::ThisAndFuture { key: key(18) }, to_thursday),
        ),
        Err(EngineError::Invalid(_))
    ));
    assert_eq!(read_block(db.conn(), id.bytes()).unwrap().unwrap(), before);
    let next = split_block_id(&id, &key(18));
    assert!(read_block(db.conn(), next.bytes()).unwrap().is_none());
    assert_eq!(titles_on(&e, &db, 25), ["Retro notes"]);
    e.assert_rebuild_identity(&db);
}
