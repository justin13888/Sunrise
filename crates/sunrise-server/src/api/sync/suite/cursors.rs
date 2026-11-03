//! Cursor-driven replay: what a subscribe asks for, the gap report, and the
//! durable log behind the ring. Was `ws_cursors`.

use super::*;

/// Was `no_cursor_still_replays_the_whole_ring`.
#[tokio::test]
async fn no_cursor_replays_everything_retained() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    for batch_id in 1..=3u64 {
        assert_eq!(
            publish(&client, &id, vec![], batch_id).await,
            StatusCode::OK
        );
    }

    let body = read(&client, &id, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        3,
        "every retained frame must replay: {body}"
    );
}

/// Was `a_cursor_narrows_the_replay_to_what_the_subscriber_missed`.
#[tokio::test]
async fn a_cursor_narrows_the_replay_to_what_was_missed() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [7u8; 16];

    for seq in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, seq)], seq).await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    subscribe(&client, &reader, Some((hex::encode(device), 2))).await;

    let body = read(&client, &reader, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        1,
        "only what the cursor does not cover may replay: {body}"
    );
}

/// #459: right after a stream's first replayed frame, the relay says how
/// many ops the rest of the replay will send, counted from the frames it
/// read, so the number follows the cursor. A stream with nothing to
/// replay states nothing: zero adds nothing to a total. It never goes
/// out ahead of the first frame, and always ahead of the marker.
#[tokio::test]
async fn the_replay_states_how_many_ops_it_is_about_to_send() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [6u8; 16];
    for seq in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, seq)], seq).await,
            StatusCode::OK
        );
    }

    for (cursor, stated) in [(None, 2), (Some(1), 1), (Some(2), 0)] {
        let reader = establish(&client).await;
        subscribe(
            &client,
            &reader,
            cursor.map(|seq| (hex::encode(device), seq)),
        )
        .await;
        let body = read(&client, &reader, &[]).await;
        let backlog = format!(
            r#""kind":"backlog","stream_id":"{}","ops":{stated}"#,
            stream_hex()
        );
        assert_eq!(body.matches(r#""kind":"backlog""#).count(), 1, "{body}");
        let stated_at = body
            .find(&backlog)
            .unwrap_or_else(|| panic!("{backlog} in {body}"));
        let first_ops = body.find(r#""kind":"ops""#).expect("a replay");
        assert!(first_ops < stated_at, "{body}");
        if let Some(second) = body[first_ops + 1..].find(r#""kind":"ops""#) {
            assert!(stated_at < first_ops + 1 + second, "{body}");
        }
        let caught_at = body.find(r#""kind":"caught_up""#).expect("a marker");
        assert!(stated_at < caught_at, "{body}");
    }

    let caught_up = establish(&client).await;
    subscribe(&client, &caught_up, Some((hex::encode(device), 3))).await;
    let body = read(&client, &caught_up, &[]).await;
    assert!(!body.contains(r#""kind":"backlog""#), "{body}");
    assert!(body.contains(r#""kind":"caught_up""#), "{body}");
}

/// ADR-0045 §5, issue #329: an envelope one container newer, at a floor
/// this relay implements, is routed by its header like any other. The
/// cursor narrows its replay, which it can only do if the relay read the
/// device and seq out of it.
#[tokio::test]
async fn a_newer_container_at_this_floor_is_routed_by_its_header() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [9u8; 16];

    for seq in 1..=3u64 {
        assert_eq!(
            publish(
                &client,
                &writer,
                vec![newer_container_envelope(device, seq)],
                seq
            )
            .await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    subscribe(&client, &reader, Some((hex::encode(device), 2))).await;

    let body = read(&client, &reader, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        1,
        "the relay must read the newer container's header to narrow the replay: {body}"
    );
}

/// Was `eviction_of_already_applied_ops_is_not_a_gap`.
#[tokio::test]
async fn a_cursor_at_the_head_replays_nothing_and_reports_no_gap() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [8u8; 16];
    assert_eq!(
        publish(&client, &writer, vec![envelope(device, 1)], 1).await,
        StatusCode::OK
    );

    let reader = establish(&client).await;
    subscribe(&client, &reader, Some((hex::encode(device), 1))).await;

    let body = read(&client, &reader, &[]).await;
    assert!(
        !body.contains("\"kind\":\"ops\""),
        "nothing was missed: {body}"
    );
    assert!(
        !body.contains("\"kind\":\"gap\""),
        "an applied op falling out of retention is not a loss: {body}"
    );
    assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
}

/// Was `a_cursor_past_durable_retention_gets_a_typed_gap_not_silence`.
///
/// The load-bearing half is the ordering: the gap precedes the partial
/// replay, so a client cannot read `caught_up` as `complete`.
#[tokio::test]
async fn a_cursor_past_retention_gets_a_typed_gap_before_the_replay() {
    let state = ServerState::new(ServerConfig::default())
        .with_ring_caps(RingCaps {
            max_frames: 1,
            max_bytes: 1 << 20,
        })
        .with_durable_caps(DurableCaps {
            max_bytes: 1,
            max_age_ms: u64::MAX,
        });
    let client = Client::from_state(state);
    let writer = establish(&client).await;
    let device = [9u8; 16];

    for seq in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, seq)], seq).await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    subscribe(&client, &reader, Some((hex::encode(device), 0))).await;

    let body = read(&client, &reader, &[]).await;
    assert!(
        body.contains("\"kind\":\"gap\""),
        "eviction past a cursor must be reported, not passed over: {body}"
    );
    let gap_at = body.find("\"kind\":\"gap\"").expect("a gap");
    let caught_at = body.find("\"kind\":\"caught_up\"").expect("a marker");
    assert!(
        gap_at < caught_at,
        "a gap must precede the caught-up it qualifies: {body}"
    );
}

/// Was `resubscribing_replaces_the_receiver_rather_than_duplicating_it`.
///
/// The fan-out assertion first, because it is the fact a client can
/// observe. `spawn_stream` walks `session.streams` and replays each entry,
/// so a set that grew on the second `Subscribe` delivers the same batch
/// once per entry — a subscriber re-applying every op it is sent twice.
/// Only the stored-set length was ever asserted, read out of the session
/// store through the harness, so the behaviour itself was unpinned and the
/// test would fail for a reason that is not a regression the moment that
/// field moves.
#[tokio::test]
async fn resubscribing_replaces_the_stream_set() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;

    subscribe(&client, &id, None).await;
    subscribe(&client, &id, None).await;

    assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);
    let body = read(&client, &id, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        1,
        "one publish must reach a twice-subscribed session once: {body}"
    );

    let session = client
        .sessions
        .get(&id, client.clock_now_ms())
        .await
        .unwrap()
        .expect("a live session");
    assert_eq!(
        session.streams.len(),
        1,
        "a second subscribe replaces rather than duplicating"
    );
}

/// Was `a_cursor_past_the_ring_bound_is_served_from_the_durable_log` and
/// `ops_survive_a_relay_restart`.
///
/// The in-memory ring is bounded to one frame, so anything replayed past
/// that came off the disk — which is the property a restart relies on.
#[tokio::test]
async fn a_replay_past_the_ring_bound_is_served_from_the_durable_log() {
    let state = ServerState::new(ServerConfig::default()).with_ring_caps(RingCaps {
        max_frames: 1,
        max_bytes: 1 << 20,
    });
    let client = Client::from_state(state);
    let writer = establish(&client).await;

    for batch_id in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![], batch_id).await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    subscribe(&client, &reader, None).await;

    let body = read(&client, &reader, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        3,
        "the durable log, not the ring, is the authority for replay: {body}"
    );
}
