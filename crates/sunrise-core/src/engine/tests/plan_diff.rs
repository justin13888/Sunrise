//! The planner's seam: a preview is solved from the vault, and a commit
//! writes exactly the previewed diff or refuses (`docs/08-features/planner.md`,
//! #343).

use super::*;
use crate::engine::PlanError;
use crate::inner_op::InnerOp;
use sunrise_domain::planner::{PlanIntent, PlanPosition, SlotTarget};
use sunrise_domain::{BlockKind, BlockSource};

const NOW_MS: u64 = 1_700_000_000_000;

fn at(h: i8, m: i8) -> SunriseTime {
    SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(h, m, 0, 0))
}

fn planned_task(e: &Engine, db: &mut Db, title: &str, start: SunriseTime, min: u64) -> EntityRef {
    task_with(
        e,
        db,
        title,
        TaskDraft {
            planned_at: Some(start),
            estimated_duration_s: Some(min * 60),
            ..Default::default()
        },
    )
}

fn block(e: &Engine, db: &mut Db, hour: i8) -> EntityRef {
    e.apply(
        db,
        Command::CreateBlock(block_draft(hour, 1, Some("standup"))),
    )
    .unwrap()
    .entity
}

fn read_block_row(db: &Db, id: EntityRef) -> sunrise_domain::Block {
    super::super::block::read_block(db.conn(), id.bytes())
        .unwrap()
        .unwrap()
}

/// A block from `start` to `end` on the test date, rewritten to carry `kind`
/// and `source` as a newer writer or an importer would.
fn block_of_kind(
    e: &Engine,
    db: &mut Db,
    start: SunriseTime,
    end: SunriseTime,
    kind: Option<BlockKind>,
    source: Option<BlockSource>,
) -> EntityRef {
    let mut draft = block_draft(9, 1, Some("held"));
    draft.starts_at = start;
    draft.ends_at = end;
    let id = e.apply(db, Command::CreateBlock(draft)).unwrap().entity;
    let block = sunrise_domain::Block {
        kind,
        source,
        ..read_block_row(db, id)
    };
    let stream = crate::engine::ids::domain_bytes(block.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(block));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .unwrap();
    id
}

/// Moves the block at 09:00 onto 11:00, where `other` sits, and returns
/// whether the diff moves `other`.
fn moving_onto_moves(kind: Option<BlockKind>, source: Option<BlockSource>) -> bool {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 9);
    let other = block_of_kind(&e, &mut db, at(11, 0), at(12, 0), kind, source);
    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    assert!(diff.moves.iter().any(|m| m.item == b), "{diff:#?}");
    diff.moves.iter().any(|m| m.item == other)
}

fn move_to_eleven(b: EntityRef) -> PlanIntent {
    PlanIntent::MoveBlock {
        block: b,
        to_start: at(11, 0),
    }
}

#[test]
fn a_committed_preview_writes_exactly_its_moves_and_previews_again_as_empty() {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 9);
    let t = planned_task(&e, &mut db, "write", at(11, 0), 30);

    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    assert_eq!(diff.moves.len(), 2, "{diff:#?}");
    assert!(diff.committable);
    let moved = e.plan_apply(&mut db, &diff, NOW_MS).unwrap().0;
    assert_eq!(moved, 2);

    let written = read_block_row(&db, b);
    assert_eq!(written.starts_at, at(11, 0));
    assert_eq!(written.ends_at, at(12, 0));
    // The task kept its floating kind and took the nearest free slot.
    assert_eq!(task_of(&e, &db, t).planned_at, Some(at(10, 30)));

    let again = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    assert!(again.is_empty(), "{again:#?}");
}

