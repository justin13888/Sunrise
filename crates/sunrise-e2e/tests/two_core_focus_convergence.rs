//! Concurrent focus sessions converge across two real `Core`s through the real
//! relay — the property [ADR-0013] turns on.
//!
//! Companion to `two_core_blocker_convergence.rs`: same in-process relay, same
//! real `WebSocket`s and wire protocol. What has to hold here is the thing the
//! ADR's original OR-Set was reaching for and that an append-only row keyed by
//! its own `EntityRef` gives for free:
//!
//! 1. **Two devices each starting a session on the same task both survive.**
//!    They mint different `fcs_` ids, so there is no register for them to fight
//!    over — no last-writer-wins, no lost session, nothing to reconcile.
//! 2. **Their focused time aggregates** rather than one overwriting the other:
//!    the calibration fold sees both.
//! 3. **A dangling `start` reads as running on the other replica too**, and the
//!    `end` that arrives later closes it — with the two ops crossing the wire
//!    independently and in either order.
//!
//! [ADR-0013]: ../../../docs/11-adr/0013-focus-session-op-representation.md

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    Clock, Command, Core, FocusSessionRow, FocusStartDraft, Query, QueryResult, SystemClock,
};
use sunrise_domain::{FocusKind, SessionLength, TaskDraft};
use sunrise_id::EntityRef;

/// Shared paired-device vault root.
const ROOT: [u8; 32] = [0x5F; 32];

/// Generous CI-safe cap; healthy runs finish well inside a second per wait.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Poll interval used inside every timeout loop.
const POLL: Duration = Duration::from_millis(25);

/// Fixed focused time each device reports, so the aggregate is exact rather
/// than wall-clock dependent.
const A_FOCUSED_MS: u64 = 12 * 60 * 1000;
const B_FOCUSED_MS: u64 = 8 * 60 * 1000;

/// Comparable projection of one session row: everything that must match across
/// replicas, and nothing derived from a live clock.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalSession {
    id: String,
    task: String,
    running: bool,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    actual_focused_ms: Option<u64>,
    completed_task: bool,
}

fn project(r: &FocusSessionRow) -> CanonicalSession {
    CanonicalSession {
        id: r.session.start.id.to_str(),
        task: r.session.start.task_id.to_str(),
        running: r.running,
        started_at_ms: r.session.start.started_at_ms(),
        ended_at_ms: r
            .session
            .end
            .as_ref()
            .map(sunrise_domain::FocusEnd::ended_at_ms),
        actual_focused_ms: r.session.end.as_ref().map(|e| e.actual_focused_ms),
        completed_task: r.session.end.as_ref().is_some_and(|e| e.completed_task),
    }
}

