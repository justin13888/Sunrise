//! The inverses [`invert`] reads off the rows before a write: edits, moves,
//! completions, defers, deletes, triage, and the batches they come in.

use super::creates::deletes;
use super::patch_fields::{apply, day, touched};
use super::*;
use crate::undo::inverse::delete_of;
use sunrise_domain::{
    ContextPatch, Energy, RRule, RoutinePatch, SavedViewPatch, StreamColor, StreamPatch,
    TaskTemplate,
};

/// A saved view's create is undone by deleting it, a delete by a restore,
/// and an edit by writing back the fields it touched, read off the view.
#[test]
fn saved_view_commands_invert() {
    use sunrise_domain::{SavedViewDraft, ViewFilter, ViewKind, ViewSort};
    struct Views(SavedView);
    impl EntityLookup for Views {
        fn task(&self, _: EntityRef) -> Option<&Task> {
            None
        }
        fn stream(&self, _: EntityRef) -> Option<&StreamRow> {
            None
        }
        fn context(&self, _: EntityRef) -> Option<&ContextRow> {
            None
        }
        fn routine(&self, _: EntityRef) -> Option<&RoutineRow> {
            None
        }
        fn saved_view(&self, id: EntityRef) -> Option<&SavedView> {
            (id == self.0.id).then_some(&self.0)
        }
    }
    let id = EntityRef::new(EntityKind::SavedView, [7; 16]);
    let held = Views(SavedView {
        id,
        created_at: jiff::Timestamp::UNIX_EPOCH,
        updated_at: jiff::Timestamp::UNIX_EPOCH,
        name: "Old".into(),
        view: ViewKind::Today,
        filter: ViewFilter::default(),
        sort: ViewSort::Title,
        sort_order: Some("N".into()),
        deleted: false,
        unknown: Unknowns::new(),
    });
    let create = Command::CreateSavedView(SavedViewDraft {
        name: "x".into(),
        view: ViewKind::Today,
        filter: ViewFilter::default(),
        sort: ViewSort::Default,
    });
    assert!(is_create(&create));
    assert!(matches!(
        invert_create(&create, id),
        Ok(Command::DeleteSavedView(d)) if d == id
    ));
    assert!(matches!(
        invert(&held, &[Command::DeleteSavedView(id)]).unwrap()[..],
        [Command::Restore(r)] if r == id
    ));
    assert!(matches!(
        invert(&held, &[Command::Restore(id)]).unwrap()[..],
        [Command::DeleteSavedView(r)] if r == id
    ));
    let edit = Command::UpdateSavedView {
        id,
        patch: SavedViewPatch {
            name: Some("New".into()),
            ..SavedViewPatch::default()
        },
    };
    match &invert(&held, &[edit]).unwrap()[..] {
        [Command::UpdateSavedView { patch, .. }] => {
            assert_eq!(patch.name.as_deref(), Some("Old"));
            assert!(patch.sort.is_none(), "only what the edit touched");
        }
        other => panic!("{other:?}"),
    }
}

