//! The planner across the seam (#343): a client sends a gesture, renders the
//! diff, and commits it by id. `flavor = "multi_thread"` for the reason
//! `seam.rs` gives.

use sunrise_core_bindings::dto::{BlockDraftIn, TimeValue};
use sunrise_core_bindings::planner::{PlanIntentItem, PlanPositionItem};
use sunrise_core_bindings::{BindingError, CoreCommand};
use sunrise_domain::planner::MoveRole;

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

    assert_eq!(core.plan_commit(diff.id.clone()).expect("commit").moved, 2);
    assert!(matches!(
        core.plan_commit(diff.id),
        Err(BindingError::PlanUnknown)
    ));
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