#[test]
fn a_changed_input_refuses_the_commit_as_stale_and_writes_nothing() {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 9);
    let t = planned_task(&e, &mut db, "write", at(11, 0), 30);
    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();

    e.apply(
        &mut db,
        Command::UpdateTask {
            id: t,
            patch: TaskPatch {
                estimated_duration_s: Some(Some(45 * 60)),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let ops = op_count(&db);
    assert!(matches!(
        e.plan_apply(&mut db, &diff, NOW_MS),
        Err(PlanError::Stale)
    ));
    assert_eq!(op_count(&db), ops, "a stale commit wrote nothing");
    assert_eq!(read_block_row(&db, b).starts_at, at(9, 0));
}

#[test]
fn a_bulk_defer_counts_a_deferral_on_each_task_and_moves_nothing_else() {
    let e = engine();
    let mut db = db();
    let a = planned_task(&e, &mut db, "a", at(9, 0), 30);
    let c = planned_task(&e, &mut db, "c", at(10, 0), 30);
    let to = jiff::civil::date(2026, 3, 5);
    let intent = PlanIntent::BulkDefer {
        tasks: vec![a, c],
        to,
        place: false,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert_eq!(e.plan_apply(&mut db, &diff, NOW_MS).unwrap().0, 2);
    for id in [a, c] {
        let task = task_of(&e, &db, id);
        assert_eq!(task.planned_at, Some(SunriseTime::all_day(to)));
        assert_eq!(task.deferred_count, 1);
        assert_eq!(task.hard_due_at, None);
    }
}

#[test]
fn a_task_moved_to_a_day_is_committed_date_only_and_not_deferred() {
    let e = engine();
    let mut db = db();
    let t = planned_task(&e, &mut db, "write", at(9, 0), 30);
    let to = jiff::civil::date(2026, 3, 6);
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::Day(to),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert_eq!(diff.moves.len(), 1, "{diff:#?}");
    assert_eq!(diff.moves[0].after, PlanPosition::Day(to));
    assert!(diff.committable);
    assert_eq!(e.plan_apply(&mut db, &diff, NOW_MS).unwrap().0, 1);
    let task = task_of(&e, &db, t);
    assert_eq!(task.planned_at, Some(SunriseTime::all_day(to)));
    // Only a bulk defer counts a deferral.
    assert_eq!(task.deferred_count, 0);
    assert!(e.plan_preview(&db, &intent, NOW_MS).unwrap().is_empty());
}

#[test]
fn a_placing_bulk_defer_writes_a_slot_on_the_day_and_counts_a_deferral() {
    let e = engine();
    let mut db = db();
    let a = planned_task(&e, &mut db, "a", at(9, 0), 30);
    let c = planned_task(&e, &mut db, "c", at(10, 0), 30);
    let to = jiff::civil::date(2026, 3, 5);
    let intent = PlanIntent::BulkDefer {
        tasks: vec![a, c],
        to,
        place: true,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert_eq!(diff.moves.len(), 2, "{diff:#?}");
    assert!(diff.unscheduled.is_empty(), "{diff:#?}");
    assert_eq!(e.plan_apply(&mut db, &diff, NOW_MS).unwrap().0, 2);
    let utc = jiff::tz::TimeZone::UTC;
    let mut starts = Vec::new();
    for id in [a, c] {
        let moved = diff.moves.iter().find(|m| m.item == id).unwrap();
        let PlanPosition::Slot { start, .. } = &moved.after else {
            panic!("{diff:#?}");
        };
        let task = task_of(&e, &db, id);
        // Exactly the previewed slot, written through the deferral.
        assert_eq!(task.planned_at.as_ref(), Some(start));
        assert_eq!(task.deferred_count, 1);
        let instant = start.resolve_in(&utc).unwrap();
        assert_eq!(instant.to_zoned(utc.clone()).date(), to);
        starts.push(instant);
    }
    // In the order named: `c` takes the next free slot after `a`'s.
    assert!(starts[0] + jiff::SignedDuration::from_mins(30) <= starts[1]);
}

#[test]
fn a_drop_outside_a_hard_window_is_refused_at_commit() {
    let e = engine();
    let mut db = db();
    let t = task_with(
        &e,
        &mut db,
        "mornings only",
        TaskDraft {
            planned_at: Some(at(9, 0)),
            scheduling_constraints: vec![ScheduleConstraint {
                time_of_day: Some(sunrise_domain::TimeOfDayRange::new(
                    jiff::civil::time(8, 0, 0, 0),
                    jiff::civil::time(12, 0, 0, 0),
                )),
                days_of_week: sunrise_domain::WeekdaySet::default(),
                date_range: None,
                at_place: Vec::new(),
                severity: sunrise_domain::ConstraintSeverity::Hard,
                unknown: Unknowns::default(),
            }],
            ..Default::default()
        },
    );
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::At(at(15, 0)),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert!(!diff.committable);
    assert!(matches!(
        e.plan_apply(&mut db, &diff, NOW_MS),
        Err(PlanError::NotCommittable)
    ));
    assert_eq!(task_of(&e, &db, t).planned_at, Some(at(9, 0)));
}

#[test]
fn an_auto_place_slots_unplanned_tasks_into_the_first_free_time() {
    let e = engine();
    let mut db = db();
    block(&e, &mut db, 0);
    let t = task_with(&e, &mut db, "loose", TaskDraft::default());
    let intent = PlanIntent::AutoPlace {
        tasks: vec![t],
        day: jiff::civil::date(2026, 3, 4),
        not_before: None,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    let PlanPosition::Slot { start, .. } = &diff.moves[0].after else {
        panic!("{diff:#?}");
    };
    // The reader's zone is UTC; the block holds midnight to 01:00.
    assert_eq!(
        start.resolve_in(&jiff::tz::TimeZone::UTC),
        at(1, 0).resolve_in(&jiff::tz::TimeZone::UTC)
    );
    e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    assert!(task_of(&e, &db, t).planned_at.is_some());
}

/// An edit since the commit to a field the commit did not write leaves the
/// commit's fields restorable: the undo is per field, not per item.
#[test]
fn an_edit_to_another_field_since_does_not_skip_the_undo() {
    let e = engine();
    let mut db = db();
    let t = planned_task(&e, &mut db, "write", at(9, 0), 30);
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::At(at(13, 0)),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    let (_, commit) = e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: t,
            patch: TaskPatch {
                title: Some("rewrite".into()),
                ..TaskPatch::default()
            },
        },
    )
    .unwrap();

    let undone = e.plan_undo(&mut db, commit).unwrap();
    assert_eq!((undone.restored, undone.skipped.len()), (1, 0));
    let task = task_of(&e, &db, t);
    assert_eq!(task.planned_at, Some(at(9, 0)));
    assert_eq!(task.title, "rewrite");
}

/// In a vault an older device shares, each write is a full-state op, and the
/// undo restores a block and a deferral the same way.
#[test]
fn a_legacy_vault_undoes_a_block_move_and_a_deferral() {
    let e = engine();
    let mut db = db();
    legacy_peer(&db);
    let b = block(&e, &mut db, 9);
    let before = read_block_row(&db, b);
    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    let (_, moved) = e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    let undone = e.plan_undo(&mut db, moved).unwrap();
    assert_eq!((undone.restored, undone.skipped.len()), (2, 0));
    let after = read_block_row(&db, b);
    assert_eq!(
        (after.starts_at, after.ends_at),
        (before.starts_at, before.ends_at)
    );

    let t = planned_task(&e, &mut db, "a", at(14, 0), 30);
    let intent = PlanIntent::BulkDefer {
        tasks: vec![t],
        to: jiff::civil::date(2026, 3, 5),
        place: false,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    let (_, deferred) = e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    assert_eq!(task_of(&e, &db, t).deferred_count, 1);
    let undone = e.plan_undo(&mut db, deferred).unwrap();
    assert_eq!(undone.restored, 1);
    let task = task_of(&e, &db, t);
    assert_eq!(task.planned_at, Some(at(14, 0)));
    assert_eq!(task.deferred_count, 0);

    // A full-state op writes every field, so a later edit of the title
    // alone is a write of the plan too, and the undo leaves it.
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::At(at(16, 0)),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    let (_, moved) = e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: t,
            patch: TaskPatch {
                title: Some("renamed".into()),
                ..TaskPatch::default()
            },
        },
    )
    .unwrap();
    let undone = e.plan_undo(&mut db, moved).unwrap();
    assert_eq!((undone.restored, undone.skipped.len()), (0, 1));
    assert_eq!(task_of(&e, &db, t).planned_at, Some(at(16, 0)));
}

/// A task deleted since the commit is left deleted, its field reported.
#[test]
fn an_item_deleted_since_is_skipped() {
    let e = engine();
    let mut db = db();
    let t = planned_task(&e, &mut db, "write", at(9, 0), 30);
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::At(at(13, 0)),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    let (_, commit) = e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    e.apply(&mut db, Command::DeleteTask(t)).unwrap();

    let undone = e.plan_undo(&mut db, commit).unwrap();
    assert_eq!(undone.restored, 0);
    assert_eq!(
        undone.skipped,
        vec![crate::engine::PlanUndoSkip {
            item: t,
            field: crate::engine::PlanField::PlannedAt,
        }]
    );
    assert!(undone.items.is_empty());
}

/// The planner reads each block's effective kind (#1184): a flexible block
/// ripples out of the way, a fixed one never moves.
#[test]
fn a_flexible_block_in_the_way_is_moved() {
    assert!(moving_onto_moves(Some(BlockKind::Flexible), None));
    // A Sunrise-created block with no kind reads as flexible.
    assert!(moving_onto_moves(None, None));
}

#[test]
fn a_fixed_block_in_the_way_is_not_moved() {
    assert!(!moving_onto_moves(Some(BlockKind::Fixed), None));
}

#[test]
fn an_imported_block_with_no_kind_is_not_moved() {
    assert!(!moving_onto_moves(None, Some(BlockSource::ImportIcs)));
}

/// The import path itself marks what it writes, so a block an `.ics` import
/// wrote, which states no kind, is held fixed.
#[test]
fn a_block_written_by_an_ics_import_is_not_moved() {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 9);
    let other = e
        .apply(
            &mut db,
            import(
                "ics",
                "meeting@example.com",
                block_draft(11, 1, Some("meeting")),
            ),
        )
        .unwrap()
        .entity;
    let written = read_block_row(&db, other);
    assert_eq!(written.source, Some(BlockSource::ImportIcs));
    assert_eq!(written.kind, None);
    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    assert!(diff.moves.iter().any(|m| m.item == b), "{diff:#?}");
    assert!(!diff.moves.iter().any(|m| m.item == other), "{diff:#?}");
}

/// A block already under way is fixed whatever its kind.
#[test]
fn an_in_progress_flexible_block_is_not_moved() {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 12);
    let other = block_of_kind(
        &e,
        &mut db,
        at(9, 30),
        at(10, 30),
        Some(BlockKind::Flexible),
        None,
    );
    let now_ms = at(10, 0)
        .resolve_in(&jiff::tz::TimeZone::UTC)
        .unwrap()
        .as_millisecond()
        .cast_unsigned();
    let intent = PlanIntent::MoveBlock {
        block: b,
        to_start: at(10, 0),
    };
    let diff = e.plan_preview(&db, &intent, now_ms).unwrap();
    assert!(diff.moves.iter().any(|m| m.item == b), "{diff:#?}");
    assert!(!diff.moves.iter().any(|m| m.item == other), "{diff:#?}");
}

