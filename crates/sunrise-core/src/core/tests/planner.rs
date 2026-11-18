//! The planner's preview cache: a commit applies a remembered diff once, and
//! an evicted, cancelled or already committed one is `PLAN_UNKNOWN`. And its
//! undo: per field, from the inverse stored with the commit.

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

fn block_span(core: &Core, block: sunrise_id::EntityRef) -> (i64, i64) {
    core.db
        .lock()
        .conn()
        .query_row(
            "SELECT starts_at_ms, ends_at_ms FROM blocks WHERE id = ?1",
            [&block.bytes()[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

async fn task_at(core: &Core, h: i8) -> sunrise_id::EntityRef {
    core.submit(Command::CreateTask(sunrise_domain::TaskDraft {
        title: format!("at {h}"),
        planned_at: Some(at(h)),
        estimated_duration_s: Some(1800),
        ..Default::default()
    }))
    .await
    .unwrap()
    .entity
}

async fn read_task(core: &Core, id: sunrise_id::EntityRef) -> sunrise_domain::Task {
    match core.query(Query::EntityById(id)).await.unwrap() {
        QueryResult::Task(t) => *t,
        other => panic!("expected Task, got {other:?}"),
    }
}

/// The inverse is stored with the commit, so an undo after every preview
/// has left the cache still restores, and the commit is then forgotten.
#[tokio::test]
async fn an_undo_after_the_preview_was_evicted_restores_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = one_block(&core).await;
    let was = block_span(&core, block);

    let diff = core.plan_preview(&move_to(block, 11)).unwrap();
    let commit = core.plan_commit(diff.id).unwrap().commit;
    assert_ne!(block_span(&core, block), was);
    for h in 0..PLAN_CACHE_SIZE {
        core.plan_preview(&move_to(block, 12 + i8::try_from(h).unwrap()))
            .unwrap();
    }
    assert!(matches!(core.plan_commit(diff.id), Err(PlanError::Unknown)));

    let mut events = core.changes();
    let undone = core.plan_undo(commit).unwrap();
    assert_eq!(undone.restored, 2, "starts_at and ends_at");
    assert!(undone.skipped.is_empty(), "{:?}", undone.skipped);
    assert_eq!(block_span(&core, block), was);
    assert!(matches!(
        events.try_recv(),
        Ok(DomainEvent::Updated(id)) if id == block
    ));
    assert!(matches!(core.plan_undo(commit), Err(PlanError::Unknown)));
    core.close().await.unwrap();
}

/// A field written again since the commit is skipped and reported, and the
/// other fields of the commit are restored; a restored deferral takes its
/// count back.
#[tokio::test]
async fn a_field_edited_since_is_skipped_and_the_others_are_restored() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let kept = task_at(&core, 9).await;
    let edited = task_at(&core, 14).await;

    let diff = core
        .plan_preview(&PlanIntent::BulkDefer {
            tasks: vec![kept, edited],
            to: jiff::civil::date(2026, 3, 5),
            place: false,
        })
        .unwrap();
    let commit = core.plan_commit(diff.id).unwrap();
    assert_eq!(commit.moved, 2);
    assert_eq!(read_task(&core, kept).await.deferred_count, 1);

    core.submit(Command::UpdateTask {
        id: edited,
        patch: sunrise_domain::TaskPatch {
            planned_at: Some(Some(at(16))),
            ..Default::default()
        },
    })
    .await
    .unwrap();

    let undone = core.plan_undo(commit.commit).unwrap();
    assert_eq!(undone.restored, 1);
    assert_eq!(
        undone.skipped,
        vec![crate::PlanUndoSkip {
            item: edited,
            field: crate::PlanField::PlannedAt,
        }]
    );
    let kept = read_task(&core, kept).await;
    assert_eq!(kept.planned_at, Some(at(9)));
    assert_eq!(kept.deferred_count, 0, "the restored deferral is undone");
    let edited = read_task(&core, edited).await;
    assert_eq!(edited.planned_at, Some(at(16)), "the later edit stands");
    assert_eq!(edited.deferred_count, 1);
    core.close().await.unwrap();
}

/// An item whose restored fields would break its invariants is skipped whole:
/// a block moved from 14:00-15:00 to 09:00-10:00 and then given a 09:30 end
/// would restore only its start, ending before it begins, so neither field is
/// written and both are reported.
#[tokio::test]
async fn an_undo_that_would_invert_a_resized_block_skips_it_whole() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = core
        .submit(Command::CreateBlock(BlockDraft {
            stream_id: None,
            starts_at: at(14),
            ends_at: at(15),
            title: Some("review".into()),
            title_track_task: false,
            tasks: Vec::new(),
        }))
        .await
        .unwrap()
        .entity;

    let diff = core.plan_preview(&move_to(block, 9)).unwrap();
    let commit = core.plan_commit(diff.id).unwrap().commit;
    core.submit(Command::UpdateBlock {
        id: block,
        scope: sunrise_domain::EditScope::All,
        patch: sunrise_domain::BlockPatch {
            ends_at: Some(SunriseTime::floating(
                jiff::civil::date(2026, 3, 4).at(9, 30, 0, 0),
            )),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    let resized = block_span(&core, block);

    let mut events = core.changes();
    let undone = core.plan_undo(commit).unwrap();
    assert_eq!(undone.restored, 0);
    assert_eq!(
        undone.skipped,
        vec![
            crate::PlanUndoSkip {
                item: block,
                field: crate::PlanField::EndsAt,
            },
            crate::PlanUndoSkip {
                item: block,
                field: crate::PlanField::StartsAt,
            },
        ],
        "both fields, in the stored order (by field name)"
    );
    assert_eq!(block_span(&core, block), resized, "the block is left as is");
    assert!(events.try_recv().is_err(), "nothing was written");
    core.close().await.unwrap();
}

/// Only the newest `PLAN_UNDO_HISTORY` commits keep their inverse.
#[tokio::test]
async fn the_oldest_commit_past_the_undo_history_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = one_block(&core).await;
    let mut commits = Vec::new();
    for i in 0..=crate::PLAN_UNDO_HISTORY {
        let h = 10 + i8::try_from(i % 2).unwrap();
        let diff = core.plan_preview(&move_to(block, h)).unwrap();
        commits.push(core.plan_commit(diff.id).unwrap().commit);
    }
    assert!(matches!(
        core.plan_undo(commits[0]),
        Err(PlanError::Unknown)
    ));
    let newest = core.plan_undo(*commits.last().unwrap()).unwrap();
    assert_eq!(newest.restored, 2);
    // The one before it wrote the fields the newest wrote over since.
    let older = core.plan_undo(commits[commits.len() - 2]).unwrap();
    assert_eq!(older.restored, 0);
    assert_eq!(older.skipped.len(), 2);
    core.close().await.unwrap();
}

#[tokio::test]
async fn a_closed_vault_previews_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let block = one_block(&core).await;
    let diff = core.plan_preview(&move_to(block, 11)).unwrap();
    let commit = core
        .plan_commit(core.plan_preview(&move_to(block, 12)).unwrap().id)
        .unwrap()
        .commit;
    core.shutdown().await;
    assert!(matches!(
        core.plan_preview(&move_to(block, 11)),
        Err(PlanError::Closed)
    ));
    assert!(matches!(core.plan_commit(diff.id), Err(PlanError::Closed)));
    assert!(matches!(core.plan_undo(commit), Err(PlanError::Closed)));
}

#[test]
fn every_plan_error_answers_its_registered_code() {
    use sunrise_error::{ErrorCode, ErrorKind};
    let cases = [
        (PlanError::Stale, ErrorCode::PlanStale, ErrorKind::Permanent),
        (
            PlanError::Unknown,
            ErrorCode::PlanUnknown,
            ErrorKind::Permanent,
        ),
        (
            PlanError::NotCommittable,
            ErrorCode::PlanNotCommittable,
            ErrorKind::User,
        ),
        (
            PlanError::Invalid("x".into()),
            ErrorCode::PlanInvalid,
            ErrorKind::User,
        ),
        (
            PlanError::Closed,
            ErrorCode::PlanClosed,
            ErrorKind::Permanent,
        ),
        (
            PlanError::from(rusqlite::Error::InvalidQuery),
            ErrorCode::PlanFailed,
            ErrorKind::Internal,
        ),
    ];
    for (err, code, kind) in cases {
        assert_eq!(err.code(), code, "{err}");
        assert_eq!(code.default_kind(), kind, "{code}");
        assert!(!code.retryable(), "{code} is never re-sent as it was");
    }
    // The two codes a client branches on also lead the message it reads.
    assert!(PlanError::Stale
        .to_string()
        .starts_with(ErrorCode::PlanStale.as_str()));
    assert!(PlanError::Unknown
        .to_string()
        .starts_with(ErrorCode::PlanUnknown.as_str()));
}
