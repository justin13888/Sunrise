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
    let undone = stacks.take_undo().expect("a step");
    stacks.push_undone(undone);

    same(&stacks, &round_trip(&stacks));
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