#[test]
fn an_intent_naming_nothing_is_invalid() {
    let e = engine();
    let db = db();
    let ghost = EntityRef::new(EntityKind::Task, [7; 16]);
    let intent = PlanIntent::MoveTask {
        task: ghost,
        to: SlotTarget::At(at(9, 0)),
    };
    assert!(matches!(
        e.plan_preview(&db, &intent, NOW_MS),
        Err(PlanError::Invalid(_))
    ));
}

// ---- a recurring block (#1213, #1276) ----

mod recurring {
    use super::super::block_recurrence::{key, march, recur, scoped_update, weekly};
    use super::*;
    use crate::feature::BLOCK_OCCURRENCE_TASKS_FEATURE;
    use sunrise_domain::{EditScope, OccurrenceBinding};

    type Row = (Option<String>, Option<String>, Vec<String>);

    /// The weekly standup with week 11 cancelled, week 18 retitled "Retro",
    /// and a task bound to week 25, in a vault that requires
    /// `block.occurrence_tasks`.
    fn standup(e: &Engine, db: &mut Db) -> (EntityRef, EntityRef) {
        let id = weekly(e, db);
        e.require_features(db, &[BLOCK_OCCURRENCE_TASKS_FEATURE], true)
            .unwrap();
        e.apply(
            db,
            Command::DeleteBlock {
                id,
                scope: EditScope::This { key: key(11) },
            },
        )
        .unwrap();
        let retitle = BlockPatch {
            title: Some(Some("Retro".into())),
            ..BlockPatch::default()
        };
        e.apply(
            db,
            scoped_update(id, EditScope::This { key: key(18) }, retitle),
        )
        .unwrap();
        let task = new_task(e, db, "Demo");
        e.apply(
            db,
            Command::BindTask {
                block: id,
                task,
                occurrence: Some(key(25)),
            },
        )
        .unwrap();
        (id, task)
    }

