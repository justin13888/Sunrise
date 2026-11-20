//! Recurring blocks: one row per occurrence, and the edit scopes that change
//! one occurrence, the rest of a series, or all of it
//! (`docs/02-domain/time-blocks.md` §Recurring blocks, #1148).

use super::*;
use crate::feature::BLOCK_RECURRENCE_FEATURE;
use sunrise_domain::{split_block_id, Block, EditScope};

/// Give block `id` the rule `rule`, as a build that writes series would:
/// no command creates one yet.
pub(super) fn recur(e: &Engine, db: &mut Db, id: EntityRef, rule: &str) {
    let b = Block {
        rrule: Some(RRule::parse(rule).unwrap()),
        ..read_block(db.conn(), id.bytes()).unwrap().unwrap()
    };
    let stream = crate::engine::ids::domain_bytes(b.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(b));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .unwrap();
}

/// A "Standup" every Wednesday 09:00–10:00 (floating) from 2026-03-04, in
/// a vault that requires `block.recurrence`.
pub(super) fn weekly(e: &Engine, db: &mut Db) -> EntityRef {
    legacy_peer(db);
    let id = e
        .apply(db, Command::CreateBlock(block_draft(9, 1, Some("Standup"))))
        .unwrap()
        .entity;
    e.require_features(db, &[BLOCK_RECURRENCE_FEATURE], true)
        .unwrap();
    recur(e, db, id, "FREQ=WEEKLY");
    id
}

/// Noon on 2026-03-`day`, for a day query.
pub(super) fn march(day: i8) -> u64 {
    SunriseTime::floating(jiff::civil::date(2026, 3, day).at(12, 0, 0, 0))
        .to_instant(&jiff::tz::TimeZone::UTC)
        .unwrap()
        .as_millisecond()
        .cast_unsigned()
}

/// 2026-03-`day` at `hour`:00, floating.
pub(super) fn at(day: i8, hour: i8) -> SunriseTime {
    SunriseTime::floating(jiff::civil::date(2026, 3, day).at(hour, 0, 0, 0))
}

/// What the day grid of 2026-03-`day` shows: each row's block, title and
/// occurrence key.
fn day(e: &Engine, db: &Db, day: i8) -> Vec<(EntityRef, Option<String>, Option<String>)> {
    day_rows(e, db, march(day))
        .into_iter()
        .map(|r| (r.block.id, r.title, r.occurrence_key))
        .collect()
}

pub(super) fn key(day: i8) -> String {
    format!("2026-03-{day:02}T09:00")
}

pub(super) fn scoped_update(id: EntityRef, scope: EditScope, patch: BlockPatch) -> Command {
    Command::UpdateBlock { id, scope, patch }
}

fn retitle(title: &str) -> BlockPatch {
    BlockPatch {
        title: Some(Some(title.into())),
        ..BlockPatch::default()
    }
}

/// A weekly block is one row per week, each at its own date with its key,
/// and nothing per occurrence is stored.
#[test]
fn a_weekly_block_is_one_row_per_week_in_the_window_with_its_key() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let rows = day_rows(&e, &db, march(11));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].block.id, id);
    assert_eq!(rows[0].occurrence_key, Some(key(11)));
    assert_eq!(rows[0].block.starts_at, at(11, 9));
    assert_eq!(rows[0].block.ends_at, at(11, 10));
    assert!(day(&e, &db, 12).is_empty(), "a Thursday has no standup");

    let from = march(1).cast_signed();
    let to = from + 28 * 86_400_000;
    let QueryResult::Blocks(rows) = e.query_blocks_between(&db, from, to).unwrap() else {
        panic!("expected blocks");
    };
    let keys: Vec<_> = rows.into_iter().filter_map(|r| r.occurrence_key).collect();
    assert_eq!(keys, vec![key(4), key(11), key(18), key(25)]);

    let stored: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM blocks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, 1);
}

/// A one-off block keeps no key, and reading a recurring one by id returns
/// the series as stored.
#[test]
fn a_one_off_block_and_a_block_read_by_id_carry_no_key() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let once = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(14, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let rows = day(&e, &db, 4);
    assert_eq!(
        rows,
        vec![
            (id, Some("Standup".into()), Some(key(4))),
            (once, Some("Gym".into()), None),
        ]
    );
}

