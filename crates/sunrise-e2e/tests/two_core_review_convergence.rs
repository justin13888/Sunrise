//! Reviews and history converge across two real `Core`s through the real relay
//! — `docs/08-features/reviews-and-stats.md` §Weekly review step 5 and
//! §Activity timeline.
//!
//! Companion to `two_core_focus_convergence.rs`: same in-process relay, same
//! real `WebSocket`s and wire protocol. Two properties have to hold, and they
//! are the two the representation was chosen for.
//!
//! 1. **A review snapshot is an event, not a register.** Both devices finish a
//!    review of the *same week*, each with its own reflection. Under a
//!    last-writer-wins row keyed by the week, one user's reflection would
//!    silently replace the other's. Keyed by its own `rvw_` id, the two
//!    snapshots simply coexist — on both replicas, in the same order.
//!
//! 2. **The activity timeline is derived, so it merges for free.** Nothing
//!    about the feed is persisted: it is a fold over the op log, and a remote
//!    op lands in that log by the same path a local one does. So B's timeline
//!    shows A's edits, attributed to **A's device id**, with no timeline-specific
//!    plumbing anywhere in the sync path.
//!
//! The ordering hazard this covers: a snapshot op names Streams the receiving
//! replica may not have materialized yet. It carries their counts inside an
//! opaque blob and its table has no foreign keys, so it lands whichever order
//! the ops arrive in — which is why the snapshot below is submitted *before*
//! the tasks it describes have finished converging.

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{
    ActivityEvent, ActivityKind, ReviewSnapshot, ReviewSnapshotDraft, ReviewSnapshotStream,
    ReviewTotals, StreakRow, TaskDraft, TaskPatch, TaskState,
};
use sunrise_id::{EntityKind, EntityRef};

/// Shared paired-device vault root.
const ROOT: [u8; 32] = [0x7A; 32];

/// Generous CI-safe cap; healthy runs finish well inside a second per wait.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Poll interval used inside every timeout loop.
const POLL: Duration = Duration::from_millis(25);

/// A fixed review window, so the assertions are exact rather than dependent on
/// when the test happens to run. Monday 2026-01-05T00:00:00Z + 7 days.
const WINDOW_START_MS: u64 = 1_767_571_200_000;
const WINDOW_END_MS: u64 = WINDOW_START_MS + 7 * 24 * 60 * 60 * 1000;

/// Comparable projection of one saved snapshot: everything that must match
/// across replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalSnapshot {
    id: String,
    window_start_ms: u64,
    window_end_ms: u64,
    completed: u32,
    note: Option<String>,
    streams: Vec<(String, u32)>,
    streaks: Vec<(String, i64)>,
}

fn project(s: &ReviewSnapshot) -> CanonicalSnapshot {
    CanonicalSnapshot {
        id: s.id.to_str(),
        window_start_ms: s.window_start_ms,
        window_end_ms: s.window_end_ms,
        completed: s.totals.completed,
        note: s.note.clone(),
        streams: s
            .streams
            .iter()
            .map(|r| (r.stream.to_str(), r.completed))
            .collect(),
        streaks: s
            .streaks
            .iter()
            .map(|r| (r.routine.to_str(), r.streak))
            .collect(),
    }
}

async fn history(core: &Core) -> Vec<CanonicalSnapshot> {
    let rows = match core
        .query(Query::ReviewHistory { limit: 50 })
        .await
        .expect("review history")
    {
        QueryResult::ReviewSnapshots(v) => v,
        other => panic!("expected ReviewSnapshots, got {other:?}"),
    };
    let mut out: Vec<CanonicalSnapshot> = rows.iter().map(project).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Comparable projection of one feed row.
///
/// Deliberately drops `op_id`. An op's log id is **replica-local**: the device
/// that authored it mints a ULID, while every receiver derives its key from
/// `(stream, device, seq)`. What must agree across replicas is the story — when,
/// on which device, about what — not the local primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalEvent {
    at_ms: u64,
    device: [u8; 16],
    entity: String,
    label: String,
    verb: &'static str,
}