async fn sessions(core: &Core, task: EntityRef) -> Vec<CanonicalSession> {
    let rows = match core
        .query(Query::TaskFocusSessions { task, limit: 100 })
        .await
        .expect("task focus sessions")
    {
        QueryResult::FocusSessions(v) => v,
        other => panic!("expected FocusSessions, got {other:?}"),
    };
    let mut out: Vec<CanonicalSession> = rows.iter().map(project).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Total focused time the calibration fold sees on this replica.
async fn total_focused_ms(core: &Core) -> u64 {
    match core
        .query(Query::FocusStats {
            stream: None,
            since_ms: None,
            // Every session in this test is closed, so `now` never enters the
            // answer — pin it so the assertion cannot drift.
            now_ms: 0,
        })
        .await
        .expect("focus stats")
    {
        QueryResult::FocusStats(s) => s.total_focused_ms,
        other => panic!("expected FocusStats, got {other:?}"),
    }
}

async fn running_count(core: &Core) -> usize {
    match core
        .query(Query::RunningFocusSessions)
        .await
        .expect("running sessions")
    {
        QueryResult::FocusSessions(v) => v.len(),
        other => panic!("expected FocusSessions, got {other:?}"),
    }
}

/// Wait until both replicas expose an identical session projection of exactly
/// `expected_len` rows, then return it.
async fn wait_sessions_converge(
    a: &Core,
    b: &Core,
    task: EntityRef,
    expected_len: usize,
) -> Vec<CanonicalSession> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(TIMEOUT, async {
        loop {
            last_a = sessions(a, task).await;
            last_b = sessions(b, task).await;
            if last_a.len() == expected_len && last_a == last_b {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(rows) = res else {
        panic!(
            "focus sessions did not converge to {expected_len} rows\n  A ({}): {last_a:#?}\n  B ({}): {last_b:#?}",
            last_a.len(),
            last_b.len()
        );
    };
    rows
}

async fn wait_until<F, Fut>(label: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let res = tokio::time::timeout(TIMEOUT, async {
        loop {
            if f().await {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    assert!(res.is_ok(), "timed out waiting for: {label}");
}

async fn start_focus(core: &Core, task: EntityRef) -> EntityRef {
    core.submit(Command::StartFocus(FocusStartDraft {
        task_id: task,
        kind: FocusKind::Work,
        length: SessionLength::OnePomodoro,
        energy: None,
    }))
    .await
    .expect("start focus")
    .entity
}

#[tokio::test(flavor = "multi_thread")]
async fn two_core_focus_convergence() {
    let (addr, _relay) = sunrise_e2e::spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = sunrise_e2e::open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = sunrise_e2e::open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    sunrise_e2e::wait_live(&a, TIMEOUT).await;
    sunrise_e2e::wait_live(&b, TIMEOUT).await;

    // --- 1. One task, on both replicas. ---
    let task = a
        .submit(Command::CreateTask(TaskDraft {
            title: "the shared piece of work".into(),
            // A 20-minute estimate: the two devices between them will spend
            // exactly that, which is what makes the calibration assertion an
            // exact number rather than a range.
            estimated_duration_s: Some(20 * 60),
            ..Default::default()
        }))
        .await
        .expect("create task")
        .entity;
    sunrise_e2e::wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // --- 2. BOTH devices start a session on it, each unaware of the other's.
    //        Under a mutable "current session" field this is the clobber case;
    //        under an append-only row keyed by its own EntityRef the two ids
    //        simply differ. ---
    let session_a = start_focus(&a, task).await;
    let session_b = start_focus(&b, task).await;
    assert_ne!(
        session_a, session_b,
        "concurrent starts on two devices mint distinct session ids"
    );

    // Both live sessions converge, and each reads as *running* on both
    // replicas — a `start` with no `end` is a valid state, not a repair case.
    let live = wait_sessions_converge(&a, &b, task, 2).await;
    assert!(
        live.iter().all(|s| s.running),
        "both sessions are still running on both replicas: {live:#?}"
    );
    assert_eq!(running_count(&a).await, 2);
    assert_eq!(running_count(&b).await, 2);

    // --- 3. An interruption logged on B lands on A. Grow-only set: it is
    //        addressed to B's session and never contends with A's. ---
    b.submit(Command::LogInterruption {
        session: session_b,
        reason: sunrise_domain::InterruptionReason::Meeting,
    })
    .await
    .expect("log interruption");
    wait_until("A sees B's interruption", || async {
        match a
            .query(Query::TaskFocusSessions { task, limit: 100 })
            .await
            .expect("sessions")
        {
            QueryResult::FocusSessions(v) => v
                .iter()
                .any(|r| r.session.start.id == session_b && r.session.interruptions.len() == 1),
            other => panic!("expected FocusSessions, got {other:?}"),
        }
    })
    .await;

    // --- 4. Each device closes its own session with its own measured time. ---
    a.submit(Command::EndFocus {
        session: session_a,
        actual_focused_ms: Some(A_FOCUSED_MS),
        completed_task: false,
    })
    .await
    .expect("A ends its session");
    b.submit(Command::EndFocus {
        session: session_b,
        actual_focused_ms: Some(B_FOCUSED_MS),
        completed_task: true,
    })
    .await
    .expect("B ends its session");

    // --- 5. Both sessions are RETAINED on both replicas, with both frozen
    //        measurements intact. Neither `end` op overwrote the other. ---
    let converged = wait_until_closed(&a, &b, task).await;
    assert_eq!(converged.len(), 2, "both sessions survived: {converged:#?}");
    let mut actuals: Vec<Option<u64>> = converged.iter().map(|s| s.actual_focused_ms).collect();
    actuals.sort_unstable();
    assert_eq!(
        actuals,
        vec![Some(B_FOCUSED_MS), Some(A_FOCUSED_MS)],
        "each device's measurement survived the merge: {converged:#?}"
    );
    assert_eq!(
        converged.iter().filter(|s| s.completed_task).count(),
        1,
        "exactly the session that completed the task says so"
    );

    // --- 6. And the focused time AGGREGATES. This is the payoff of the
    //        representation: stats are a fold over an immutable log, so two
    //        devices' work adds up instead of one replacing the other. ---
    for (label, core) in [("A", a.as_ref()), ("B", b.as_ref())] {
        let total = total_focused_ms(core).await;
        assert_eq!(
            total,
            A_FOCUSED_MS + B_FOCUSED_MS,
            "{label}: focused time from both devices is summed, not clobbered"
        );
    }

    // The calibration factor follows from the aggregate: 20 minutes recorded
    // against a 20-minute estimate.
    for (label, core) in [("A", a.as_ref()), ("B", b.as_ref())] {
        let calibration = match core
            .query(Query::FocusStats {
                stream: None,
                since_ms: None,
                now_ms: 0,
            })
            .await
            .expect("focus stats")
        {
            QueryResult::FocusStats(s) => s.overall,
            other => panic!("expected FocusStats, got {other:?}"),
        };
        let c = calibration.unwrap_or_else(|| panic!("{label}: expected a calibration factor"));
        assert_eq!(c.samples, 1, "{label}: one task, two sessions");
        assert!(
            (c.factor - 1.0).abs() < 1e-9,
            "{label}: factor was {}",
            c.factor
        );
    }

    // Nothing is left running anywhere.
    assert_eq!(running_count(&a).await, 0);
    assert_eq!(running_count(&b).await, 0);

    a.shutdown().await;
    b.shutdown().await;
}

/// Wait until both replicas show two sessions and neither is running.
async fn wait_until_closed(a: &Core, b: &Core, task: EntityRef) -> Vec<CanonicalSession> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(TIMEOUT, async {
        loop {
            last_a = sessions(a, task).await;
            last_b = sessions(b, task).await;
            if last_a.len() == 2 && last_a == last_b && last_a.iter().all(|s| !s.running) {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(rows) = res else {
        panic!("sessions never both closed on both replicas\n  A: {last_a:#?}\n  B: {last_b:#?}");
    };
    rows
}
