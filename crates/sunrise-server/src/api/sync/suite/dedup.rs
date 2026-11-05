//! A re-sent batch is stored, acked and fanned out once. Was `ws_batch_dedup`.

use super::*;

/// The defect: a client that re-sends a batch it never saw acked — every
/// reconnect re-drains the outbox — had the relay store and fan out a
/// second copy of ops it already held.
#[tokio::test]
async fn a_resent_batch_is_appended_once_and_acked_once() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    let ops = vec![envelope([3u8; 16], 1)];

    let first = publish_acked(&client, &id, ops.clone(), 1).await;
    let second = publish_acked(&client, &id, ops, 1).await;

    assert_eq!(
        first["server_first_seen_ms"], second["server_first_seen_ms"],
        "a re-send is acked with the timestamp the first copy got"
    );
    assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 1);
    // Received work counts the first copy only; the re-send is churn.
    assert_eq!(client.metrics.get("sunrise_sync_ops_received_total"), 1);
    assert_eq!(
        client
            .metrics
            .histogram_count("sunrise_sync_batch_ops", &[]),
        1
    );

    let body = read(&client, &id, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        1,
        "the batch must be retained once, not twice: {body}"
    );
}

/// Why the key cannot be the `batch_id`.
///
/// `sync_driver.rs` initialises its counter inside `session()`, so a
/// reconnect re-sends the very same ops under a *different* number — and,
/// worse, later ops under a number an earlier session already used. Only a
/// content key catches the first and spares the second.
#[tokio::test]
async fn a_resent_batch_reacked_after_a_reconnect_carries_the_first_timestamp() {
    let clock = TestClock::at(T0_MS);
    let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
    let client = Client::from_state(state);
    let ops = vec![envelope([4u8; 16], 9)];

    let first = publish_acked(&client, &establish(&client).await, ops.clone(), 7).await;
    assert_eq!(first["server_first_seen_ms"], T0_MS);

    // A new session, a counter that restarted, and a later clock.
    clock.set(T0_MS + 60_000);
    let second = publish_acked(&client, &establish(&client).await, ops, 1).await;

    assert_eq!(
        second["server_first_seen_ms"], T0_MS,
        "'first seen' is when the relay first saw it, not when the copy arrived"
    );
}

/// The other half of the same rule: the same `batch_id` over different ops
/// is two batches, and reading them as one would be the data loss the
/// `batch_id` key was rejected for.
#[tokio::test]
async fn a_batch_with_different_ops_is_never_deduped() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    publish_acked(&client, &id, vec![envelope([5u8; 16], 1)], 1).await;
    publish_acked(&client, &id, vec![envelope([5u8; 16], 2)], 1).await;

    assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 0);
    let body = read(&client, &id, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        2,
        "two distinct batches must both be retained: {body}"
    );
}

/// The shape ADR-0033 accepted and could not see, now counted.
///
/// A client loses an ack, authors between the loss and the reconnect, and
/// re-drains its outbox in a different partition. The whole-batch content
/// key reads that as new work — correctly, by its own rule — so the batch
/// is stored and fanned out, and `sunrise_relay_batch_duplicate_total`
/// stays at zero while ops are being re-sent. The two counters together are
/// what makes the trade in that ADR falsifiable.
#[tokio::test]
async fn a_repartitioned_resend_is_counted_without_being_refused() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    let device = [6u8; 16];

    publish_acked(
        &client,
        &id,
        vec![envelope(device, 1), envelope(device, 2)],
        1,
    )
    .await;
    // Seq 2 again, this time carried with a newer op: a different hash and
    // a different partition of the same work.
    publish_acked(
        &client,
        &id,
        vec![envelope(device, 2), envelope(device, 3)],
        2,
    )
    .await;

    assert_eq!(
        client.metrics.get("sunrise_relay_batch_duplicate_total"),
        0,
        "the batch key is not supposed to catch this; that is the point"
    );
    assert_eq!(
        client.metrics.get("sunrise_relay_batch_overlap_total"),
        1,
        "an op the channel already held arrived again"
    );

    // Counted, never refused.
    let body = read(&client, &id, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        2,
        "both batches must still be stored and served: {body}"
    );
}