fn project_event(e: &ActivityEvent) -> CanonicalEvent {
    CanonicalEvent {
        at_ms: e.at_ms,
        device: e.device,
        entity: e.entity.to_str(),
        label: e.label.clone(),
        verb: e.kind.verb(),
    }
}

async fn canonical_timeline(core: &Core, entity: EntityRef) -> Vec<CanonicalEvent> {
    timeline(core, entity)
        .await
        .iter()
        .map(project_event)
        .collect()
}

async fn timeline(core: &Core, entity: EntityRef) -> Vec<ActivityEvent> {
    match core
        .query(Query::ActivityTimeline { entity, limit: 100 })
        .await
        .expect("activity timeline")
    {
        QueryResult::Activity(v) => v,
        other => panic!("expected Activity, got {other:?}"),
    }
}

/// A snapshot draft that names a Stream by id without that Stream needing to
/// exist anywhere — the counts are opaque payload, which is what lets the op
/// overtake the entities it describes.
fn draft(note: &str, phantom_stream: EntityRef, completed: u32) -> ReviewSnapshotDraft {
    ReviewSnapshotDraft {
        window_start_ms: WINDOW_START_MS,
        window_end_ms: WINDOW_END_MS,
        totals: ReviewTotals {
            completed,
            deferred: 1,
            dropped: 0,
            created: 2,
            reopened: 0,
        },
        streams: vec![ReviewSnapshotStream {
            stream: phantom_stream,
            name: "Work".into(),
            completed,
            deferred: 1,
            created: 2,
        }],
        streaks: vec![StreakRow {
            routine: EntityRef::new(EntityKind::Routine, [0xC3; 16]),
            title: "Stretch".into(),
            streak: 9,
            last_completed_at_ms: Some(WINDOW_START_MS),
        }],
        note: Some(note.to_string()),
    }
}