/// *This* writes the occurrence's exception and nothing else: the other
/// weeks keep the series' title and time.
#[test]
fn an_edit_of_this_occurrence_changes_that_week_only() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let patch = BlockPatch {
        starts_at: Some(at(11, 10)),
        ends_at: Some(at(11, 11)),
        ..retitle("Moved standup")
    };
    let res = e
        .apply(
            &mut db,
            scoped_update(id, EditScope::This { key: key(11) }, patch),
        )
        .unwrap();
    assert_eq!(res.entity, id);

    let rows = day_rows(&e, &db, march(11));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title.as_deref(), Some("Moved standup"));
    assert_eq!(rows[0].block.starts_at, at(11, 10));
    assert_eq!(rows[0].occurrence_key, Some(key(11)));
    assert_eq!(
        day(&e, &db, 18),
        vec![(id, Some("Standup".into()), Some(key(18)))]
    );

    // A second edit of the same occurrence keeps what the first set.
    e.apply(
        &mut db,
        scoped_update(id, EditScope::This { key: key(11) }, retitle("Sync")),
    )
    .unwrap();
    let stored = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(stored.exceptions.len(), 1);
    assert_eq!(stored.exceptions[&key(11)].title.as_deref(), Some("Sync"));
    assert_eq!(stored.exceptions[&key(11)].starts_at, Some(at(11, 10)));
    assert_eq!(stored.title.as_deref(), Some("Standup"));
    e.assert_rebuild_identity(&db);
}

/// What one occurrence cannot hold, a key the rule does not produce, and a
/// key on a block with no rule are each refused, writing nothing.
#[test]
fn an_occurrence_edit_the_series_cannot_take_is_refused() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let once = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(14, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let ops = op_count(&db);
    let this = |k: String| EditScope::This { key: k };

    let restream = BlockPatch {
        stream_id: Some(None),
        ..BlockPatch::default()
    };
    invalid(
        e.apply(&mut db, scoped_update(id, this(key(11)), restream)),
        "an edit of one occurrence",
    );
    let clear = BlockPatch {
        title: Some(None),
        ..BlockPatch::default()
    };
    invalid(
        e.apply(&mut db, scoped_update(id, this(key(11)), clear)),
        "an edit of one occurrence",
    );
    assert!(matches!(
        e.apply(
            &mut db,
            scoped_update(id, this("2026-03-12T09:00".into()), retitle("x"))
        ),
        Err(EngineError::NotFound(_))
    ));
    invalid(
        e.apply(
            &mut db,
            scoped_update(id, this("soon".into()), retitle("x")),
        ),
        "not an occurrence key",
    );
    invalid(
        e.apply(&mut db, scoped_update(once, this(key(4)), retitle("x"))),
        "does not recur",
    );
    invalid(
        e.apply(
            &mut db,
            Command::DeleteBlock {
                id: once,
                scope: EditScope::ThisAndFuture { key: key(4) },
            },
        ),
        "does not recur",
    );
    let inverted = BlockPatch {
        ends_at: Some(at(11, 8)),
        ..BlockPatch::default()
    };
    assert!(matches!(
        e.apply(&mut db, scoped_update(id, this(key(11)), inverted)),
        Err(EngineError::Validation(ValidationError::Field {
            field: "block.ends_at",
            constraint: "after_starts_at",
        }))
    ));
    assert_eq!(op_count(&db), ops, "a refused edit wrote nothing");
}

/// Deleting *this* cancels the one occurrence, and the block stays.
#[test]
fn deleting_this_occurrence_cancels_that_week_only() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::This { key: key(11) },
        },
    )
    .unwrap();
    assert!(day(&e, &db, 11).is_empty());
    assert_eq!(day(&e, &db, 18).len(), 1);
    let stored = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert!(!stored.deleted);
    assert!(stored.exceptions[&key(11)].cancelled);

    // A cancelled occurrence is not there to edit.
    assert!(matches!(
        e.apply(
            &mut db,
            scoped_update(id, EditScope::This { key: key(11) }, retitle("x"))
        ),
        Err(EngineError::NotFound(_))
    ));
}

