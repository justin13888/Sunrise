//! The outcome pass for issues #19 and #20: two real `Core`s, the real relay,
//! real WebSockets, and a device that comes back to find history it could
//! previously never recover.
//!
//! Both scenarios here were **guaranteed data loss** before the durable op log,
//! and neither is detectable from the client side:
//!
//! - Past the in-memory ring bound the frames simply no longer existed.
//! - Across a relay restart the frames *and* the eviction watermarks vanished
//!   together, so the relay could not even tell the client something was
//!   missing. It reported no gap and the client believed it was caught up.
//!
//! Both are driven end to end here rather than at the socket level, because the
//! thing under test is what a *device* ends up holding.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, SystemClock};
use sunrise_domain::TaskDraft;
use sunrise_e2e::{
    canonical_tasks, open_synced_core, spawn_relay_with, trust_each_other, wait_live,
    wait_pending_zero, wait_tasks_converge,
};
use sunrise_server::relay::RingCaps;
use sunrise_server::ServerConfig;
use sunrise_sync::SyncState;

const ROOT: [u8; 32] = [0x42; 32];
const TIMEOUT: Duration = Duration::from_secs(30);

async fn create_task(core: &Core, title: &str) {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task");
}

async fn sync_state(core: &Core) -> SyncState {
    match core
        .query(sunrise_core::Query::SyncStatus)
        .await
        .expect("sync status")
    {
        sunrise_core::QueryResult::SyncStatus(s) => s.state,
        other => panic!("expected sync status, got {other:?}"),
    }
}

/// B is away while A writes far past the relay's in-memory ring. When B
/// returns it must still receive every op — the ring bound is a memory budget,
/// not a retention policy.
#[tokio::test(flavor = "multi_thread")]
async fn a_returning_device_catches_up_past_the_ring_bound() {
    // A ring that holds four frames against thirty ops: everything B needs has
    // certainly been evicted from memory by the time it comes back.
    let (addr, _relay) = spawn_relay_with(ServerConfig::default(), |s| {
        s.with_ring_caps(RingCaps {
            max_frames: 4,
            max_bytes: usize::MAX,
        })
    })
    .await;

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Pair both devices, then take B away.
    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    create_task(&a, "before-b-leaves").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;
    b.shutdown().await;
    drop(b);

    // A works alone, well past the ring bound.
    for i in 0..30 {
        create_task(&a, &format!("while-b-was-away-{i}")).await;
    }
    wait_pending_zero(&a, TIMEOUT).await;

    // B returns on the same vault, so it resubscribes with real cursors.
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    wait_live(&b, TIMEOUT).await;
    wait_tasks_converge(&a, &b, 31, TIMEOUT).await;

    assert_eq!(
        canonical_tasks(&a).await,
        canonical_tasks(&b).await,
        "B recovered every op written while it was away"
    );
    assert_eq!(
        sync_state(&b).await,
        SyncState::Live,
        "nothing was lost, so B must not be degraded"
    );

    a.shutdown().await;
    b.shutdown().await;
}

/// The relay restarts mid-session. Its in-memory ring is gone; the durable log
/// is not. Before it existed this was silent loss — the fresh hub could not
/// distinguish "never held it" from "evicted it", so it reported no gap and B
/// was told it was complete.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_restart_does_not_lose_history() {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join("meta.db");
    let cfg = || ServerConfig {
        sqlite_path: Some(db.clone()),
        ..ServerConfig::default()
    };

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // ---- First relay process. ----
    let (addr, relay) = spawn_relay_with(cfg(), |s| s).await;
    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    create_task(&a, "before-restart").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // B goes away, A keeps writing, then the relay dies with those ops in it.
    b.shutdown().await;
    drop(b);
    for i in 0..5 {
        create_task(&a, &format!("only-on-disk-{i}")).await;
    }
    wait_pending_zero(&a, TIMEOUT).await;
    a.shutdown().await;
    drop(a);
    relay.abort();

    // ---- Second relay process: same database, brand-new hub. ----
    let (addr2, _relay2) = spawn_relay_with(cfg(), |s| s).await;
    let a = open_synced_core(dir_a.path(), ROOT, addr2, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr2, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    wait_tasks_converge(&a, &b, 6, TIMEOUT).await;
    assert_eq!(
        canonical_tasks(&a).await,
        canonical_tasks(&b).await,
        "the ops A wrote before the restart survived it"
    );
    assert_eq!(
        sync_state(&b).await,
        SyncState::Live,
        "a restart that loses nothing must not report a gap"
    );

    a.shutdown().await;
    b.shutdown().await;
}