/// A move is undone by putting back the keys it replaced: one task's as
/// one update, several as one bulk command, and an unplaced task goes
/// back to unplaced.
#[test]
fn a_move_inverts_to_the_keys_it_replaced() {
    let mut placed = task(1);
    placed.sort_order = Some("G".into());
    let rows = Rows {
        tasks: vec![placed, task(2)],
        ..Rows::default()
    };
    let one = Command::MoveTasks {
        ids: vec![tid(1)],
        after: None,
        before: None,
    };
    match &invert(&rows, &[one]).expect("invertible")[..] {
        [Command::UpdateTask { id, patch }] => {
            assert_eq!(*id, tid(1));
            assert_eq!(patch.sort_order, Some(Some("G".into())));
            assert!(patch.title.is_none(), "nothing else is touched");
        }
        other => panic!("{other:?}"),
    }
    let both = Command::MoveTasks {
        ids: vec![tid(2), tid(1)],
        after: None,
        before: None,
    };
    match &invert(&rows, &[both]).expect("invertible")[..] {
        [Command::BulkTasks(items)] => {
            let keys: Vec<_> = items
                .iter()
                .map(|i| match &i.action {
                    sunrise_core::BulkTaskAction::Update(p) => (i.id, p.sort_order.clone()),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(
                keys,
                vec![(tid(2), Some(None)), (tid(1), Some(Some("G".into())))]
            );
        }
        other => panic!("{other:?}"),
    }
    let missing = Command::MoveTasks {
        ids: vec![tid(9)],
        after: None,
        before: None,
    };
    assert_eq!(
        invert(&rows, &[missing]).unwrap_err(),
        NotUndoable::Unsupported
    );
}

#[test]
fn completing_inverts_to_the_state_the_task_was_in() {
    let mut t = task(1);
    t.state = TaskState::InProgress;
    let rows = rows_with(t);
    let back = invert(&rows, &[Command::CompleteTask(tid(1))]).expect("invertible");
    match &back[..] {
        [Command::UpdateTask { id, patch }] => {
            assert_eq!(*id, tid(1));
            assert_eq!(patch.state, Some(TaskState::InProgress));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn restoring_a_finished_task_lands_on_todo_not_in_progress() {
    // `Task.state` does not record how far along something was before it
    // finished, so restoring `InProgress` would be inventing a claim.
    // `Todo` is the honest reading: it is back on the list.
    let mut t = task(1);
    t.state = TaskState::Done;
    let rows = rows_with(t);
    let forward = Command::UpdateTask {
        id: tid(1),
        patch: TaskPatch {
            state: Some(TaskState::InProgress),
            ..TaskPatch::default()
        },
    };
    let back = invert(&rows, &[forward]).expect("invertible");
    match &back[..] {
        [Command::UpdateTask { patch, .. }] => {
            assert_eq!(patch.state, Some(TaskState::Todo));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_patch_inverts_only_the_fields_it_touched() {
    let mut t = task(1);
    t.priority = Some(2);
    t.energy = Some(Energy::High);
    let rows = rows_with(t);
    let forward = Command::UpdateTask {
        id: tid(1),
        patch: TaskPatch {
            priority: Some(Some(5)),
            ..TaskPatch::default()
        },
    };
    let back = invert(&rows, &[forward]).expect("invertible");
    match &back[..] {
        [Command::UpdateTask { patch, .. }] => {
            assert_eq!(patch.priority, Some(Some(2)), "restored");
            assert_eq!(patch.energy, None, "untouched fields stay untouched");
            assert_eq!(patch.title, None);
        }
        other => panic!("{other:?}"),
    }
}

// ---- triage actions ----

/// Two tasks holding different values in each field a triage action
/// writes, so an inverse that read the wrong task's row shows.
fn triage_rows() -> Rows {
    let first = task(1);
    let mut second = task(2);
    second.planned_at = Some(day(1));
    second.state = TaskState::InProgress;
    second.late_acknowledged_at = Some(jiff::Timestamp::UNIX_EPOCH);
    Rows {
        tasks: vec![first, second],
        ..Rows::default()
    }
}

/// The triage, over the same tasks in both orders.
fn both_orders() -> [Vec<EntityRef>; 2] {
    [vec![tid(1), tid(2)], vec![tid(2), tid(1)]]
}

/// The inverse of `Triage { ids, action }`, as `(task, patch)` in the
/// order it is applied.
fn triage_inverse(
    rows: &Rows,
    ids: &[EntityRef],
    action: sunrise_domain::TriageAction,
) -> Vec<(EntityRef, TaskPatch)> {
    let back = invert(
        rows,
        &[Command::Triage {
            ids: ids.to_vec(),
            action,
        }],
    )
    .expect("invertible");
    match back.as_slice() {
        [Command::BulkTasks(items)] => items
            .iter()
            .map(|i| match &i.action {
                sunrise_core::BulkTaskAction::Update(p) => (i.id, (**p).clone()),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// Checks that the inverse of `action` over each order of the two tasks
/// unwinds them in reverse, writing `field` alone, and that applying it
/// to each task after the action puts the task back.
fn check_triage_inverse(
    action: &sunrise_domain::TriageAction,
    field: &str,
    after: fn(&Task) -> Task,
) {
    let rows = triage_rows();
    for ids in both_orders() {
        let back = triage_inverse(&rows, &ids, action.clone());
        let order: Vec<_> = back.iter().map(|(id, _)| *id).collect();
        let reversed: Vec<_> = ids.iter().rev().copied().collect();
        assert_eq!(
            order, reversed,
            "{action:?} over {ids:?} unwinds in reverse"
        );
        for (id, patch) in &back {
            assert_eq!(touched(patch), [field], "{action:?} on {id:?}");
            let held = rows.task(*id).expect("held");
            assert_eq!(
                &apply(&after(held), patch),
                held,
                "{action:?} on {id:?} is undone"
            );
        }
    }
}

#[test]
fn a_triage_defer_inverts_to_each_tasks_own_plan() {
    check_triage_inverse(
        &sunrise_domain::TriageAction::Defer {
            to: sunrise_domain::DeferTarget::NextFreeSlot,
        },
        "planned_at",
        |t| {
            let mut t = t.clone();
            // The deferral count is left out: the inverse keeps it, on
            // purpose, which `a_defer_restores_the_date_and_leaves_the_counter_alone` pins.
            t.planned_at = Some(day(20));
            t
        },
    );
}

#[test]
fn a_triage_already_done_inverts_to_each_tasks_own_state() {
    check_triage_inverse(
        &sunrise_domain::TriageAction::AlreadyDone {
            completed_at: day(1),
        },
        "state",
        |t| {
            let mut t = t.clone();
            t.state = TaskState::Done;
            t
        },
    );
}

#[test]
fn a_triage_drop_inverts_to_each_tasks_own_state() {
    check_triage_inverse(
        &sunrise_domain::TriageAction::Drop {
            reason: Some("no longer needed".into()),
        },
        "state",
        |t| {
            let mut t = t.clone();
            t.state = TaskState::Cancelled;
            t
        },
    );
}

#[test]
fn a_triage_keep_inverts_to_each_tasks_own_acknowledgement() {
    check_triage_inverse(
        &sunrise_domain::TriageAction::Keep,
        "late_acknowledged_at",
        |t| {
            let mut t = t.clone();
            t.late_acknowledged_at =
                Some(jiff::Timestamp::UNIX_EPOCH + jiff::SignedDuration::from_secs(3600));
            t
        },
    );
}

#[test]
fn a_triage_over_a_task_the_client_is_not_holding_is_refused() {
    let rows = triage_rows();
    let triage = Command::Triage {
        ids: vec![tid(1), tid(9)],
        action: sunrise_domain::TriageAction::Keep,
    };
    assert_eq!(
        invert(&rows, &[triage]).unwrap_err(),
        NotUndoable::Unsupported
    );
}

#[test]
fn a_defer_restores_the_date_and_leaves_the_counter_alone() {
    // `deferred_count` is a PN-counter with a history a review reads; undo
    // is a new write, not a rollback.
    let mut t = task(1);
    t.deferred_count = 3;
    let rows = rows_with(t);
    let back = invert(
        &rows,
        &[Command::DeferTask {
            id: tid(1),
            to_ms: 1,
        }],
    )
    .expect("invertible");
    match &back[..] {
        [Command::UpdateTask { patch, .. }] => {
            assert_eq!(patch.planned_at, Some(None), "the date comes back");
        }
        other => panic!("{other:?}"),
    }
}

/// A defer to a time of any kind undoes the same way, restoring the
/// previous value with its own kind.
#[test]
fn a_defer_to_a_floating_time_restores_the_previous_one() {
    let was = sunrise_domain::SunriseTime::all_day(jiff::civil::date(2026, 6, 1));
    let mut t = task(1);
    t.planned_at = Some(was.clone());
    let rows = rows_with(t);
    let back = invert(
        &rows,
        &[Command::DeferTaskTo {
            id: tid(1),
            to: sunrise_domain::SunriseTime::floating(jiff::civil::date(2026, 6, 2).at(9, 0, 0, 0)),
        }],
    )
    .expect("invertible");
    match &back[..] {
        [Command::UpdateTask { patch, .. }] => {
            assert_eq!(patch.planned_at, Some(Some(was)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_move_between_streams_inverts_to_where_it_came_from() {
    let rows = rows_with(task(1));
    let back = invert(
        &rows,
        &[Command::PromoteToStream {
            id: tid(1),
            stream: sid(9),
        }],
    )
    .expect("invertible");
    assert!(matches!(
        back[..],
        [Command::PromoteToStream { stream, .. }] if stream == sid(1)
    ));
}

#[test]
fn undo_of_a_delete_is_a_restore() {
    // No row is read: the restore brings back whatever the entity was.
    let rows = Rows::default();
    for id in [
        tid(1),
        sid(1),
        EntityRef::new(EntityKind::Context, [3; 16]),
        EntityRef::new(EntityKind::Routine, [4; 16]),
        EntityRef::new(EntityKind::Block, [5; 16]),
    ] {
        let delete = delete_of(id).expect("a kind with a delete");
        let back = invert(&rows, std::slice::from_ref(&delete)).expect("invertible");
        assert!(
            matches!(back[..], [Command::Restore(r)] if r == id),
            "{back:?}"
        );
        // And the restore undoes back into the same delete.
        let again = invert(&rows, &back).expect("invertible");
        assert!(
            matches!(&again[..], [d] if deletes(d) == Some(id)),
            "{again:?}"
        );
    }
}

#[test]
fn a_batch_with_an_uninvertible_command_is_refused_whole() {
    // All or nothing: one un-invertible command sinks the whole batch,
    // because a half-undone bulk operation is worse than one that refuses.
    let rows = rows_with(task(1));
    let resolve = Command::ResolveConflict {
        entity: tid(1),
        field: "title".into(),
        value: serde_json::from_value(serde_json::json!("x")).expect("a CBOR text"),
    };
    assert_eq!(
        invert(&rows, &[Command::CompleteTask(tid(1)), resolve]).unwrap_err(),
        NotUndoable::Unsupported
    );
    assert_eq!(
        invert(
            &rows,
            &[Command::DetachFile(EntityRef::new(
                EntityKind::Attachment,
                [7; 16]
            ))]
        )
        .unwrap_err(),
        NotUndoable::Unsupported,
        "an attachment has no restore"
    );
}

#[test]
fn a_row_the_client_is_not_holding_cannot_be_restored() {
    let rows = Rows::default();
    assert_eq!(
        invert(&rows, &[Command::CompleteTask(tid(1))]).unwrap_err(),
        NotUndoable::Unsupported
    );
}

#[test]
fn a_batch_unwinds_in_the_order_it_was_wound() {
    let rows = Rows {
        tasks: vec![task(1), task(2)],
        ..Rows::default()
    };
    let back = invert(
        &rows,
        &[Command::CompleteTask(tid(1)), Command::CompleteTask(tid(2))],
    )
    .expect("invertible");
    assert!(matches!(&back[0], Command::UpdateTask { id, .. } if *id == tid(2)));
    assert!(matches!(&back[1], Command::UpdateTask { id, .. } if *id == tid(1)));
}

#[test]
fn stream_context_and_routine_edits_invert_from_their_rows() {
    let rows = Rows {
        streams: vec![StreamRow {
            id: sid(1),
            name: "Travel".into(),
            color: StreamColor::Slate,
            open_task_count: 0,
            archived: false,
            paused: false,
            sort_order: "N".into(),
        }],
        contexts: vec![ContextRow {
            id: cid(1),
            name: "home".into(),
            description: None,
            archived: false,
            task_count: 0,
        }],
        routines: vec![RoutineRow {
            id: rid(1),
            title: "Water plants".into(),
            rrule: "every day".into(),
            next: None,
            paused: false,
            template: TaskTemplate {
                title: "Water plants".into(),
                stream_id: Some(sid(1)),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: sunrise_domain::Unknowns::new(),
            },
            rule: RRule::parse("FREQ=DAILY").expect("valid rrule"),
            streak: 0,
        }],
        ..Rows::default()
    };

    let back = invert(
        &rows,
        &[Command::UpdateStream {
            id: sid(1),
            patch: StreamPatch {
                name: Some("Renamed".into()),
                ..StreamPatch::default()
            },
        }],
    )
    .expect("invertible");
    assert!(matches!(
        &back[..],
        [Command::UpdateStream { patch, .. }] if patch.name.as_deref() == Some("Travel")
    ));

    // A reorder is undoable for the same reason it is cheap: the old key
    // is on the row the client is already holding, and putting it back
    // touches no sibling.
    let back = invert(
        &rows,
        &[Command::UpdateStream {
            id: sid(1),
            patch: StreamPatch {
                sort_order: Some("ZN".into()),
                ..StreamPatch::default()
            },
        }],
    )
    .expect("invertible");
    assert!(matches!(
        &back[..],
        [Command::UpdateStream { patch, .. }] if patch.sort_order.as_deref() == Some("N")
    ));

    let back = invert(
        &rows,
        &[Command::UpdateContext {
            id: cid(1),
            patch: ContextPatch {
                archived: Some(true),
                ..ContextPatch::default()
            },
        }],
    )
    .expect("invertible");
    assert!(matches!(
        &back[..],
        [Command::UpdateContext { patch, .. }] if patch.archived == Some(false)
    ));

    let back = invert(
        &rows,
        &[Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: rid(1),
            patch: RoutinePatch {
                paused: Some(true),
                ..RoutinePatch::default()
            },
        }],
    )
    .expect("invertible");
    assert!(matches!(
        &back[..],
        [Command::UpdateRoutine { patch, .. }] if patch.paused == Some(false)
    ));
}
