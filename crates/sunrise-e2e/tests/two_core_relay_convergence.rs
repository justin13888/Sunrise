//! The flagship end-to-end: two real `Core`s converging through the real
//! `sunrise-server` relay over real WebSockets.
//!
//! This is the acceptance test for the entire sync stack — envelope sealing
//! (S6), wire payloads + the relay replay ring (S7), `apply_remote` / LWW (S8),
//! and the client sync driver (S9). Everything runs in-process but over actual
//! TCP/WebSocket sockets and the production wire protocol; nothing here is
//! mocked.

#![allow(
    clippy::missing_panics_doc,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use jiff::civil::time;
use jiff::Timestamp;
use sunrise_core::{Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{
    inbox_stream_ref, ConstraintSeverity, Frequency, RRule, RoutineCatchupPolicy, RoutineDraft,
    ScheduleConstraint, StreamDraft, TaskDraft, TaskPatch, TaskTemplate, TimeOfDayRange, Weekday,
    WeekdaySet,
};
use sunrise_e2e::{
    canonical_tasks, open_synced_core, spawn_relay, trust_each_other, wait_live, wait_pending_zero,
    wait_streams_converge, wait_tasks_converge,
};
use sunrise_id::EntityRef;
use sunrise_sync::SyncState;

/// Shared paired-device vault root (both replicas key their per-stream keys
/// from this; envelopes decrypt on both sides).
const ROOT: [u8; 32] = [0x42; 32];

/// Generous CI-safe cap; healthy runs finish in well under a second per wait.
const TIMEOUT: Duration = Duration::from_secs(20);

async fn create_stream(core: &Core, name: &str) -> EntityRef {
    core.submit(Command::CreateStream(StreamDraft {
        name: name.into(),
        ..Default::default()
    }))
    .await
    .expect("create stream")
    .entity
}

async fn create_task(
    core: &Core,
    title: &str,
    stream: EntityRef,
    constraints: Vec<ScheduleConstraint>,
) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        stream_id: Some(stream),
        scheduling_constraints: constraints,
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

async fn set_title(core: &Core, id: EntityRef, title: &str) {
    core.submit(Command::UpdateTask {
        id,
        patch: TaskPatch {
            title: Some(title.into()),
            ..Default::default()
        },
    })
    .await
    .expect("update title");
}

async fn assert_live_and_drained(core: &Core) {
    match core.query(Query::SyncStatus).await.expect("sync status") {
        QueryResult::SyncStatus(s) => {
            assert_eq!(s.state, SyncState::Live, "ends Live");
            assert_eq!(s.outbox_pending, 0, "outbox fully drained");
        }
        other => panic!("expected SyncStatus, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_core_relay_convergence() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // --- 1. Boot both cores, trust each other, reach Live. ---
    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // --- 2. A creates a stream + 3 tasks (one with scheduling constraints);
    //        B converges to identical canonical state. ---
    let shared = create_stream(&a, "Shared").await;
    let constraint = ScheduleConstraint {
        time_of_day: Some(TimeOfDayRange {
            start: time(9, 0, 0, 0),
            end: time(17, 0, 0, 0),
        }),
        days_of_week: WeekdaySet::from_days([Weekday::Mo, Weekday::We, Weekday::Fr]),
        date_range: None,
        severity: ConstraintSeverity::Hard,
    };
    let t1 = create_task(&a, "task one", shared, vec![]).await;
    let t2 = create_task(&a, "task two", shared, vec![]).await;
    let t3 = create_task(&a, "task three", shared, vec![constraint]).await;

    let converged = wait_tasks_converge(&a, &b, 3, TIMEOUT).await;
    assert_eq!(
        converged,
        canonical_tasks(&a).await,
        "B's table equals A's table"
    );
    // The constraint round-tripped through seal → relay → apply_remote intact.
    assert!(
        converged
            .iter()
            .any(|t| t.id == t3.to_str() && t.constraints.len() == 1),
        "the constrained task carried its constraint list across the wire"
    );
    // Both learn the "Shared" stream (Inbox + Shared = 2 rows).
    wait_streams_converge(&a, &b, 2, TIMEOUT).await;

    // --- 3. B completes one task and renames another; A converges. ---
    b.submit(Command::CompleteTask(t1))
        .await
        .expect("complete t1");
    set_title(&b, t2, "task two (edited by B)").await;
    let converged = wait_tasks_converge(&a, &b, 3, TIMEOUT).await;
    assert!(
        converged
            .iter()
            .any(|t| t.id == t1.to_str() && t.state == "done"),
        "B's completion reached A"
    );
    assert!(
        converged
            .iter()
            .any(|t| t.title == "task two (edited by B)"),
        "B's rename reached A"
    );

    // --- 4. Offline catch-up: close B fully, A creates 5 tasks, reopen B. ---
    b.shutdown().await;
    drop(b); // releases B's vault lock so the same dir can be reopened
    for i in 0..5 {
        create_task(&a, &format!("offline extra {i}"), shared, vec![]).await;
    }
    wait_pending_zero(&a, TIMEOUT).await; // A drained to the relay while B was gone
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    wait_live(&b, TIMEOUT).await;
    // B recovers the 5 missed tasks via the relay's retained ring replay + its
    // own persisted cursors; its own earlier frames replay too and no-op.
    wait_tasks_converge(&a, &b, 8, TIMEOUT).await;

    // --- 5. Concurrent conflict on the SAME task; both converge to one title. ---
    set_title(&a, t3, "t3 written by A").await;
    set_title(&b, t3, "t3 written by B").await;
    let converged = wait_tasks_converge(&a, &b, 8, TIMEOUT).await;
    let t3_row = converged
        .iter()
        .find(|t| t.id == t3.to_str())
        .expect("t3 present");
    assert!(
        t3_row.title == "t3 written by A" || t3_row.title == "t3 written by B",
        "LWW picked one of the two titles consistently on both replicas: {}",
        t3_row.title
    );

    // --- 6. Both end Live with a fully-drained outbox. ---
    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    assert_live_and_drained(&a).await;
    assert_live_and_drained(&b).await;

    a.shutdown().await;
    b.shutdown().await;
}

/// Deterministic fixed clock: the routine scenario needs both replicas to agree
/// on `now` so occurrence keys — and thus the deterministic task ids that make
/// double-materialization collapse — line up exactly.
#[derive(Debug)]
struct FixedClock(u64);
impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn routine_materialization_convergence() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let now: u64 = 1_700_000_000_000;
    let clock: Arc<dyn Clock> = Arc::new(FixedClock(now));

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // A daily routine anchored two days in the past, COUNT=3, Queue catchup:
    // three missed occurrences (t-2d, t-1d, now) all materialize.
    let starts_ms = now - 2 * 86_400_000;
    let draft = RoutineDraft {
        template: TaskTemplate {
            title: "standup".into(),
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
        starts_at: Timestamp::from_millisecond(i64::try_from(starts_ms).unwrap()).unwrap(),
        ends_at: None,
        scheduling_constraints: vec![],
        catchup_policy: RoutineCatchupPolicy::Queue,
    };
    a.submit(Command::CreateRoutine(draft))
        .await
        .expect("create routine");

    // A materialized 3 occurrences locally; they + the routine meta sync to B.
    wait_tasks_converge(&a, &b, 3, TIMEOUT).await;

    // Trigger materialization on BOTH with the same `now`. Deterministic
    // occurrence ids collapse via INSERT-OR-IGNORE, so nothing is duplicated.
    a.submit(Command::MaterializeRoutines { now_ms: now })
        .await
        .expect("materialize A");
    b.submit(Command::MaterializeRoutines { now_ms: now })
        .await
        .expect("materialize B");

    let converged = wait_tasks_converge(&a, &b, 3, TIMEOUT).await;
    assert_eq!(
        converged.len(),
        3,
        "exactly the 3 occurrences — double-materialization collapsed"
    );

    // The routine itself synced to B via the meta stream.
    match b.query(Query::Routines).await.expect("routines") {
        QueryResult::Routines(r) => assert_eq!(r.len(), 1, "routine synced to B"),
        other => panic!("expected Routines, got {other:?}"),
    }

    a.shutdown().await;
    b.shutdown().await;
}
