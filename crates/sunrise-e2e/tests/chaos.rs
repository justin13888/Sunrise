//! Chaos convergence scenarios: two real `Core`s syncing through the real relay
//! over fault-injected transports ([`Toxic`](sunrise_e2e::chaos::Toxic)), each
//! asserting byte-identical canonical state once the network heals.
//!
//! # Why the convergence survives faults
//!
//! The driver recovers *within* a session, so these scenarios heal the network
//! and then simply wait — no test-driven reconnect is involved, which is what
//! makes them meaningful. Two mechanisms do the work (issue #20):
//!
//! - **Outbound**, an `OpBatch` that goes unacked is retransmitted on a backoff.
//!   Retransmission is safe because the receiver's `OpLog` idempotence gate,
//!   keyed by op id, dedupes any batch that actually did land.
//! - **Inbound**, nothing acks a frame, so a dropped one leaves no trace and no
//!   outbound retry can help. The session instead re-subscribes on a timer with
//!   its current cursors, and the relay replays whatever those cursors do not
//!   cover.
//!
//! Before that, an op whose `OpBatch` or returning `Ack` was dropped stayed
//! stranded in the outbox until the session *ended*, so each scenario had to
//! manufacture a reconnect with a partition blip to make progress. That
//! workaround is gone, and its absence is the assertion: if a scenario below
//! converges, it converged over a link that was never cycled.
//!
//! # Determinism
//!
//! Every `Toxic` RNG stream is seeded deterministically from [`base_seed`]
//! (honouring `SUNRISE_FUZZ_SEED`) xor a per-scenario tag. Scenarios never issue
//! two conflicting writes to the same task from both replicas, so there are no
//! last-writer-wins ties whose outcome would depend on the wall clock — the
//! converged state is a pure function of the ops, not of timing.

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{TaskDraft, TaskPatch};
use sunrise_e2e::chaos::{seed_from_env, ToxicConfig, DEFAULT_FUZZ_SEED};
use sunrise_e2e::{
    canonical_tasks, open_core_with_factory, spawn_relay, toxic_ws_factory, trust_each_other,
    wait_live, wait_pending_zero, wait_tasks_converge, ws_factory,
};
use sunrise_id::EntityRef;
use sunrise_sync::SyncState;

/// Shared paired-device vault root (both replicas key per-stream keys from it).
const ROOT: [u8; 32] = [0x42; 32];

/// Generous CI-safe cap for every eventual-state wait; healthy runs finish in
/// well under a second per wait.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Base RNG seed, honouring the `SUNRISE_FUZZ_SEED` convention with a fixed
/// default. Xor a per-scenario tag so scenarios draw independent fault streams.
fn base_seed(tag: u64) -> u64 {
    seed_from_env().unwrap_or(DEFAULT_FUZZ_SEED) ^ tag
}

// ---------------------------------------------------------------------------
// Command helpers (all tasks land on the shared inbox stream)
// ---------------------------------------------------------------------------

async fn create_task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
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

async fn complete(core: &Core, id: EntityRef) {
    core.submit(Command::CompleteTask(id))
        .await
        .expect("complete task");
}

