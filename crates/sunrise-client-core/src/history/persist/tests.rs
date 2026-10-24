use super::*;
use sunrise_id::EntityKind;

fn task(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Task, [n; 16])
}

fn entry(label: &str, forward: Vec<Command>, backward: Vec<Command>) -> UndoEntry {
    UndoEntry {
        label: label.to_owned(),
        forward,
        backward,
    }
}

fn round_trip(stacks: &UndoStacks) -> UndoStacks {
    let (json, dropped) = to_json(stacks).expect("writes");
    assert_eq!(dropped, 0, "nothing in these stacks is out of scope");
    from_json(&json).expect("reads back")
}

/// `Debug` is the comparison: `Command` has no `PartialEq`, and its `Debug`
/// prints `Some(None)` and `None` differently, which is the property under
/// test.
fn same(a: &UndoStacks, b: &UndoStacks) {
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}

#[test]
fn a_clear_survives_the_trip_as_a_clear_and_an_untouched_field_as_untouched() {
    let forward = Command::UpdateTask {
        id: task(1),
        patch: TaskPatch {
            priority: Some(Some(1)),
            ..TaskPatch::default()
        },
    };
    // The inverse of setting a priority on a task that had none.
    let backward = Command::UpdateTask {
        id: task(1),
        patch: TaskPatch {
            priority: Some(None),
            ..TaskPatch::default()
        },
    };
    let mut stacks = UndoStacks::default();
    stacks.push(entry("prioritise", vec![forward], vec![backward]));

    let back = round_trip(&stacks);
    same(&stacks, &back);
    let Command::UpdateTask { patch, .. } = &back.done[0].backward[0] else {
        panic!("an update");
    };
    assert_eq!(patch.priority, Some(None), "a clear, not a no-op");
    assert_eq!(patch.energy, None, "untouched stays untouched");
}

#[test]
fn every_patch_family_and_both_stacks_round_trip() {
    let stream = EntityRef::new(EntityKind::Stream, [2; 16]);
    let context = EntityRef::new(EntityKind::Context, [3; 16]);
    let routine = EntityRef::new(EntityKind::Routine, [4; 16]);
    let mut stacks = UndoStacks::default();
    stacks.push(entry(
        "rename",
        vec![Command::UpdateStream {
            id: stream,
            patch: StreamPatch {
                name: Some("Home".into()),
                icon: Some(None),
                ..StreamPatch::default()
            },
        }],
        vec![Command::UpdateStream {
            id: stream,
            patch: StreamPatch {
                name: Some("House".into()),
                icon: Some(Some("house".into())),
                ..StreamPatch::default()
            },
        }],
    ));
    stacks.push(entry(
        "archive",
        vec![Command::UpdateContext {
            id: context,
            patch: ContextPatch {
                archived: Some(true),
                description: Some(None),
                ..ContextPatch::default()
            },
        }],
        vec![Command::UpdateRoutine {
            id: routine,
            patch: RoutinePatch {
                paused: Some(false),
                ends_at: Some(None),
                ..RoutinePatch::default()
            },
        }],
    ));
    stacks.push(entry(
        "capture",
        vec![Command::CreateTask(TaskDraft {
            title: "Renew passport".into(),
            ..TaskDraft::default()
        })],
        vec![Command::DeleteTask(task(9))],
    ));
    stacks.push(entry(
        "drop",
        vec![Command::DeleteTask(task(8))],
        vec![Command::Restore(task(8))],
    ));
    let undone = stacks.take_undo().expect("a step");
    stacks.push_undone(undone);

    same(&stacks, &round_trip(&stacks));
}

/// A move and its undo, the keys it put back included, survive the trip:
/// a key put back to "unplaced" stays a clear (#340).
#[test]
fn a_move_and_the_keys_its_undo_puts_back_round_trip() {
    let mut stacks = UndoStacks::default();
    stacks.push(entry(
        "move",
        vec![Command::MoveTasks {
            ids: vec![task(1), task(2)],
            after: Some(task(3)),
            before: None,
        }],
        vec![Command::BulkTasks(vec![
            BulkTaskItem {
                id: task(1),
                action: BulkTaskAction::Update(Box::new(TaskPatch {
                    sort_order: Some(Some("G".into())),
                    ..TaskPatch::default()
                })),
            },
            BulkTaskItem {
                id: task(2),
                action: BulkTaskAction::Update(Box::new(TaskPatch {
                    sort_order: Some(None),
                    ..TaskPatch::default()
                })),
            },
        ])],
    ));
    same(&stacks, &round_trip(&stacks));
}

/// A saved view's create, edit and delete survive the trip, a filter part a
/// newer build added and a cleared place in the list included (#341).
#[test]
fn saved_view_commands_round_trip_losslessly() {
    use sunrise_domain::{SavedViewDraft, SavedViewPatch, ViewFilter, ViewKind, ViewSort};
    let id = EntityRef::new(EntityKind::SavedView, [7; 16]);
    let mut filter = ViewFilter {
        text: "milk".into(),
        ..ViewFilter::default()
    };
    filter.unknown.insert(
        "place".into(),
        sunrise_cbor::decode_lenient(&[0x62, b'p', b'x']).unwrap(),
    );
    let mut stacks = UndoStacks::default();
    stacks.push(entry(
        "save",
        vec![Command::CreateSavedView(SavedViewDraft {
            name: "Errands".into(),
            view: ViewKind::from_raw("map"),
            filter: filter.clone(),
            sort: ViewSort::Manual,
        })],
        vec![Command::DeleteSavedView(id)],
    ));
    stacks.push(entry(
        "edit",
        vec![Command::UpdateSavedView {
            id,
            patch: SavedViewPatch {
                filter: Some(filter),
                sort_order: Some(None),
                ..SavedViewPatch::default()
            },
        }],
        vec![Command::Restore(id)],
    ));
    let back = round_trip(&stacks);
    same(&stacks, &back);
    let Command::UpdateSavedView { patch, .. } = &back.done[1].forward[0] else {
        panic!("an edit");
    };
    assert_eq!(patch.sort_order, Some(None), "a clear, not a no-op");
}

#[test]
fn a_step_with_a_command_out_of_scope_is_dropped_and_counted() {
    let mut stacks = UndoStacks::default();
    stacks.push(entry(
        "focus",
        vec![Command::MaterializeRoutines { now_ms: 0 }],
        vec![],
    ));
    stacks.push(entry("done", vec![Command::CompleteTask(task(1))], vec![]));
    let (json, dropped) = to_json(&stacks).expect("writes");
    assert_eq!(dropped, 1);
    let back = from_json(&json).expect("reads");
    assert_eq!(back.labels().undo.as_deref(), Some("done"));
    assert_eq!(back.done.len(), 1);
}

#[test]
fn another_version_and_garbage_are_refused_by_name() {
    assert!(matches!(
        from_json(r#"{"version": 99, "done": [], "undone": []}"#),
        Err(PersistError::Version(99))
    ));
    assert!(matches!(
        from_json("not json"),
        Err(PersistError::Malformed(_))
    ));
}