/// The counter measures the near miss and nothing else.
///
/// Ordinary new work and an exact re-send are the two cases dedup already
/// handles, and a counter that fired on either would be unreadable against
/// the append rate.
#[tokio::test]
async fn new_work_and_an_exact_resend_are_not_overlaps() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    let device = [7u8; 16];

    let ops = vec![envelope(device, 1)];
    publish_acked(&client, &id, ops.clone(), 1).await;
    // The same batch again: caught by the content key.
    publish_acked(&client, &id, ops, 1).await;
    // And strictly later ops: new work.
    publish_acked(&client, &id, vec![envelope(device, 2)], 2).await;

    assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 1);
    assert_eq!(
        client.metrics.get("sunrise_relay_batch_overlap_total"),
        0,
        "neither shape is a near miss"
    );
}

/// Retention must not erase the evidence.
///
/// A re-send of an op the channel evicted is still a re-send, and
/// `relay_frame_heads` cannot say so once the frame that carried it is
/// gone. `relay_evicted` outlives it, which is why the head is read from
/// both.
#[tokio::test]
async fn an_op_the_channel_evicted_is_still_an_overlap() {
    let device = [8u8; 16];
    let first = envelope(device, 1);
    // A budget that holds one frame of this size, so the first append is
    // evicted by the second.
    let caps = DurableCaps {
        max_bytes: u64::try_from(first.len()).unwrap_or(u64::MAX),
        ..DurableCaps::default()
    };
    let state = ServerState::new(ServerConfig::default()).with_durable_caps(caps);
    let client = Client::from_state(state);
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    publish_acked(&client, &id, vec![first], 1).await;
    publish_acked(&client, &id, vec![envelope(device, 2)], 2).await;
    assert_eq!(
        client.metrics.get("sunrise_relay_batch_overlap_total"),
        0,
        "seq 2 is new work, whatever retention did to seq 1"
    );

    // Seq 1 again, long after the frame that carried it was evicted.
    publish_acked(&client, &id, vec![envelope(device, 1)], 3).await;
    assert_eq!(
        client.metrics.get("sunrise_relay_batch_overlap_total"),
        1,
        "an evicted op is one the channel held, not one it never saw"
    );
}

/// An empty batch has no content to be the same as. Collapsing them would
/// silently drop the three-event expectation the cursor tests hold.
#[tokio::test]
async fn an_empty_batch_is_never_deduped() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    for batch_id in 1..=3u64 {
        assert_eq!(
            publish(&client, &id, vec![], batch_id).await,
            StatusCode::OK
        );
    }

    assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 0);
    let body = read(&client, &id, &[]).await;
    assert_eq!(body.matches("\"kind\":\"ops\"").count(), 3, "{body}");
}

/// A duplicate must not reach the ring either. Storing it once and fanning
/// it out twice would leave every live subscriber re-applying ops in
/// proportion to how flaky the *sender's* link is.
#[tokio::test]
async fn a_duplicate_is_not_fanned_out_to_a_live_subscriber() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    let ops = vec![envelope([6u8; 16], 1)];

    publish_acked(&client, &id, ops.clone(), 1).await;
    let body = read(&client, &id, &[]).await;
    assert!(body.contains("id: 1"), "the first copy is frame 1: {body}");

    publish_acked(&client, &id, ops, 2).await;

    // Resuming past frame 1 is how a live subscriber that already had the
    // first copy sees the stream: a second frame would show up here.
    let after = read(&client, &id, &[("last-event-id", "1")]).await;
    assert!(
        !after.contains("\"kind\":\"ops\""),
        "a duplicate produced a second frame: {after}"
    );
}
