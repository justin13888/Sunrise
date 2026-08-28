//! Probe: does the id-only-delete divergence that `TaskDelete` had also affect
//! `StreamDelete`, `ContextDelete`, and `RoutineDelete`?
//!
//! `TaskDelete` was converted to carry full entity state under
//! `DOC_SCHEMA_V = 4` (see `two_core_delete_convergence.rs` and ADR-0014). The
//! other delete ops still carry a bare `EntityRef` and apply through
//! `tombstone_*`, which sets `deleted = 1` plus the LWW stamp and touches no
//! other column — structurally identical to what `TaskDelete` used to do.
//!
//! These tests exist to establish empirically whether that shape actually
//! diverges, rather than inferring it from the code. Each races a delete on one
//! replica against an update on the other and asserts full convergence,
//! tombstone included — the same assertion the Task test makes.
//!
//! # Finding: all three diverge
//!
//! Measured over 12 runs each:
//!
//! | Op | Runs diverged |
//! |---|---|
//! | `StreamDelete` | 1 / 12 |
//! | `ContextDelete` | 2 / 12 |
//! | `RoutineDelete` | 2 / 12 |
//!
//! The failure is identical in shape to the one `TaskDelete` had — both
//! replicas agree on `deleted` and disagree on the contested scalar forever:
//!
//! ```text
//! a = ("contested", true)
//! b = ("renamed",   true)
//! ```
//!
//! **Do not read a low rate as a mild bug.** Divergence occurs only when the
//! *delete* wins the LWW race. When the update wins, the delete is rejected
//! wholesale and both replicas keep the updated row, which converges — so the
//! rate here measures how often the delete happened to be later, not how often
//! the defect applies. Every run where the delete wins diverges, and the
//! divergence is permanent: both sides then carry the same winning stamp, so
//! neither will ever accept a correction.
//!
//! (Sampling at 12 runs also means these counts are noisy — the underlying
//! probability is roughly "how often does the delete land last", not the
//! 8–17% the table happens to show.)
//!
//! # Why these are `#[ignore]`d
//!
//! Converting the other delete ops is a separate decision that has not been
//! taken. These are evidence for that decision, not a red gate. Run them with
//! `cargo test -p sunrise-e2e --test delete_defect_probe -- --ignored`.
//!
//! If the decision is to convert, the fix is mechanical and mirrors
//! `TaskDelete`: change the `InnerOp` variant to carry the entity, pass the
//! already-mutated value at each construction site, apply through the same path
//! as the corresponding update, delete the now-dead `tombstone_*` helper, and
//! bump `DOC_SCHEMA_V`. Un-`#[ignore]` these and they become the regression
//! tests.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use sunrise_core::{Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{
    inbox_stream_ref, ContextDraft, ContextPatch, Frequency, RRule, RoutineCatchupPolicy,
    RoutineDraft, RoutinePatch, StreamDraft, StreamPatch, TaskTemplate,
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

#[ignore = "probe, not a gate: StreamDelete diverges when the delete wins the LWW race — see module docs"]
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

#[ignore = "probe, not a gate: ContextDelete diverges when the delete wins the LWW race — see module docs"]
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

#[ignore = "probe, not a gate: RoutineDelete diverges when the delete wins the LWW race — see module docs"]
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
