//! Regression coverage for delete convergence on `Stream`, `Context`,
//! `Routine`, and `Block` — the ops that had the same id-only-delete defect as
//! `TaskDelete`, and were converted alongside it under `DOC_SCHEMA_V = 4`.
//!
//! Each races a delete on one replica against an update on the other and
//! asserts full convergence, tombstone included — the same assertion
//! `two_core_delete_convergence.rs` makes for `Task`.
//!
//! # What these caught, and why the rate was misleading
//!
//! These began as probes, written to establish empirically whether the shape
//! `TaskDelete` had actually diverged rather than inferring it from the code.
//! It did, in all three cases. Measured over 12 runs each, before the fix:
//!
//! | Op | Runs diverged |
//! |---|---|
//! | `StreamDelete` | 1 / 12 |
//! | `ContextDelete` | 2 / 12 |
//! | `RoutineDelete` | 2 / 12 |
//!
//! The failure was identical in shape to `TaskDelete`'s — both replicas agree
//! on `deleted` and disagree on the contested scalar forever:
//!
//! ```text
//! a = ("contested", true)
//! b = ("renamed",   true)
//! ```
//!
//! **A low rate here was never evidence of a mild bug**, and this is the part
//! worth keeping. Divergence occurs only when the *delete* wins the LWW race.
//! When the update wins, the delete is rejected wholesale and both replicas
//! keep the updated row, which converges. So the rate measured how often the
//! delete happened to land last — not how often the defect applied. Every
//! delete-wins run diverged, and permanently: both sides then carried the same
//! winning stamp, so neither would ever accept a correction.
//!
//! That distinction mattered. The very first probe run had `Stream` failing
//! while `Context` and `Routine` passed, which looked like "only Stream is
//! affected". It was luck, not signal. Repeating the runs showed all three.
//!
//! The same reasoning governs how these are read *now*: a green run does not
//! prove much on its own, because a run where the update wins would pass even
//! against the old broken code. Confidence comes from repetition. The fix was
//! validated at 0 failures in 40 runs, and reverting the three apply paths to
//! tombstone-only reproduced divergence in all three.
//!
//! # The fix these lock in
//!
//! Every delete op carries the entity's full state with `deleted` set, and
//! applies through the same path as the corresponding update, so the whole row
//! is replaced. See ADR-0014.
//!
//! # Where the deterministic coverage lives
//!
//! `BlockDelete` and `AttachmentDelete` were converted last, and their coverage
//! deliberately does **not** rest on a race. `crates/sunrise-core/src/engine.rs`
//! carries three unit tests over two `Engine`s on `FakeClock`s that *choose*
//! the stamps so the delete always wins, and that drive a delete which
//! overtakes its own create:
//!
//! - `a_block_delete_that_wins_lww_replaces_the_whole_row`
//! - `a_block_delete_that_overtakes_its_create_still_lands_as_a_tombstone`
//! - `an_attachment_delete_that_overtakes_its_create_still_lands_as_a_tombstone`
//!
//! That is the instrument this file's history argues for: because the winner is
//! forced rather than raced, a single green run *is* conclusive, and reverting
//! the apply paths fails all three every time instead of one run in four.
//!
//! `AttachmentDelete` has no probe here on purpose. Attachment metadata is
//! write-once — there is no `AttachmentUpdate` op — so the delete-versus-update
//! race these probes stage cannot be constructed for it at all. Its half of the
//! defect is the ordering case, which the engine test above covers exactly.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use sunrise_core::{Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{
    inbox_stream_ref, BlockDraft, BlockPatch, ContextDraft, ContextPatch, Frequency, RRule,
    RoutineCatchupPolicy, RoutineDraft, RoutinePatch, StreamDraft, StreamPatch, SunriseTime,
    TaskTemplate,
};
use sunrise_e2e::{open_synced_core, spawn_relay, trust_each_other, wait_live, wait_pending_zero};
use sunrise_id::EntityRef;

const ROOT: [u8; 32] = [0x42; 32];
const TIMEOUT: Duration = Duration::from_secs(20);

