//! The planner across the seam (#343): a client sends a gesture, renders the
//! diff, commits it by id, and undoes the commit per field (#1158).
//! `flavor = "multi_thread"` for the reason `seam.rs` gives.

use sunrise_core::PlanField;
use sunrise_core_bindings::dto::{BlockDraftIn, TaskEdit, TimeValue};
use sunrise_core_bindings::planner::{PlanEdge, PlanIntentItem, PlanPositionItem};
use sunrise_core_bindings::{BindingError, CoreCommand};
use sunrise_domain::planner::{MoveRole, OccurrenceScope};

mod support;
use support::{draft, open_core};

fn at(h: u8) -> TimeValue {
    TimeValue::Floating {
        civil: format!("2030-03-04T{h:02}:00:00").parse().expect("civil"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drag_previews_a_ripple_and_commits_it_once() {
    let (_dir, core) = open_core().await;
    let block = core
        .submit(CoreCommand::CreateBlock {
            draft: BlockDraftIn {
                stream_id: sunrise_core_bindings::vocab::inbox_stream_id(),
                starts_at: at(9),
                ends_at: at(10),
                title: Some("Standup".into()),
                title_track_task: false,
                tasks: Vec::new(),
            },
        })
        .await
        .expect("block")
        .entity;
    let mut d = draft("Write");
    d.scheduled_at = Some(at(11));
    d.estimated_duration_s = Some(3600);
    let task = core
        .submit(CoreCommand::CreateTask { draft: d })
        .await
        .expect("task")
        .entity;

    let diff = core
        .plan_preview(PlanIntentItem::MoveBlock {
            block,
            to_start: at(11),
        })
        .expect("preview");
    assert_eq!(diff.id.len(), 32);
    assert!(diff.committable);
    assert_eq!(diff.moves.len(), 2);
    assert_eq!(diff.moves[0].item, block);
    assert!(matches!(diff.moves[0].role, MoveRole::Subject));
    let rippled = &diff.moves[1];
    assert_eq!(rippled.item, task);
    assert!(matches!(
        rippled.after,
        PlanPositionItem::Slot {
            start: TimeValue::Floating { .. },
            ..
        }
    ));

    let commit = core.plan_commit(diff.id.clone()).expect("commit");
    assert_eq!(commit.moved, 2);
    assert_eq!(commit.commit_id.len(), 32);
    assert!(matches!(
        core.plan_commit(diff.id),
        Err(BindingError::PlanUnknown)
    ));

    // The task is moved again by hand: its undo is skipped and reported,
    // and the block's two fields are restored.
    core.submit(CoreCommand::UpdateTask {
        id: task,
        edit: TaskEdit {
            set_scheduled_at: Some(at(15)),
            ..TaskEdit::default()
        },
    })
    .await
    .expect("edit");
    let undone = core.plan_undo(commit.commit_id.clone()).expect("undo");
    assert_eq!(undone.restored, 2);
    assert_eq!(undone.skipped.len(), 1);
    assert_eq!(undone.skipped[0].item, task);
    assert!(matches!(undone.skipped[0].field, PlanField::PlannedAt));
    assert!(matches!(
        core.plan_undo(commit.commit_id),
        Err(BindingError::PlanUnknown)
    ));
}

/// No command creates a recurring block across the seam yet, so a drag of
/// one occurrence is sent for a one-off block, which the core refuses as
/// not repeating: the refusal shows the key and scope crossed, since the
/// same drag without them previews.
#[tokio::test(flavor = "multi_thread")]
async fn a_drag_of_one_occurrence_reaches_the_core() {
    let (_dir, core) = open_core().await;
    let block = core
        .submit(CoreCommand::CreateBlock {
            draft: BlockDraftIn {
                stream_id: sunrise_core_bindings::vocab::inbox_stream_id(),
                starts_at: at(9),
                ends_at: at(10),
                title: Some("Standup".into()),
                title_track_task: false,
                tasks: Vec::new(),
            },
        })
        .await
        .expect("block")
        .entity;
    let key = "2030-03-04T09:00".to_owned();
    for scope in [
        OccurrenceScope::This,
        OccurrenceScope::All,
        OccurrenceScope::ThisAndFollowing,
    ] {
        let moved = core.plan_preview(PlanIntentItem::MoveBlockOccurrence {
            block,
            key: key.clone(),
            scope,
            to_start: at(11),
        });
        assert!(
            matches!(&moved, Err(BindingError::PlanRefused(why)) if why.contains("does not repeat")),
            "{moved:?}"
        );
        let resized = core.plan_preview(PlanIntentItem::ResizeBlockOccurrence {
            block,
            key: key.clone(),
            scope,
            edge: PlanEdge::End,
            to: at(11),
        });
        assert!(
            matches!(&resized, Err(BindingError::PlanRefused(why)) if why.contains("does not repeat")),
            "{resized:?}"
        );
    }
    assert!(core
        .plan_preview(PlanIntentItem::MoveBlock {
            block,
            to_start: at(11),
        })
        .is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_diff_id_is_a_width_error() {
    let (_dir, core) = open_core().await;
    assert!(matches!(
        core.plan_commit("not hex".into()),
        Err(BindingError::BadFixedBytes { .. })
    ));
    assert!(matches!(
        core.plan_cancel("00".into()),
        Err(BindingError::BadFixedBytes { .. })
    ));
}