async fn wait_history_converges(a: &Core, b: &Core, expected: usize) -> Vec<CanonicalSnapshot> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(TIMEOUT, async {
        loop {
            last_a = history(a).await;
            last_b = history(b).await;
            if last_a.len() == expected && last_a == last_b {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(rows) = res else {
        panic!(
            "review history did not converge to {expected} rows\n  A ({}): {last_a:#?}\n  B ({}): {last_b:#?}",
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

#[tokio::test(flavor = "multi_thread")]
async fn two_core_review_convergence() {
    let (addr, _relay) = sunrise_e2e::spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = sunrise_e2e::open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = sunrise_e2e::open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    sunrise_e2e::trust_each_other(&a, &b).await;
    sunrise_e2e::wait_live(&a, TIMEOUT).await;
    sunrise_e2e::wait_live(&b, TIMEOUT).await;

    // --- 1. BOTH devices finish a review of the SAME week, before any of the
    //        work they describe has converged. The Stream they name has never
    //        been created on either replica: a snapshot must not depend on the
    //        entities it counts having arrived. ---
    let phantom = EntityRef::new(EntityKind::Stream, [0x5E; 16]);
    let snap_a = a
        .submit(Command::SaveReviewSnapshot(draft(
            "A: felt scattered",
            phantom,
            3,
        )))
        .await
        .expect("A saves its review")
        .entity;
    let snap_b = b
        .submit(Command::SaveReviewSnapshot(draft(
            "B: good week",
            phantom,
            5,
        )))
        .await
        .expect("B saves its review")
        .entity;
    assert_ne!(
        snap_a, snap_b,
        "two reviews of one week mint distinct `rvw_` ids"
    );
    assert_eq!(snap_a.kind(), EntityKind::ReviewSnapshot);

    // --- 2. Both survive, on both replicas, with both reflections intact.
    //        This is the property a week-keyed LWW row would lose. ---
    let converged = wait_history_converges(&a, &b, 2).await;
    let mut notes: Vec<Option<String>> = converged.iter().map(|s| s.note.clone()).collect();
    notes.sort();
    assert_eq!(
        notes,
        vec![
            Some("A: felt scattered".to_string()),
            Some("B: good week".to_string())
        ],
        "neither device's reflection overwrote the other's: {converged:#?}"
    );
    for s in &converged {
        assert_eq!(s.window_start_ms, WINDOW_START_MS);
        assert_eq!(s.window_end_ms, WINDOW_END_MS);
        assert_eq!(
            s.streams,
            vec![(phantom.to_str(), s.completed)],
            "the per-stream counts rode across intact even though the Stream does not exist"
        );
        assert_eq!(
            s.streaks,
            vec![(EntityRef::new(EntityKind::Routine, [0xC3; 16]).to_str(), 9)],
            "the streaks the review showed are part of the record"
        );
    }
    let mut completed: Vec<u32> = converged.iter().map(|s| s.completed).collect();
    completed.sort_unstable();
    assert_eq!(completed, vec![3, 5], "each device's own counts survived");

    // --- 3. Re-saving the same review is a NEW event, not an edit. A snapshot
    //        records that a review happened; it is never a register. ---
    a.submit(Command::SaveReviewSnapshot(draft(
        "A: reviewed again",
        phantom,
        3,
    )))
    .await
    .expect("A reviews again");
    let converged = wait_history_converges(&a, &b, 3).await;
    assert_eq!(converged.len(), 3);

    // --- 4. The activity timeline is a fold over the op log, so a task edited
    //        on A reads on B — with A's device id on it. ---
    let task = a
        .submit(Command::CreateTask(TaskDraft {
            title: "the shared piece of work".into(),
            ..Default::default()
        }))
        .await
        .expect("create task")
        .entity;
    sunrise_e2e::wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    a.submit(Command::UpdateTask {
        id: task,
        patch: TaskPatch {
            title: Some("the shared piece of work, renamed".into()),
            priority: Some(Some(1)),
            ..Default::default()
        },
    })
    .await
    .expect("A edits it");

    // Wait for the edit to land on B before B acts. The two ops are then
    // *sequential*, not concurrent, so the feed has one unambiguous reading —
    // concurrent full-state edits are an LWW question, and this test is about
    // the timeline, not about which write wins.
    wait_until("B sees A's edit", || async {
        timeline(&b, task).await.len() == 2
    })
    .await;

    b.submit(Command::UpdateTask {
        id: task,
        patch: TaskPatch {
            state: Some(TaskState::Done),
            ..Default::default()
        },
    })
    .await
    .expect("B completes it");

    // Both replicas end up with the same four-event story: created, the
    // two-field edit, the completion. (The edit and the completion are
    // independent ops from different devices, so the feed carries both.)
    wait_until("both timelines agree", || async {
        let ta = canonical_timeline(&a, task).await;
        let tb = canonical_timeline(&b, task).await;
        ta.len() == 3 && ta == tb
    })
    .await;

    let feed = timeline(&b, task).await;
    let verbs: Vec<&str> = feed.iter().map(|e| e.kind.verb()).collect();
    assert_eq!(
        verbs,
        vec!["task.completed", "task.updated", "task.created"],
        "B's feed tells the whole story, newest first: {feed:#?}"
    );
    assert_eq!(
        feed[1].kind,
        ActivityKind::TaskUpdated { fields: 2 },
        "the field-level edit is summarized with its count"
    );

    let devices: std::collections::BTreeSet<[u8; 16]> = feed.iter().map(|e| e.device).collect();
    assert_eq!(
        devices.len(),
        2,
        "the merged feed attributes each event to the device that authored it: {feed:#?}"
    );

    a.shutdown().await;
    b.shutdown().await;
}