/// *This and future* ends the series before the key and continues it in the
/// block `split_block_id(root, key)` names, with the edit applied, as one
/// group. Splitting again at that key edits the continuation.
#[test]
fn an_edit_of_this_and_future_splits_the_series_at_the_key() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let ops = op_count(&db);
    let next = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                retitle("Retro"),
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(next, split_block_id(&id, &key(18)));
    assert_eq!(
        op_count(&db),
        ops + 2,
        "the ended series and its continuation"
    );

    assert_eq!(
        day(&e, &db, 11),
        vec![(id, Some("Standup".into()), Some(key(11)))]
    );
    assert_eq!(
        day(&e, &db, 18),
        vec![(next, Some("Retro".into()), Some(key(18)))]
    );
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Retro".into()), Some(key(25)))]
    );

    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert_eq!(continuation.split_from, Some(id));
    assert_eq!(continuation.starts_at, at(18, 9));
    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(ended.rrule.unwrap().until, Some(at(11, 9)));

    // Again at the same key: the continuation is edited, nothing new made.
    let again = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                retitle("Review"),
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(again, next);
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Review".into()), Some(key(25)))]
    );
    e.assert_rebuild_identity(&db);
}

/// At the series' first occurrence *this and future* is the whole series.
#[test]
fn an_edit_of_this_and_future_at_the_first_occurrence_edits_the_block() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let res = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(4) },
                retitle("Retro"),
            ),
        )
        .unwrap();
    assert_eq!(res.entity, id);
    assert_eq!(
        day(&e, &db, 4),
        vec![(id, Some("Retro".into()), Some(key(4)))]
    );
    assert_eq!(
        day(&e, &db, 25),
        vec![(id, Some("Retro".into()), Some(key(25)))]
    );
}

/// Cancel week `cancelled` and retitle week `retitled` "Retro", each as an
/// edit of that one occurrence.
fn except_two_weeks(e: &Engine, db: &mut Db, id: EntityRef, cancelled: i8, retitled: i8) {
    e.apply(
        db,
        Command::DeleteBlock {
            id,
            scope: EditScope::This {
                key: key(cancelled),
            },
        },
    )
    .unwrap();
    e.apply(
        db,
        scoped_update(id, EditScope::This { key: key(retitled) }, retitle("Retro")),
    )
    .unwrap();
}

/// The standup moved to 10:00–11:00 on 2026-03-`day`.
fn an_hour_later(day: i8) -> BlockPatch {
    BlockPatch {
        starts_at: Some(at(day, 10)),
        ends_at: Some(at(day, 11)),
        ..BlockPatch::default()
    }
}

/// Moving the whole series an hour moves its exceptions with their weeks:
/// the cancelled week stays cancelled and the retitled week keeps its title
/// (#1200).
#[test]
fn moving_the_series_moves_its_exceptions_with_their_weeks() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    except_two_weeks(&e, &mut db, id, 11, 18);

    e.apply(&mut db, scoped_update(id, EditScope::All, an_hour_later(4)))
        .unwrap();
    assert!(day(&e, &db, 11).is_empty(), "the cancelled week stays so");
    assert_eq!(
        day(&e, &db, 18),
        vec![(id, Some("Retro".into()), Some("2026-03-18T10:00".into()))]
    );
    assert_eq!(
        day(&e, &db, 25),
        vec![(id, Some("Standup".into()), Some("2026-03-25T10:00".into()))]
    );
    let block = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(
        block
            .exceptions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["2026-03-11T10:00", "2026-03-18T10:00"]
    );
    e.assert_rebuild_identity(&db);
}

/// A split *this and future* that moves the continuation moves the
/// exceptions it carries with their weeks; the ended series keeps its own
/// where they were (#1200).
#[test]
fn moving_this_and_future_moves_the_continuations_exceptions() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::This { key: key(4) },
        },
    )
    .unwrap();
    except_two_weeks(&e, &mut db, id, 18, 25);

    let next = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(11) },
                an_hour_later(11),
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(
        day(&e, &db, 11),
        vec![(
            next,
            Some("Standup".into()),
            Some("2026-03-11T10:00".into())
        )]
    );
    assert!(day(&e, &db, 18).is_empty(), "the cancelled week stays so");
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Retro".into()), Some("2026-03-25T10:00".into()))]
    );
    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert_eq!(
        continuation
            .exceptions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["2026-03-18T10:00", "2026-03-25T10:00"]
    );
    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(
        ended.exceptions.keys().cloned().collect::<Vec<_>>(),
        [key(4)]
    );
    e.assert_rebuild_identity(&db);
}