    /// Commit the planner's move of block `id` to `to_start`.
    fn commit_move(
        e: &Engine,
        db: &mut Db,
        id: EntityRef,
        to_start: SunriseTime,
    ) -> Result<crate::engine::PlanCommitId, PlanError> {
        let intent = PlanIntent::MoveBlock {
            block: id,
            to_start,
        };
        let diff = e.plan_preview(db, &intent, NOW_MS).unwrap();
        assert!(diff.moves.iter().any(|m| m.item == id), "{diff:#?}");
        e.plan_apply(db, &diff, NOW_MS).map(|(_, commit)| commit)
    }

    /// What the day grid of 2026-03-`day` shows: each row's title, key, and
    /// the titles of the tasks listed on it.
    fn grid(e: &Engine, db: &Db, day: i8) -> Vec<Row> {
        day_rows(e, db, march(day))
            .into_iter()
            .map(|r| (r.title, r.occurrence_key, r.task_titles))
            .collect()
    }

    fn row(title: &str, key: &str, tasks: &[&str]) -> Row {
        (
            Some(title.into()),
            Some(key.into()),
            tasks.iter().map(|t| (*t).to_owned()).collect(),
        )
    }

    /// The block's exception keys and occurrence binding keys.
    fn keys(block: &sunrise_domain::Block) -> (Vec<String>, Vec<String>) {
        (
            block.exceptions.keys().cloned().collect(),
            block
                .occurrence_tasks
                .iter()
                .map(|b| b.key().to_owned())
                .collect(),
        )
    }

