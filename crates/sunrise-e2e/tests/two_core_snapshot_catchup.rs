//! Issue #462: a device the relay's op retention has left behind catches up
//! from the stream's snapshot record instead of latching `Degraded`.
//!
//! Real `Core`s, the real relay with its durable log trimmed to a few frames
//! per channel, and the real SSE transport. The compactor seals its snapshot
//! record, uploads it through the blob routes and pins it
//! (`PUT /api/v1/snapshots/{stream_id}`); the device that comes back is told
//! `SYNC_CURSOR_GAP`, fetches the pinned record, applies it, and re-subscribes
//! from the cursors it set (ADR-0068).
//!
//! What each case pins:
//!
//! * [`a_device_offline_past_the_relays_retention_converges_from_the_snapshot`]
//!   — the returning device holds the same tasks and the same conflicts as the
//!   one that never left, the relay did report a gap, and the device is `Live`,
//!   not `Degraded`. A concurrent retitle by a third device while it was away
//!   puts merge losers into the state the record has to carry.
//! * [`a_new_device_joining_an_old_account_catches_up_from_the_snapshot`] —
//!   a vault with no cursors at all, whose first subscribe is below everything
//!   the relay still holds.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, CompactionPolicy, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{TaskDraft, TaskPatch, INBOX_STREAM_BYTES};
use sunrise_e2e::{
    canonical_tasks, open_core_offline, open_paired_core, open_synced_core, spawn_relay_with,
    wait_live, wait_pending_zero, wait_tasks_converge,
};
use sunrise_id::EntityRef;
use sunrise_server::relay::RingCaps;
use sunrise_server::relay_log::DurableCaps;
use sunrise_server::{Metrics, ServerConfig};
use sunrise_sync::{SseTransport, SyncState, Transport};

const ROOT: [u8; 32] = [0x42; 32];
const TIMEOUT: Duration = Duration::from_secs(60);

/// Ciphertext the relay keeps per channel: a handful of task frames, so a
/// few dozen tasks written while a device is away evict what it last saw.
const KEPT_BYTES: u64 = 12 * 1024;

/// The relay, trimmed hard, and the metrics registry it reports into.
async fn trimmed_relay() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, Metrics) {
    let mut metrics = None;
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| {
        metrics = Some(s.metrics.clone());
        s.with_ring_caps(RingCaps {
            max_frames: 2,
            max_bytes: usize::MAX,
        })
        .with_durable_caps(DurableCaps {
            max_bytes: KEPT_BYTES,
            max_age_ms: u64::MAX,
        })
    })
    .await;
    (addr, relay, metrics.expect("the relay's registry"))
}

/// Make `core` its streams' compactor and let it fold everything it holds:
/// no other device is known to it, and nothing is too young to fold.
fn compact_everything(core: &Core) {
    core.set_compaction_policy(CompactionPolicy {
        retention_ms: 0,
        known_device_window_ms: 1,
        snapshot_every_ms: 0,
    });
}

async fn create_task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

async fn retitle(core: &Core, id: EntityRef, title: &str) {
    core.submit(Command::UpdateTask {
        id,
        patch: TaskPatch {
            title: Some(title.into()),
            ..Default::default()
        },
    })
    .await
    .expect("retitle");
}

async fn conflicts(core: &Core) -> String {
    match core
        .query(Query::Conflicts {
            entity: None,
            include_quiet: true,
        })
        .await
        .expect("conflicts")
    {
        QueryResult::Conflicts(c) => serde_json::to_string(&c).expect("conflicts serialize"),
        other => panic!("expected conflicts, got {other:?}"),
    }
}

async fn sync_state(core: &Core) -> SyncState {
    match core.query(Query::SyncStatus).await.expect("sync status") {
        QueryResult::SyncStatus(s) => s.state,
        other => panic!("expected sync status, got {other:?}"),
    }
}