/// A rule that holds the weekday (`BYDAY=WE`) does not follow an anchor
/// moved to Thursday, so neither could its cancelled week: the move is
/// refused and the series is left as it was (#1200).
#[test]
fn a_move_that_would_strand_an_exception_is_refused() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    recur(&e, &mut db, id, "FREQ=WEEKLY;BYDAY=WE");
    except_two_weeks(&e, &mut db, id, 11, 18);
    let before = read_block(db.conn(), id.bytes()).unwrap().unwrap();

    let to_thursday = BlockPatch {
        starts_at: Some(at(5, 9)),
        ends_at: Some(at(5, 10)),
        ..BlockPatch::default()
    };
    assert!(matches!(
        e.apply(&mut db, scoped_update(id, EditScope::All, to_thursday)),
        Err(EngineError::Invalid(_))
    ));
    assert_eq!(read_block(db.conn(), id.bytes()).unwrap().unwrap(), before);
    assert!(day(&e, &db, 11).is_empty());
}

/// Deleting *this and future* ends the series before the key; at the first
/// occurrence it tombstones the block.
#[test]
fn deleting_this_and_future_ends_the_series() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::ThisAndFuture { key: key(18) },
        },
    )
    .unwrap();
    assert_eq!(day(&e, &db, 11).len(), 1);
    assert!(day(&e, &db, 18).is_empty());
    assert!(day(&e, &db, 25).is_empty());
    assert!(read_block(db.conn(), split_block_id(&id, &key(18)).bytes())
        .unwrap()
        .is_none());

    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::ThisAndFuture { key: key(4) },
        },
    )
    .unwrap();
    assert!(read_block(db.conn(), id.bytes()).unwrap().unwrap().deleted);
    assert!(day(&e, &db, 4).is_empty());
}

/// Two devices that split one series at the same occurrence derive the same
/// continuation id, so after they sync there is one continuation, its
/// fields merged, and the week shows one standup.
#[test]
fn two_devices_splitting_at_the_same_key_converge_on_one_block() {
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
    let sync = |dba: &mut Db, dbb: &mut Db| {
        for env in all_envelopes(dba) {
            let _ = eb.apply_remote(dbb, &env);
        }
        for env in all_envelopes(dbb) {
            let _ = ea.apply_remote(dba, &env);
        }
    };
    sync(&mut dba, &mut dbb);
    let id = ea
        .apply(
            &mut dba,
            Command::CreateBlock(block_draft(9, 1, Some("Standup"))),
        )
        .unwrap()
        .entity;
    ea.require_features(&mut dba, &[BLOCK_RECURRENCE_FEATURE], false)
        .unwrap();
    set_clock(&ca, T0 + 1_000);
    recur(&ea, &mut dba, id, "FREQ=WEEKLY");
    sync(&mut dba, &mut dbb);

    let split = |k: &str| EditScope::ThisAndFuture { key: k.to_owned() };
    set_clock(&ca, T0 + 2_000);
    let on_a = ea
        .apply(
            &mut dba,
            scoped_update(id, split(&key(18)), retitle("Retro")),
        )
        .unwrap()
        .entity;
    set_clock(&cb, T0 + 3_000);
    let on_b = eb
        .apply(
            &mut dbb,
            scoped_update(id, split(&key(18)), retitle("Review")),
        )
        .unwrap()
        .entity;
    assert_eq!(on_a, on_b);
    sync(&mut dba, &mut dbb);
    sync(&mut dba, &mut dbb);

    for (e, db) in [(&ea, &dba), (&eb, &dbb)] {
        assert_eq!(
            day(e, db, 25),
            vec![(on_a, Some("Review".into()), Some(key(25)))],
            "the later title wins, on one block"
        );
        assert_eq!(day(e, db, 11).len(), 1);
        let continuations: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM blocks WHERE split_from = ? AND deleted = 0",
                [&id.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(continuations, 1);
    }
    assert_eq!(
        read_block(dba.conn(), on_a.bytes()).unwrap(),
        read_block(dbb.conn(), on_a.bytes()).unwrap()
    );
    assert_eq!(
        read_block(dba.conn(), id.bytes()).unwrap(),
        read_block(dbb.conn(), id.bytes()).unwrap()
    );
    ea.assert_rebuild_identity(&dba);
    eb.assert_rebuild_identity(&dbb);
}

/// Give block `id` an import's `source` and `external_id`, as an import of
/// a recurring item writes them.
fn imported_from(e: &Engine, db: &mut Db, id: EntityRef) {
    let b = Block {
        source: Some(sunrise_domain::BlockSource::ImportIcs),
        external_id: Some("standup@example.com".into()),
        ..read_block(db.conn(), id.bytes()).unwrap().unwrap()
    };
    let stream = crate::engine::ids::domain_bytes(b.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(b));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .unwrap();
}

/// Bind `task` to the whole of recurring block `id`, as a build before
/// occurrence bindings (#1162) did: `BindTask` now names an occurrence.
pub(super) fn bound_to_the_series(e: &Engine, db: &mut Db, id: EntityRef, task: EntityRef) {
    let mut b = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    b.tasks.insert(task);
    let stream = crate::engine::ids::domain_bytes(b.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(b));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .unwrap();
}

/// A split leaves the whole-block task bindings and the import fields on
/// the block they were made on: the continuation binds no task to the
/// whole series and is no external item's, and it keeps the title the
/// series showed (time-blocks.md §Recurring blocks).
#[test]
fn a_split_continuation_takes_no_bindings_and_no_import_fields() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");
    bound_to_the_series(&e, &mut db, id, task);
    imported_from(&e, &mut db, id);

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

    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert!(continuation.tasks.is_empty(), "no binding is copied");
    assert_eq!(continuation.source, None);
    assert_eq!(continuation.external_id, None);
    assert_eq!(continuation.title.as_deref(), Some("Standup"));
    assert!(!continuation.title_track_task);

    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(ended.tasks, BTreeSet::from([task]), "the original keeps it");
    assert_eq!(ended.source, Some(sunrise_domain::BlockSource::ImportIcs));
    assert_eq!(ended.external_id.as_deref(), Some("standup@example.com"));

    // The task lists the original block only, and only its rows bind it.
    let bound = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(bound.blocks, BTreeSet::from([id]));
    let week_of = |d: i8| day_rows(&e, &db, march(d)).remove(0);
    assert_eq!(week_of(11).task_titles, vec!["Prepare notes".to_string()]);
    assert!(week_of(25).task_titles.is_empty());
    e.assert_rebuild_identity(&db);
}