    /// A planner move of the series an hour later moves its exceptions and
    /// its occurrence binding with their weeks, as an edit of the whole
    /// block does; its undo moves them back to the keys they had.
    #[test]
    fn a_planner_move_carries_exceptions_and_bindings_and_its_undo_restores_them() {
        let e = engine();
        let mut db = db();
        let (id, task) = standup(&e, &mut db);
        let before = read_block_row(&db, id);

        let commit = commit_move(&e, &mut db, id, at(10, 0)).unwrap();
        assert!(grid(&e, &db, 11).is_empty(), "the cancelled week stays so");
        assert_eq!(grid(&e, &db, 18), [row("Retro", "2026-03-18T10:00", &[])]);
        assert_eq!(
            grid(&e, &db, 25),
            [row("Standup", "2026-03-25T10:00", &["Demo"])]
        );
        let moved = read_block_row(&db, id);
        assert_eq!(
            keys(&moved),
            (
                vec!["2026-03-11T10:00".into(), "2026-03-18T10:00".into()],
                vec!["2026-03-25T10:00".into()],
            )
        );
        assert_eq!(
            moved.occurrence_tasks,
            BTreeSet::from([OccurrenceBinding::new("2026-03-25T10:00", task)])
        );

        let undone = e.plan_undo(&mut db, commit).unwrap();
        assert_eq!((undone.restored, undone.skipped.len()), (2, 0));
        let after = read_block_row(&db, id);
        assert_eq!(keys(&after), keys(&before));
        assert_eq!(after.occurrence_tasks, before.occurrence_tasks);
        assert!(grid(&e, &db, 11).is_empty());
        assert_eq!(grid(&e, &db, 18), [row("Retro", &key(18), &[])]);
        assert_eq!(grid(&e, &db, 25), [row("Standup", &key(25), &["Demo"])]);
        e.assert_rebuild_identity(&db);
    }

    /// A rule that holds the weekday (`BYDAY=WE`) does not follow an anchor
    /// the planner moves to Thursday, so neither could its cancelled week:
    /// the commit is refused as invalid and writes nothing.
    #[test]
    fn a_planner_move_that_would_strand_an_exception_is_refused() {
        let e = engine();
        let mut db = db();
        let (id, _) = standup(&e, &mut db);
        recur(&e, &mut db, id, "FREQ=WEEKLY;BYDAY=WE");
        let before = read_block_row(&db, id);
        let ops = op_count(&db);

        let thursday = SunriseTime::floating(jiff::civil::date(2026, 3, 5).at(9, 0, 0, 0));
        assert!(matches!(
            commit_move(&e, &mut db, id, thursday),
            Err(PlanError::Invalid(_))
        ));
        assert_eq!(read_block_row(&db, id), before);
        assert_eq!(op_count(&db), ops, "a refused commit writes nothing");
        assert!(grid(&e, &db, 11).is_empty());
    }
}