/// Compact on `core` now and wait until the relay pins the inbox's record:
/// the driver publishes it on its next resync tick.
async fn publish_inbox_snapshot(core: &Core, addr: std::net::SocketAddr) {
    let report = core.compact_op_log().expect("compact");
    assert!(
        report.snapshots_written > 0,
        "the compactor wrote a snapshot record"
    );
    let want = core
        .stream_snapshot(INBOX_STREAM_BYTES)
        .expect("read the record")
        .expect("a record of the inbox");
    let want_id = &sunrise_crypto::ciphertext_hash(std::iter::once(want.as_slice()))[..16];
    let mut relay = SseTransport::connect(&format!("http://{addr}"));
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let pin = relay
            .snapshot_latest(&INBOX_STREAM_BYTES)
            .await
            .expect("ask the relay for its pin");
        if pin.is_some_and(|p| p.blob_id[..] == *want_id) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the compactor never pinned its record"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// B is away while A writes far past what the relay keeps, and C, also on
/// the account, retitles a task concurrently with A. When B returns the
/// relay cannot replay what B missed; the snapshot record does.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_offline_past_the_relays_retention_converges_from_the_snapshot() {
    let (addr, _relay, metrics) = trimmed_relay().await;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (dir_a, dir_b, dir_c) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    let c = open_paired_core(dir_c.path(), &a, addr, clock.clone()).await;
    for core in [&a, &b, &c] {
        wait_live(core, TIMEOUT).await;
    }
    let shared = create_task(&a, "before-b-leaves").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;
    wait_tasks_converge(&a, &c, 1, TIMEOUT).await;

    // B goes away.
    b.shutdown().await;
    drop(b);

    // A and C retitle one task without seeing each other's edit.
    c.shutdown().await;
    drop(c);
    let c = open_core_offline(dir_c.path(), ROOT, addr, clock.clone()).await;
    retitle(&c, shared, "retitled-on-c").await;
    retitle(&a, shared, "retitled-on-a").await;
    wait_pending_zero(&a, TIMEOUT).await;
    c.shutdown().await;
    drop(c);
    let c = open_synced_core(dir_c.path(), ROOT, addr, clock.clone()).await;
    wait_live(&c, TIMEOUT).await;
    wait_pending_zero(&c, TIMEOUT).await;

    // A works alone, far past what the relay keeps.
    for i in 0..40 {
        create_task(&a, &format!("while-b-was-away-{i}")).await;
    }
    wait_pending_zero(&a, TIMEOUT).await;
    wait_tasks_converge(&a, &c, 41, TIMEOUT).await;

    compact_everything(&a);
    publish_inbox_snapshot(&a, addr).await;

    // B returns on its own vault, with the cursors it left with.
    let gaps_before = metrics.get("sunrise_relay_cursor_gap_total");
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    wait_live(&b, TIMEOUT).await;
    wait_tasks_converge(&a, &b, 41, TIMEOUT).await;

    assert!(
        metrics.get("sunrise_relay_cursor_gap_total") > gaps_before,
        "the relay could not replay what B missed: B caught up from the record"
    );
    assert_eq!(canonical_tasks(&a).await, canonical_tasks(&b).await);
    assert_eq!(
        conflicts(&a).await,
        conflicts(&b).await,
        "the record carries the merge state, losers included"
    );
    assert_eq!(
        sync_state(&b).await,
        SyncState::Live,
        "a gap a snapshot covers is not a loss"
    );

    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

/// A device paired after the relay has trimmed the account's early history
/// subscribes with no cursors at all, and every stream it asks for starts
/// below what the relay holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_joining_an_old_account_catches_up_from_the_snapshot() {
    let (addr, _relay, metrics) = trimmed_relay().await;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (dir_a, dir_d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    for i in 0..40 {
        create_task(&a, &format!("history-{i}")).await;
    }
    wait_pending_zero(&a, TIMEOUT).await;
    compact_everything(&a);
    publish_inbox_snapshot(&a, addr).await;

    let gaps_before = metrics.get("sunrise_relay_cursor_gap_total");
    let d = open_paired_core(dir_d.path(), &a, addr, clock.clone()).await;
    wait_live(&d, TIMEOUT).await;
    wait_tasks_converge(&a, &d, 40, TIMEOUT).await;
    assert!(metrics.get("sunrise_relay_cursor_gap_total") > gaps_before);
    assert_eq!(canonical_tasks(&a).await, canonical_tasks(&d).await);
    assert_eq!(sync_state(&d).await, SyncState::Live);

    a.shutdown().await;
    d.shutdown().await;
}