/// A split continuation of an untitled block bound to one task shows the
/// task's title, as the series did, without tracking it.
#[test]
fn a_split_continuation_keeps_the_title_a_bound_task_lent_the_series() {
    let mut db = db();
    let e = engine();
    legacy_peer(&db);
    let task = new_task(&e, &mut db, "Write the report");
    let id = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                title_track_task: true,
                tasks: vec![task],
                ..block_draft(9, 1, None)
            }),
        )
        .unwrap()
        .entity;
    e.require_features(&mut db, &[BLOCK_RECURRENCE_FEATURE], true)
        .unwrap();
    recur(&e, &mut db, id, "FREQ=WEEKLY");
    let next = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                BlockPatch {
                    ends_at: Some(at(18, 11)),
                    ..BlockPatch::default()
                },
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Write the report".into()), Some(key(25)))]
    );
    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert!(continuation.tasks.is_empty());
    assert!(!continuation.title_track_task);
}

/// A split names what it wrote: the continuation made and the series
/// ended. A continuation tombstoned before is written back, an update.
#[test]
fn a_split_names_the_continuation_it_made_and_the_block_it_ended() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let split = |db: &mut Db, title: &str| {
        e.apply(
            db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                retitle(title),
            ),
        )
        .unwrap()
    };
    let res = split(&mut db, "Retro");
    let next = split_block_id(&id, &key(18));
    assert_eq!(
        res.events,
        vec![DomainEvent::Created(next), DomainEvent::Updated(id)]
    );

    e.apply(
        &mut db,
        Command::DeleteBlock {
            id: next,
            scope: EditScope::All,
        },
    )
    .unwrap();
    // A concurrent edit of the whole series that won its rule back, so the
    // key is an occurrence of it again.
    recur(&e, &mut db, id, "FREQ=WEEKLY");
    let res = split(&mut db, "Review");
    assert_eq!(
        res.events,
        vec![DomainEvent::Updated(next), DomainEvent::Updated(id)]
    );
    assert!(
        !read_block(db.conn(), next.bytes())
            .unwrap()
            .unwrap()
            .deleted
    );
}

/// A *this and future* delete names the block deleted when it tombstoned
/// it (at the first occurrence) and leaves the default, an update, when it
/// only ended the series. An edit that is one of the whole block names
/// nothing of its own.
#[test]
fn a_this_and_future_delete_names_the_tombstone_it_wrote() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let end = |db: &mut Db, k: String| {
        e.apply(
            db,
            Command::DeleteBlock {
                id,
                scope: EditScope::ThisAndFuture { key: k },
            },
        )
        .unwrap()
    };
    let res = end(&mut db, key(18));
    assert!(res.events.is_empty());
    assert!(!read_block(db.conn(), id.bytes()).unwrap().unwrap().deleted);

    let res = e
        .apply(
            &mut db,
            scoped_update(id, EditScope::ThisAndFuture { key: key(4) }, retitle("x")),
        )
        .unwrap();
    assert!(res.events.is_empty());

    let res = end(&mut db, key(4));
    assert_eq!(res.events, vec![DomainEvent::Deleted(id)]);
    assert!(read_block(db.conn(), id.bytes()).unwrap().unwrap().deleted);
}