/// Two paired, live replicas on one relay.
async fn pair(
    dir_a: &std::path::Path,
    dir_b: &std::path::Path,
    addr: std::net::SocketAddr,
) -> (Arc<Core>, Arc<Core>) {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let a = open_synced_core(dir_a, ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b, ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    (a, b)
}

/// The name each replica holds for `id`, tombstone-blind — `EntityById` does
/// not filter deleted rows, which is exactly why it can see the divergence.
async fn name_and_deleted(core: &Core, id: EntityRef) -> Option<(String, bool)> {
    match core.query(Query::EntityById(id)).await {
        Ok(QueryResult::Stream(s)) => Some((s.name.clone(), s.deleted)),
        Ok(QueryResult::Context(c)) => Some((c.name.clone(), c.deleted)),
        // Routine has no name; its timezone is the contested scalar.
        Ok(QueryResult::Routine(r)) => Some((r.timezone.clone(), r.deleted)),
        // `EntityById` on a Block yields the one grid row. Its title is the
        // contested scalar, and the row is not tombstone-filtered.
        Ok(QueryResult::Blocks(rows)) => rows.first().map(|row| {
            (
                row.block.title.clone().unwrap_or_default(),
                row.block.deleted,
            )
        }),
        Ok(other) => panic!("unexpected query result: {other:?}"),
        Err(_) => None,
    }
}

/// Race a delete on A against a rename on B, then assert both replicas agree on
/// the entity's *whole* state.
async fn assert_converges_after_race(
    a: &Core,
    b: &Core,
    id: EntityRef,
    delete: Command,
    rename: Command,
) {
    tokio::join!(
        async {
            a.submit(delete).await.expect("delete");
        },
        async {
            b.submit(rename).await.expect("rename");
        }
    );
    wait_pending_zero(a, TIMEOUT).await;
    wait_pending_zero(b, TIMEOUT).await;

    // Settle: both sides have drained, so any remaining difference is stable.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let sa = name_and_deleted(a, id).await;
    let sb = name_and_deleted(b, id).await;
    assert_eq!(
        sa, sb,
        "replicas disagree about {id} after a concurrent delete/update"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_delete_vs_update_converges() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = pair(dir_a.path(), dir_b.path(), addr).await;

    let id = a
        .submit(Command::CreateStream(StreamDraft {
            name: "contested".into(),
            ..Default::default()
        }))
        .await
        .expect("create stream")
        .entity;
    wait_pending_zero(&a, TIMEOUT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_converges_after_race(
        &a,
        &b,
        id,
        Command::DeleteStream(id),
        Command::UpdateStream {
            id,
            patch: StreamPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        },
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn context_delete_vs_update_converges() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = pair(dir_a.path(), dir_b.path(), addr).await;

    let id = a
        .submit(Command::CreateContext(ContextDraft {
            name: "contested".into(),
            ..Default::default()
        }))
        .await
        .expect("create context")
        .entity;
    wait_pending_zero(&a, TIMEOUT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_converges_after_race(
        &a,
        &b,
        id,
        Command::DeleteContext(id),
        Command::UpdateContext {
            id,
            patch: ContextPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        },
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn routine_delete_vs_update_converges() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = pair(dir_a.path(), dir_b.path(), addr).await;

    let id = a
        .submit(Command::CreateRoutine(RoutineDraft {
            template: TaskTemplate {
                title: "occurrence".into(),
                stream_id: inbox_stream_ref(),
                contexts: vec![],
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule {
                freq: Frequency::Daily,
                interval: 1,
                by_day: vec![],
                by_month_day: vec![],
                by_month: vec![],
                by_set_pos: vec![],
                count: Some(3),
                until: None,
                wkst: None,
            },
            timezone: "UTC".into(),
            starts_at: Timestamp::from_millisecond(1_700_000_000_000).unwrap(),
            ends_at: None,
            scheduling_constraints: vec![],
            catchup_policy: RoutineCatchupPolicy::Skip,
        }))
        .await
        .expect("create routine")
        .entity;
    wait_pending_zero(&a, TIMEOUT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_converges_after_race(
        &a,
        &b,
        id,
        Command::DeleteRoutine(id),
        Command::UpdateRoutine {
            id,
            patch: RoutinePatch {
                timezone: Some("Europe/London".into()),
                ..Default::default()
            },
        },
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn block_delete_vs_update_converges() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (a, b) = pair(dir_a.path(), dir_b.path(), addr).await;

    let id = a
        .submit(Command::CreateBlock(BlockDraft {
            stream_id: inbox_stream_ref(),
            starts_at: SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(9, 0, 0, 0)),
            ends_at: SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(10, 0, 0, 0)),
            title: Some("contested".into()),
            title_track_task: false,
            tasks: vec![],
        }))
        .await
        .expect("create block")
        .entity;
    wait_pending_zero(&a, TIMEOUT).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_converges_after_race(
        &a,
        &b,
        id,
        Command::DeleteBlock(id),
        Command::UpdateBlock {
            id,
            patch: BlockPatch {
                title: Some(Some("renamed".into())),
                ..Default::default()
            },
        },
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}
