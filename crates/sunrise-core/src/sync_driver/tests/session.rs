//! A session's frames against the fake relay: outbox, inbound ops, reconnect,
//! retransmit, resync and cursor gaps.

use super::*;

// ---- Test (a): pending outbox drains, acks, reaches Live ----
#[tokio::test(start_paused = true)]
async fn on_connect_drains_pending_outbox_and_reaches_live() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    // Warm the vault before measuring. A fresh one is not quiet: opening
    // it queues this device's announcement, and the first task in the
    // Inbox mints that stream's key and queues the `key_envelope` op that
    // distributes it. Both are real ops that must reach the relay; they
    // are simply not what this test is about.
    core.submit(Command::CreateTask(TaskDraft {
        title: "warm-up".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    drain_announcement(&core);
    core.submit(Command::CreateTask(TaskDraft {
        title: "pending".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    assert_eq!(core.sync_pending().unwrap(), 1);

    let mut status_rx = core.sync_status();
    let (factory, _server, mut batch_rx) = harness(vec![]);
    core.start_sync(factory).unwrap();

    let rb = timeout(Duration::from_secs(10), batch_rx.recv())
        .await
        .expect("server received a batch")
        .unwrap();
    assert_eq!(rb.ops.len(), 1, "the one pending op is delivered");

    let seq = collect_until_live(&mut status_rx).await;
    assert!(
        seq.contains(&SyncState::CatchingUp),
        "went through CatchingUp: {seq:?}"
    );
    assert_eq!(
        *seq.last().unwrap(),
        SyncState::Live,
        "reached Live: {seq:?}"
    );
    // Reaching Live implies the ack was applied → outbox empty in the DB.
    assert_eq!(core.sync_pending().unwrap(), 0);

    match core.query(Query::SyncStatus).await.unwrap() {
        QueryResult::SyncStatus(s) => {
            assert_eq!(s.state, SyncState::Live);
            assert_eq!(s.outbox_pending, 0);
        }
        other => panic!("expected sync status, got {other:?}"),
    }
}

// ---- Test (b): inbound remote OpBatch materializes + advances cursor ----
#[tokio::test(start_paused = true)]
async fn inbound_remote_op_materializes_and_advances_cursor() {
    let (peer, stream, envs) = make_remote_tasks(&["from B"]).await;
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc_paired(dir.path(), &peer).await;

    let mut changes = core.changes();
    let script = Script {
        inject: vec![peer.meta.clone(), (stream, envs)],
        close_after_subscribe: false,
        ..Default::default()
    };
    let (factory, _server, _batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();

    wait_created(&mut changes).await;
    assert_eq!(inbox_len(&core).await, 1, "remote task materialized");

    // Cursor advanced for device B on the inbox stream.
    let entries = core.sync_subscribe_entries().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.stream_id == stream)
        .expect("inbox stream is subscribed");
    assert!(
        entry.cursors.iter().any(|c| c.last_applied_seq >= 1),
        "cursor advanced: {:?}",
        entry.cursors
    );
}

// ---- Test (c): duplicate inbound delivery is idempotent ----
#[tokio::test(start_paused = true)]
async fn duplicate_inbound_delivery_is_idempotent() {
    let (peer, stream, envs) = make_remote_tasks(&["dup", "sentinel"]).await;
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc_paired(dir.path(), &peer).await;

    let mut changes = core.changes();
    // Deliver the first op twice (duplicate), then a distinct sentinel op.
    let dup = envs[0].clone();
    let sentinel = envs[1].clone();
    let script = Script {
        inject: vec![
            peer.meta.clone(),
            (stream, vec![dup.clone(), dup, sentinel]),
        ],
        close_after_subscribe: false,
        ..Default::default()
    };
    let (factory, _server, _batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();

    // Exactly two Created events: the duplicate second delivery emits none.
    wait_created(&mut changes).await;
    wait_created(&mut changes).await;
    assert_eq!(
        inbox_len(&core).await,
        2,
        "duplicate did not double-materialize"
    );
}

// ---- Test (d): transport drop → reconnect → convergence ----
#[tokio::test(start_paused = true)]
async fn reconnect_after_drop_converges() {
    let (peer, stream, envs) = make_remote_tasks(&["missed"]).await;
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc_paired(dir.path(), &peer).await;

    let mut changes = core.changes();
    // First connection drops right after Subscribe; the missed op only
    // arrives on the second connection.
    let scripts = vec![
        Script {
            inject: vec![],
            close_after_subscribe: true,
            ..Default::default()
        },
        Script {
            inject: vec![peer.meta.clone(), (stream, envs)],
            close_after_subscribe: false,
            ..Default::default()
        },
    ];
    let (factory, server, _batch_rx) = harness(scripts);
    core.start_sync(factory).unwrap();

    wait_created(&mut changes).await;
    assert_eq!(inbox_len(&core).await, 1, "converged after reconnect");
    assert!(
        server.lock().connect_count >= 2,
        "factory was called again to reconnect"
    );
}

// ---- Test (e): submit while Live pushes immediately (no reconnect) ----
#[tokio::test(start_paused = true)]
async fn submit_while_live_pushes_without_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    // Warm the vault before measuring. A fresh one is not quiet: opening
    // it queues this device's announcement, and the first task in the
    // Inbox mints that stream's key and queues the `key_envelope` op that
    // distributes it. Both are real ops that must reach the relay; they
    // are simply not what this test is about.
    core.submit(Command::CreateTask(TaskDraft {
        title: "warm-up".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    drain_announcement(&core);

    let mut status_rx = core.sync_status();
    let (factory, server, mut batch_rx) = harness(vec![]);
    core.start_sync(factory).unwrap();

    let _ = collect_until_live(&mut status_rx).await;
    let connects_before = server.lock().connect_count;

    // Nothing sent yet (no pending outbox on connect).
    core.submit(Command::CreateTask(TaskDraft {
        title: "live submit".into(),
        ..Default::default()
    }))
    .await
    .unwrap();

    let rb = timeout(Duration::from_secs(10), batch_rx.recv())
        .await
        .expect("server received the live submit")
        .unwrap();
    assert_eq!(rb.ops.len(), 1);
    assert_eq!(
        server.lock().connect_count,
        connects_before,
        "no reconnect was needed"
    );
}

// ---- Test (f): an unacked op batch is retransmitted inside the session ----
//
// The heart of issue #20. The server *receives* the batch and deliberately
// withholds the ack, which is a lossy link rather than a broken one: the
// socket stays up, so nothing tears the session down and, before the retry
// path existed, the op sat in the outbox indefinitely while the UI showed
// `Live`. The assertion that matters is not merely that the op arrives, but
// that it arrives *without a reconnect* — recovery has to happen in-session,
// because on a link like this no reconnect is ever coming.
#[tokio::test(start_paused = true)]
async fn unacked_batch_is_retransmitted_within_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    // Warm the vault before measuring. A fresh one is not quiet: opening
    // it queues this device's announcement, and the first task in the
    // Inbox mints that stream's key and queues the `key_envelope` op that
    // distributes it. Both are real ops that must reach the relay; they
    // are simply not what this test is about.
    core.submit(Command::CreateTask(TaskDraft {
        title: "warm-up".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    drain_announcement(&core);

    let mut status_rx = core.sync_status();
    let script = Script {
        swallow_first_batches: 1,
        ..Default::default()
    };
    let (factory, server, mut batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();

    let _ = collect_until_live(&mut status_rx).await;
    let connects_before = server.lock().connect_count;

    core.submit(Command::CreateTask(TaskDraft {
        title: "stranded".into(),
        ..Default::default()
    }))
    .await
    .unwrap();

    // First delivery: swallowed, never acked.
    let first = timeout(Duration::from_secs(10), batch_rx.recv())
        .await
        .expect("server saw the original send")
        .unwrap();
    assert_eq!(first.ops.len(), 1);

    // Second delivery: the retransmit, carrying the same op.
    let second = timeout(Duration::from_secs(10), batch_rx.recv())
        .await
        .expect("server saw the retransmit")
        .unwrap();
    assert_eq!(second.ops, first.ops, "retransmit carried the same op");

    // The retransmit was acked, so the outbox drained.
    wait_pending_zero(&core).await;
    assert_eq!(
        server.lock().connect_count,
        connects_before,
        "recovered in-session: the link was never cycled"
    );
}

// ---- Test (f2): a slow send does not turn a delivered batch into a loss ----
//
// Issue #475. Each `OpBatch` send holds the driver for 300 ms, three times
// the first retransmit delay, with the ack queued while it waits: the SSE
// transport's shape over a link with that round trip. A fresh vault with
// one task owes two streams, so the first drain sends two batches back to
// back, and by the time the second send returns the first batch's
// deadline has passed with its ack sitting unread. The driver must read
// that ack rather than resend: a resend here is a retransmit of a batch
// the relay already acked, and under a sustained commit rate it is the
// loop that exhausted the retry policy and tore the session down.
#[tokio::test(start_paused = true)]
async fn an_ack_queued_during_a_slow_send_is_read_before_the_batch_is_resent() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    core.submit(Command::CreateTask(TaskDraft {
        title: "slow link".into(),
        ..Default::default()
    }))
    .await
    .unwrap();

    let script = Script {
        client_batch_send_hold: Duration::from_millis(300),
        ..Default::default()
    };
    let (factory, server, mut batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();

    wait_pending_zero(&core).await;
    // Past every batch's first retransmit deadline, on paused time, so any
    // resend the driver would make has reached the relay.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut seen: Vec<Vec<Vec<u8>>> = Vec::new();
    while let Ok(batch) = batch_rx.try_recv() {
        assert!(
            !seen.contains(&batch.ops),
            "a batch whose ack was already queued was sent again"
        );
        seen.push(batch.ops);
    }
    assert!(
        seen.len() >= 2,
        "two batches back to back is the case this is about, saw {}",
        seen.len()
    );
    assert_eq!(server.lock().connect_count, 1, "one session throughout");
}

// ---- Test (g): loss evidence pulls a resync forward, in-session ----
//
// Nothing acks an inbound frame, so a dropped one leaves no trace and no
// outbound retry can recover it. The driver instead treats a retransmit as
// evidence that this link is dropping frames in *both* directions and
// re-subscribes early. The injected batch is delivered only on the second
// and later Subscribe, so materializing it proves an in-session resync
// happened — a reconnect would show up as a second connect.
#[tokio::test(start_paused = true)]
async fn loss_evidence_triggers_an_in_session_resync() {
    let (peer, stream, envs) = make_remote_tasks(&["only on resync"]).await;
    let dir = tempfile::tempdir().unwrap();
    // Long timer, so anything observed here came from the evidence path.
    let core = open_arc_paired_with_resync(dir.path(), &peer, Duration::from_secs(300)).await;

    let script = Script {
        // Withholding this ack is what produces the loss evidence.
        swallow_first_batches: 1,
        inject_on_resync: vec![peer.meta.clone(), (stream, envs)],
        ..Default::default()
    };
    let (factory, server, _batch_rx, subs) = harness_counting(vec![script]);
    core.start_sync(factory).unwrap();

    // This local op is what gets swallowed, producing the retransmit that
    // is the loss evidence. It also lands in the inbox, so the assertion
    // below counts two: this one, and the remote op the resync delivers.
    core.submit(Command::CreateTask(TaskDraft {
        title: "provokes a retransmit".into(),
        ..Default::default()
    }))
    .await
    .unwrap();

    wait_inbox_len(&core, 2).await;
    assert!(
        subs.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "a second Subscribe was sent: the resync"
    );
    assert_eq!(
        server.lock().connect_count,
        1,
        "the resync happened inside the original session"
    );
}

// ---- Test (h): the resync timer fires with no evidence at all ----
//
// The evidence path cannot cover the case where the *last* frame in each
// direction is the one that vanished: there is then nothing left to notice
// it. Here the link is clean and nothing is withheld, so no evidence is
// ever produced, and only the timer can account for the resync. The timer
// is the guarantee; evidence is only the latency optimisation.
#[tokio::test(start_paused = true)]
async fn resync_timer_fires_without_any_loss_evidence() {
    let (peer, stream, envs) = make_remote_tasks(&["timer backstop"]).await;
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc_paired_with_resync(dir.path(), &peer, Duration::from_millis(150)).await;

    let script = Script {
        inject_on_resync: vec![peer.meta.clone(), (stream, envs)],
        ..Default::default()
    };
    let (factory, server, _batch_rx, subs) = harness_counting(vec![script]);
    core.start_sync(factory).unwrap();

    // Nothing is ever submitted locally here, so the inbox can only reach
    // one via the injected remote op — which only the resync carries.
    wait_inbox_len(&core, 1).await;
    assert!(
        subs.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "a second Subscribe was sent: the timer resync"
    );
    assert_eq!(
        server.lock().connect_count,
        1,
        "the resync happened inside the original session"
    );
}

// ---- Test (i): a cursor gap degrades the session instead of going Live ----
//
// The relay sends `SYNC_CURSOR_GAP` *before* `CaughtUp` precisely so a
// client cannot read "caught up" as "complete". Until this arm existed the
// driver discarded the Error, accepted the `CaughtUp`, and reported `Live`
// while permanently missing ops the relay can never resend — the same "no
// progress, no signal" failure as issue #20, one layer up. Re-subscribing
// cannot fix it, so the state has to survive the rest of the session.
#[tokio::test(start_paused = true)]
async fn cursor_gap_degrades_the_session_and_never_reports_live() {
    let dir = tempfile::tempdir().unwrap();
    // Short resync timer: proves the degraded latch survives re-subscribes,
    // which are the only thing that could plausibly clear it.
    let core = open_arc_with_resync(dir.path(), Duration::from_millis(100)).await;

    let script = Script {
        gap_on_subscribe: true,
        ..Default::default()
    };
    let mut status_rx = core.sync_status();
    let (factory, _server, _batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();

    wait_state(&core, SyncState::Degraded).await;

    // Hold through several resync cycles: still degraded, never Live.
    for _ in 0..5u32 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let s = sync_state(&core).await;
        assert_eq!(
            s,
            SyncState::Degraded,
            "a gap the relay cannot fill must not resolve to Live"
        );
    }
    assert_never_live(&mut status_rx);
}

/// A gap that names its stream sends the driver to the stream's
/// snapshot record (ADR-0068). With none pinned there is nothing to
/// catch up from, and the session latches exactly as before.
#[tokio::test(start_paused = true)]
async fn a_gap_with_no_snapshot_pinned_still_degrades() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc_with_resync(dir.path(), Duration::from_millis(100)).await;
    let script = Script {
        gap_on_subscribe: true,
        snapshots: SnapshotRoutes::Pinned(None),
        ..Default::default()
    };
    let mut status_rx = core.sync_status();
    let (factory, _server, _batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();
    wait_state(&core, SyncState::Degraded).await;
    for _ in 0..5u32 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(sync_state(&core).await, SyncState::Degraded);
    }
    assert_never_live(&mut status_rx);
}

/// A pinned record that is not one this vault can apply — forged, junk,
/// of another history — is refused by the engine, and the gap stands.
#[tokio::test(start_paused = true)]
async fn a_pinned_record_the_engine_refuses_leaves_the_gap_degraded() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc_with_resync(dir.path(), Duration::from_millis(100)).await;
    let script = Script {
        gap_on_subscribe: true,
        snapshots: SnapshotRoutes::Pinned(Some(b"SR\x04\x00\x01not a record".to_vec())),
        ..Default::default()
    };
    let mut status_rx = core.sync_status();
    let (factory, _server, _batch_rx) = harness(vec![script]);
    core.start_sync(factory).unwrap();
    wait_state(&core, SyncState::Degraded).await;
    for _ in 0..5u32 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(sync_state(&core).await, SyncState::Degraded);
    }
    assert_never_live(&mut status_rx);
}
