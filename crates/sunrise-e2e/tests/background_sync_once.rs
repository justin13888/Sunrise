//! `SunriseCore::sync_once` — what an iOS background refresh or silent push
//! runs — against a real relay.
//!
//! Driven through the seam the Swift app drives, with a peer that writes
//! while the device is "asleep": the device has to come back level with the
//! peer inside its budget, report what changed, stay bounded when the budget
//! is too small, and leave nothing half applied when the OS cancels it
//! mid-drain (`docs/07-clients/mobile-ios.md` §Background sync).

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, SystemClock};
use sunrise_core_bindings::{CoreQuery, CoreQueryResult, SunriseCore};
use sunrise_domain::TaskDraft;
use sunrise_e2e::{open_synced_core, pair_with, spawn_relay, wait_pending_zero};
use sunrise_sync::SyncState;

const ROOT: [u8; 32] = [0x6c; 32];
const TIMEOUT: Duration = Duration::from_secs(30);
/// A budget that comfortably covers a loopback catch-up: an iOS refresh task
/// gets about thirty seconds, and a test that fails should say so before CI's
/// own timeout does.
const BUDGET_MS: u64 = 30_000;

async fn create_task(core: &Core, title: &str) {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task");
}

/// How many times each title appears in `core`'s Inbox.
async fn inbox_titles(core: &SunriseCore) -> HashMap<String, usize> {
    let CoreQueryResult::Tasks { tasks } = core.query(CoreQuery::Inbox).await.expect("inbox")
    else {
        panic!("the Inbox is a task list");
    };
    let mut seen = HashMap::new();
    for task in tasks {
        *seen.entry(task.title).or_insert(0) += 1;
    }
    seen
}

/// The peer's titles, each expected exactly once on the device.
fn exactly_once(titles: &[String]) -> HashMap<String, usize> {
    titles.iter().map(|t| (t.clone(), 1)).collect()
}

/// A paired replica of `peer`'s account that has never run sync: what a
/// device the OS woke from a cold background launch holds.
async fn asleep_replica(peer: &Core, dir: &std::path::Path) -> Arc<SunriseCore> {
    let payload = pair_with(peer);
    let bundle = sunrise_pairing::encode_pairing_payload(&payload).expect("encode the pairing");
    SunriseCore::open(
        dir.to_string_lossy().into_owned(),
        payload.vault_root.to_vec(),
        "0.1.0+e2e".into(),
        Some(bundle),
    )
    .await
    .expect("the paired device opens")
}

/// Each property runs on a replica of its own that has never synced. One
/// replica cannot carry them all: the first call starts the driver, which
/// then keeps draining between calls the way it would in a foreground app —
/// so only a device's first call says what a cold background wake receives.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_sync_is_bounded_idempotent_and_leaves_nothing_half_applied() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // The peer writes while every replica is asleep.
    let dir_peer = tempfile::tempdir().expect("peer vault dir");
    let peer = open_synced_core(dir_peer.path(), ROOT, addr, clock).await;
    let mut titles: Vec<String> = (0..12)
        .map(|i| format!("written while asleep {i}"))
        .collect();
    for title in &titles {
        create_task(&peer, title).await;
    }
    wait_pending_zero(&peer, TIMEOUT).await;

    // 1. A budget too small to finish is still a bounded call: it returns
    //    incomplete, promptly, rather than overrunning what the OS granted.
    let dir_starved = tempfile::tempdir().expect("vault dir");
    let starved_device = asleep_replica(&peer, dir_starved.path()).await;
    let starved = tokio::time::timeout(
        Duration::from_secs(5),
        starved_device.sync_once(url.clone(), None, None, 1),
    )
    .await
    .expect("a 1 ms budget returns long before five seconds")
    .expect("a starved run still returns an outcome");
    assert!(!starved.completed, "{starved:?}");

    // 2. The OS expiring the task mid-drain: the foreign side cancels, which
    //    drops the future. Whatever landed before the drop landed whole —
    //    no row twice, none the peer did not write — and the next run
    //    finishes the job with every task exactly once.
    let dir_expired = tempfile::tempdir().expect("vault dir");
    let expired = asleep_replica(&peer, dir_expired.path()).await;
    let _ = tokio::time::timeout(
        Duration::from_millis(20),
        expired.sync_once(url.clone(), None, None, BUDGET_MS),
    )
    .await;
    for (title, count) in inbox_titles(&expired).await {
        assert!(titles.contains(&title), "an unexpected row {title:?}");
        assert_eq!(count, 1, "{title:?} applied more than once");
    }
    let resumed = expired
        .sync_once(url.clone(), None, None, BUDGET_MS)
        .await
        .expect("the next run");
    assert!(resumed.completed, "{resumed:?}");
    assert_eq!(inbox_titles(&expired).await, exactly_once(&titles));

    // 3. A run with room catches up completely and says it changed things.
    let dir_device = tempfile::tempdir().expect("vault dir");
    let device = asleep_replica(&peer, dir_device.path()).await;
    let caught_up = device
        .sync_once(url.clone(), None, None, BUDGET_MS)
        .await
        .expect("a full run");
    assert!(caught_up.completed, "{caught_up:?}");
    assert_eq!(caught_up.state, SyncState::Live);
    assert_eq!(caught_up.outbox_pending, 0);
    assert!(
        caught_up.changes >= titles.len() as u64,
        "every task the peer wrote is a change: {caught_up:?}"
    );
    assert_eq!(inbox_titles(&device).await, exactly_once(&titles));

    // 4. Idempotent: nothing new on either side completes, changes nothing,
    //    and replays nothing twice. The driver is live here, so this also
    //    proves a live session is replaced rather than trusted — the call
    //    completes only once a session dialled after it has caught up.
    let again = device
        .sync_once(url.clone(), None, None, BUDGET_MS)
        .await
        .expect("a repeat run");
    assert!(again.completed, "{again:?}");
    assert_eq!(again.changes, 0, "{again:?}");
    assert_eq!(inbox_titles(&device).await, exactly_once(&titles));

    // 5. What a silent push is for: the peer writes again and one more run
    //    brings it in. Whether this call or the still-running driver applied
    //    it first is a race the test does not pin, so only the outcome is
    //    asserted, not who counted the change.
    let late = "written after the push".to_owned();
    create_task(&peer, &late).await;
    wait_pending_zero(&peer, TIMEOUT).await;
    let woken = device
        .sync_once(url.clone(), None, None, BUDGET_MS)
        .await
        .expect("the push-triggered run");
    assert!(woken.completed, "{woken:?}");
    titles.push(late);
    assert_eq!(inbox_titles(&device).await, exactly_once(&titles));

    for replica in [device, expired, starved_device] {
        replica.shutdown().await;
    }
    peer.shutdown().await;
    relay.abort();
}

/// Outbound too: what the device wrote while it could not reach the relay is
/// acked by the time a run reports complete.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_sync_drains_the_outbox() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");

    let dir = tempfile::tempdir().expect("vault dir");
    let device = SunriseCore::open(
        dir.path().to_string_lossy().into_owned(),
        ROOT.to_vec(),
        "0.1.0+e2e".into(),
        None,
    )
    .await
    .expect("the device opens");
    for i in 0..5 {
        device
            .capture(format!("captured offline {i}"), "UTC".into())
            .await
            .expect("capture");
    }

    let outcome = device
        .sync_once(url, None, None, BUDGET_MS)
        .await
        .expect("a full run");
    assert!(outcome.completed, "{outcome:?}");
    assert_eq!(outcome.outbox_pending, 0, "{outcome:?}");

    device.shutdown().await;
    relay.abort();
}