async fn task_count(core: &Core) -> usize {
    canonical_tasks(core).await.len()
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Assert both replicas expose identical canonical task tables (the byte-for-
/// byte convergence unit). Call only once traffic has settled.
async fn assert_converged(a: &Core, b: &Core) {
    let ca = canonical_tasks(a).await;
    let cb = canonical_tasks(b).await;
    assert_eq!(ca, cb, "replicas converged to identical canonical tables");
}

/// Assert the core is `Live` with a fully-drained outbox.
async fn assert_live_and_drained(core: &Core) {
    match core.query(Query::SyncStatus).await.expect("sync status") {
        QueryResult::SyncStatus(s) => {
            assert_eq!(s.state, SyncState::Live, "ends Live");
            assert_eq!(s.outbox_pending, 0, "outbox fully drained");
        }
        other => panic!("expected SyncStatus, got {other:?}"),
    }
}

/// Poll `core`'s task count over `window`, asserting it never exceeds `max`.
/// Guards the "corrupted data never materializes" invariant during the run.
async fn assert_count_never_exceeds(core: &Core, max: usize, window: Duration) {
    let deadline = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < deadline {
        let n = task_count(core).await;
        assert!(n <= max, "task count {n} exceeded max {max} mid-run");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// Scenario (a): heavy frame drop, both directions, both links.
// ---------------------------------------------------------------------------

/// A creates 10 tasks and B creates 5 while 30% of frames drop in both
/// directions on both links. Dropped `OpBatch`/`Ack` frames strand ops, but a
/// heal (drop→0) plus a forced reconnect re-drains every stranded op and the
/// ring replay backfills what each peer missed, converging to all 15.
#[tokio::test(flavor = "multi_thread")]
async fn drop_heavy_converges() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Establish clean sessions first (a dropped Hello would deadlock a handshake
    // — a state neither a partition nor a poke can break), then inject drops.
    let (fa, ha) = toxic_ws_factory(addr, ToxicConfig::passthrough(), base_seed(0xA));
    let (fb, hb) = toxic_ws_factory(addr, ToxicConfig::passthrough(), base_seed(0xB));
    let a = open_core_with_factory(dir_a.path(), ROOT, addr, clock.clone(), fa).await;
    let b = open_core_with_factory(dir_b.path(), ROOT, addr, clock.clone(), fb).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // Faults ON: 30% frame drop in both directions on both links.
    ha.set_drop_prob(0.3);
    hb.set_drop_prob(0.3);

    let mut a_tasks = Vec::new();
    for i in 0..10 {
        a_tasks.push(create_task(&a, &format!("a{i}")).await);
    }
    let mut b_tasks = Vec::new();
    for i in 0..5 {
        b_tasks.push(create_task(&b, &format!("b{i}")).await);
    }

    // Let the lossy exchange actually run so frames drop and ops strand.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Heal only. Both sessions stay up: retransmit drains what stranded
    // outbound, resync pulls back what was lost inbound.
    ha.set_drop_prob(0.0);
    hb.set_drop_prob(0.0);

    // Converge, then assert at rest with faults off.
    wait_tasks_converge(&a, &b, 15, TIMEOUT).await;
    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    assert_converged(&a, &b).await;
    assert_live_and_drained(&a).await;
    assert_live_and_drained(&b).await;

    a.shutdown().await;
    b.shutdown().await;
}

// ---------------------------------------------------------------------------
// Scenario (b): corruption on the receiver's inbound path.
// ---------------------------------------------------------------------------

/// A creates 10 tasks; 15% of B's inbound frames get a bit flipped. Corrupted
/// frames fail framing or AEAD verification and are dropped by B — so B's count
/// can never exceed A's created total mid-run, and after heal B converges to
/// exactly A's canonical state.
///
/// Corruption is applied only to B's link: the relay stores op envelopes
/// opaquely (it does not AEAD-verify), so corrupting the *sender's* outbound
/// path would poison the replay ring — a transport-integrity concern outside
/// this scenario, which is about the receiver's own verification rejecting
/// tampered data.
#[tokio::test(flavor = "multi_thread")]
async fn corruption_never_applies() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let fa = ws_factory(addr); // A stays clean → the relay ring stays clean.
    let (fb, hb) = toxic_ws_factory(addr, ToxicConfig::passthrough(), base_seed(0xC));
    let a = open_core_with_factory(dir_a.path(), ROOT, addr, clock.clone(), fa).await;
    let b = open_core_with_factory(dir_b.path(), ROOT, addr, clock.clone(), fb).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // Clean anchor: a converged baseline before corruption is switched on, so
    // the assertions below measure only what the corrupted run did.
    create_task(&a, "anchor").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // Corruption ON, then the remaining 9 tasks (10 total from A).
    hb.set_corrupt_prob(0.15);
    for i in 0..9 {
        create_task(&a, &format!("c{i}")).await;
    }

    // Invariant during the corrupted run: B never materializes more than A made.
    assert_count_never_exceeds(&b, 10, Duration::from_millis(1500)).await;

    // Heal only. B's session stays up; its resync re-subscribes with the
    // cursors it actually reached and the relay replays clean copies of every
    // frame the corruption made it skip.
    hb.set_corrupt_prob(0.0);

    wait_tasks_converge(&a, &b, 10, TIMEOUT).await;
    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    assert_converged(&a, &b).await;
    assert_live_and_drained(&a).await;
    assert_live_and_drained(&b).await;

    a.shutdown().await;
    b.shutdown().await;
}

// ---------------------------------------------------------------------------
// Scenario (c): uniform delay, both directions.
// ---------------------------------------------------------------------------

/// Every frame is held 50–300 ms in both directions on both links. Delay never
/// strands (frames still arrive, in order, so acks return and cursors advance);
/// a mix of 10 ops — creates, completes, and title updates — still converges to
/// identical canonical state, just later.
#[tokio::test(flavor = "multi_thread")]
async fn delay_preserves_convergence() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let cfg = ToxicConfig {
        drop_prob: 0.0,
        corrupt_prob: 0.0,
        delay: Some((Duration::from_millis(50), Duration::from_millis(300))),
    };
    let (fa, _ha) = toxic_ws_factory(addr, cfg, base_seed(0xD));
    let (fb, _hb) = toxic_ws_factory(addr, cfg, base_seed(0xE));
    let a = open_core_with_factory(dir_a.path(), ROOT, addr, clock.clone(), fa).await;
    let b = open_core_with_factory(dir_b.path(), ROOT, addr, clock.clone(), fb).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // 10 mixed ops: 6 creates (→ 6 tasks), 2 completes, 2 title updates.
    let mut ta = Vec::new();
    for i in 0..5 {
        ta.push(create_task(&a, &format!("d{i}")).await); // 5 creates on A
    }
    let tb0 = create_task(&b, "d-b").await; // 1 create on B → 6 tasks
    complete(&a, ta[0]).await; // op 7
    complete(&a, ta[1]).await; // op 8
    set_title(&a, ta[2], "d2-edited").await; // op 9
    set_title(&b, tb0, "d-b-edited").await; // op 10

    wait_tasks_converge(&a, &b, 6, TIMEOUT).await;
    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    assert_converged(&a, &b).await;
    assert_live_and_drained(&a).await;
    assert_live_and_drained(&b).await;

    a.shutdown().await;
    b.shutdown().await;
}

// ---------------------------------------------------------------------------
// Scenario (d): partition, then heal.
// ---------------------------------------------------------------------------

/// Both Live, then B's link is cut. A creates 5 tasks that B cannot see; after a
/// settle window B's count is still unchanged. Healing the partition lets B
/// reconnect and the ring replay backfills all 5, converging both to Live with a
/// drained outbox.
#[tokio::test(flavor = "multi_thread")]
async fn partition_then_heal() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let fa = ws_factory(addr);
    let (fb, hb) = toxic_ws_factory(addr, ToxicConfig::passthrough(), base_seed(0xF));
    let a = open_core_with_factory(dir_a.path(), ROOT, addr, clock.clone(), fa).await;
    let b = open_core_with_factory(dir_b.path(), ROOT, addr, clock.clone(), fb).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // Anchor: converged baseline + a task B can update to collapse its session.
    let anchor = create_task(&a, "anchor").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // Cut B's link and poke it so the (idle) session actually tears down and
    // drops into its failing reconnect loop rather than staying parked in recv.
    hb.partition(true);
    set_title(&b, anchor, "b-partitioned").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A creates 5 tasks while B is cut off.
    for i in 0..5 {
        create_task(&a, &format!("p{i}")).await;
    }
    wait_pending_zero(&a, TIMEOUT).await; // A drained them to the relay.

    // Settle window: B sees none of them — its count stays at the baseline (1).
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        task_count(&b).await,
        1,
        "B saw none of A's partitioned-era tasks"
    );

    // Heal: B reconnects, re-subscribes, and the ring replays the 5.
    hb.partition(false);

    wait_tasks_converge(&a, &b, 6, TIMEOUT).await;
    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    assert_converged(&a, &b).await;
    assert_live_and_drained(&a).await;
    assert_live_and_drained(&b).await;

    a.shutdown().await;
    b.shutdown().await;
}