/// A series this build cannot expand shows no row, not even its anchor's,
/// and the query still succeeds with the other blocks (decision 7 of #1164,
/// ADR-0045 §6).
#[test]
fn a_series_that_cannot_be_expanded_yields_no_rows_and_no_anchor() {
    let mut db = db();
    let e = engine();
    legacy_peer(&db);
    let utc = jiff::tz::TimeZone::UTC;
    let id = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                starts_at: SunriseTime::instant(at(4, 9).to_instant(&utc).unwrap()),
                ends_at: SunriseTime::instant(at(4, 10).to_instant(&utc).unwrap()),
                ..block_draft(9, 1, Some("Pinned"))
            }),
        )
        .unwrap()
        .entity;
    let once = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(14, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    assert_eq!(day(&e, &db, 4).len(), 2, "a one-off instant block shows");
    e.require_features(&mut db, &[BLOCK_RECURRENCE_FEATURE], true)
        .unwrap();
    // Another device's rule on an instant anchor, which has no wall-clock
    // time to repeat, so `expand_block` refuses it.
    recur(&e, &mut db, id, "FREQ=WEEKLY");
    let series = read_block(db.conn(), id.bytes())
        .unwrap()
        .unwrap()
        .series()
        .expect("a recurring block");
    let window = (
        jiff::Timestamp::from_millisecond(march(1).cast_signed()).unwrap(),
        jiff::Timestamp::from_millisecond(march(28).cast_signed()).unwrap(),
    );
    assert!(sunrise_domain::expand_block(&series, window, &utc).is_err());

    assert_eq!(day(&e, &db, 4), vec![(once, Some("Gym".into()), None)]);
    assert!(day(&e, &db, 11).is_empty());
}

/// Block reminders are planned per occurrence: a weekly block fires once a
/// week in the horizon, the block lead before each occurrence's start, and a
/// cancelled week fires nothing.
#[test]
fn a_recurring_block_is_reminded_once_per_occurrence() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::This { key: key(11) },
        },
    )
    .unwrap();
    let now = march(1);
    let plan = e
        .reminder_plan(
            &db,
            now,
            now + 28 * 86_400_000,
            &sunrise_domain::ReminderSettings::default(),
        )
        .unwrap();
    let fires: Vec<_> = plan
        .intents
        .iter()
        .filter(|i| i.entity == id)
        .map(|i| (i.kind, i.fire_at, i.title.clone()))
        .collect();
    let utc = jiff::tz::TimeZone::UTC;
    let lead = jiff::SignedDuration::from_secs(i64::from(sunrise_domain::BLOCK_DEFAULT_LEAD_S));
    let expected: Vec<_> = [4, 18, 25]
        .into_iter()
        .map(|d| {
            (
                sunrise_domain::ReminderKind::Block,
                at(d, 9).to_instant(&utc).unwrap() - lead,
                "Standup".to_string(),
            )
        })
        .collect();
    assert_eq!(fires, expected);
}

/// Upcoming lists a recurring block on each day it occurs, read at that
/// occurrence, and on no other day.
#[test]
fn upcoming_lists_a_recurring_block_on_each_day_it_occurs() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        scoped_update(id, EditScope::This { key: key(11) }, retitle("Planning")),
    )
    .unwrap();
    let QueryResult::Upcoming(v) = e
        .query(
            &db,
            Query::Upcoming {
                now_ms: march(3),
                span_days: Some(14),
                contexts: vec![],
            },
        )
        .unwrap()
    else {
        panic!("expected Upcoming");
    };
    let shown: Vec<_> = v
        .days
        .iter()
        .flat_map(|d| {
            d.blocks
                .iter()
                .map(move |b| (d.date, b.title.clone(), b.occurrence_key.clone()))
        })
        .collect();
    let on = |d: i8| jiff::civil::date(2026, 3, d);
    assert_eq!(
        shown,
        vec![
            (on(4), Some("Standup".into()), Some(key(4))),
            (on(11), Some("Planning".into()), Some(key(11))),
        ]
    );
}
