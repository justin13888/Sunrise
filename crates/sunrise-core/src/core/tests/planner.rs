//! The planner's preview cache: a commit applies a remembered diff once, and
//! an evicted, cancelled or already committed one is `PLAN_UNKNOWN`.

use super::*;
use crate::engine::PlanError;
use crate::PLAN_CACHE_SIZE;
use sunrise_domain::planner::PlanIntent;
use sunrise_domain::{BlockDraft, SunriseTime};

fn at(h: i8) -> SunriseTime {
    SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(h, 0, 0, 0))
}

async fn one_block(core: &Core) -> sunrise_id::EntityRef {
    core.submit(Command::CreateBlock(BlockDraft {
        stream_id: None,
        starts_at: at(9),
        ends_at: at(10),
        title: Some("standup".into()),
        title_track_task: false,
        tasks: Vec::new(),
    }))
    .await
    .unwrap()
    .entity
}

fn move_to(block: sunrise_id::EntityRef, h: i8) -> PlanIntent {
    PlanIntent::MoveBlock {
        block,
        to_start: at(h),
    }
}

#[tokio::test]
async fn a_remembered_diff_commits_once_and_then_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = one_block(&core).await;
    let mut events = core.changes();

    let diff = core.plan_preview(&move_to(block, 11)).unwrap();
    assert_eq!(core.plan_commit(diff.id).unwrap().moved, 1);
    assert!(matches!(
        events.try_recv(),
        Ok(DomainEvent::Updated(id)) if id == block
    ));
    assert!(matches!(core.plan_commit(diff.id), Err(PlanError::Unknown)));
    core.close().await.unwrap();
}

#[tokio::test]
async fn the_cache_forgets_the_oldest_preview_and_a_cancelled_one() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = one_block(&core).await;

    let first = core.plan_preview(&move_to(block, 10)).unwrap();
    for h in 0..PLAN_CACHE_SIZE {
        core.plan_preview(&move_to(block, 11 + i8::try_from(h).unwrap()))
            .unwrap();
    }
    assert!(matches!(
        core.plan_commit(first.id),
        Err(PlanError::Unknown)
    ));

    let kept = core.plan_preview(&move_to(block, 12)).unwrap();
    core.plan_cancel(kept.id);
    assert!(matches!(core.plan_commit(kept.id), Err(PlanError::Unknown)));
    core.close().await.unwrap();
}

#[tokio::test]
async fn a_closed_vault_previews_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = one_block(&core).await;
    let diff = core.plan_preview(&move_to(block, 11)).unwrap();
    core.shutdown().await;
    assert!(matches!(
        core.plan_preview(&move_to(block, 11)),
        Err(PlanError::Closed)
    ));
    assert!(matches!(core.plan_commit(diff.id), Err(PlanError::Closed)));
}
