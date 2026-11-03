//! The surface's own invariants: the session header, durability before the
//! ack, and the `Last-Event-ID` resume point.

use super::*;

/// Every operation after establishment needs the session id.
#[tokio::test]
async fn an_operation_without_a_session_header_is_refused() {
    let client = Client::new(ServerConfig::default());
    let _ = establish(&client).await;

    client
        .send(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(&serde_json::json!({ "streams": [] })),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// A session id is a bearer-equivalent, so it is checked against the caller
/// rather than merely looked up.
#[tokio::test]
async fn an_unknown_session_is_refused() {
    let client = Client::new(ServerConfig::default());
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({ "streams": [] })),
            &[("x-sunrise-session", "ses_00000000000000000000000000000000")],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// Durable first, then the ack.
#[tokio::test]
async fn a_batch_is_acked_after_it_is_durable() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": stream_hex(), "batch_id": 7, "ops": [],
            })),
            &[("x-sunrise-session", &id)],
        )
        .await;

    res.assert_status(StatusCode::OK);
    let body = res.json();
    assert_eq!(body["batch_id"], 7);
    assert!(body["server_first_seen_ms"].is_u64(), "{body}");
}

/// A stream id that is not 32 hex characters never reaches the relay.
#[tokio::test]
async fn a_malformed_stream_id_is_refused() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;

    client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": "not-a-stream", "batch_id": 1, "ops": [],
            })),
            &[("x-sunrise-session", &id)],
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// The replay, then the marker that completes it, each frame carrying the
/// durable id `Last-Event-ID` resumes from.
#[tokio::test]
async fn the_event_stream_replays_then_reports_caught_up() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

    let body = read(&client, &id, &[]).await;
    assert!(body.contains("\"kind\":\"ops\""), "no replay in: {body}");
    assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
    let ops_at = body.find("\"kind\":\"ops\"").expect("a replay");
    let caught_at = body.find("\"kind\":\"caught_up\"").expect("a marker");
    assert!(ops_at < caught_at, "{body}");
    assert!(body.contains("id: 1"), "{body}");
}

/// `Last-Event-ID` resumes rather than replaying from the start.
///
/// The stream that serves the subscription comes first, because that is
/// where the ids come from: a client cannot hold a `Last-Event-ID` it was
/// never sent, and a resume on a stream that follows another stream is
/// exactly what `SseTransport` does. The test used to resume straight off
/// the `Subscribe`, which is the shape the server now refuses.
#[tokio::test]
async fn a_resumed_stream_does_not_replay_what_it_already_had() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    for batch_id in 1..=3u64 {
        assert_eq!(
            publish(&client, &id, vec![], batch_id).await,
            StatusCode::OK
        );
    }

    let first = read(&client, &id, &[]).await;
    assert!(first.contains("id: 3"), "the ids come from here: {first}");

    let body = read(&client, &id, &[("last-event-id", "2")]).await;
    assert!(!body.contains("id: 1"), "already had: {body}");
    assert!(!body.contains("id: 2"), "already had: {body}");
    assert!(body.contains("id: 3"), "not yet had: {body}");
}

/// The catalogue's `srv.sync.stream_open`, and the field that is the
/// reason for emitting it.
///
/// A plain `#[test]` with its own runtime rather than `#[tokio::test]`,
/// because installing a dispatcher is synchronous and has to wrap the whole
/// exchange rather than sit inside it.
///
/// Both opens are asserted, not just the resumed one: `resumed` is only
/// readable as a ratio, and a field that is always true measures nothing.
#[test]
fn a_stream_open_records_whether_it_resumed() {
    use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};

    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: "info".to_owned(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    tracing::dispatcher::with_default(&dispatch, || {
        rt.block_on(async {
            let client = Client::new(ServerConfig::default());
            let id = establish(&client).await;
            subscribe(&client, &id, None).await;
            assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

            // A cold open first: the ids a resume names come from it.
            let first = read(&client, &id, &[]).await;
            assert!(first.contains("id: 1"), "no id to resume from: {first}");
            let _ = read(&client, &id, &[("last-event-id", "1")]).await;
        });
    });

    let out = cap.contents();
    assert!(!out.is_empty(), "the capture received no records at all");
    let opens: Vec<serde_json::Value> = out
        .lines()
        .filter(|l| l.contains(r#""ev":"srv.sync.stream_open""#))
        .map(|l| serde_json::from_str(l).expect("record is JSON"))
        .collect();
    assert_eq!(opens.len(), 2, "one record per opened stream: {out}");
    assert_eq!(opens[0]["resumed"], serde_json::json!(false), "{out}");
    assert_eq!(opens[1]["resumed"], serde_json::json!(true), "{out}");
    for open in &opens {
        assert!(
            open["account_h"].is_string(),
            "the record is scoped to an account by hash: {open}"
        );
    }
}

/// The defect #101 names, at the boundary.
///
/// A client crashes between receiving a frame and committing it, restarts,
/// re-sends `Subscribe` with the cursors it can actually vouch for, and
/// keeps the id of the last frame it *received*. Id-only selection skips
/// every frame at or below that id, so the ops the cursors asked for are
/// never sent and `caught_up` is reported over the hole. The pair is
/// refused instead, with a code the client can branch on.
#[tokio::test]
async fn a_resume_id_presented_with_a_fresh_subscribe_is_refused() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [9u8; 16];
    for seq in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, seq)], seq).await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    // Received through frame 3, applied only through seq 1.
    subscribe(&client, &reader, Some((hex::encode(device), 1))).await;

    let res = open_events(&client, &reader, &[("last-event-id", "3")]).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json()["code"],
        serde_json::json!("SYNC_RESUME_CONFLICT"),
        "the client has to learn this from a code, not from missing frames"
    );
    assert_eq!(
        client.metrics.get("sunrise_sync_resume_conflict_total"),
        1,
        "an operator watching a third-party client needs to see this"
    );
}

/// And the frames the refusal protected are actually there.
///
/// Without the refusal this is the silent half: the same session, the same
/// cursors, and the two ops it never applied simply never arrive.
#[tokio::test]
async fn dropping_the_id_replays_what_the_cursors_asked_for() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [10u8; 16];
    for seq in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, seq)], seq).await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    subscribe(&client, &reader, Some((hex::encode(device), 1))).await;
    open_events(&client, &reader, &[("last-event-id", "3")])
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let body = read(&client, &reader, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        2,
        "seqs 2 and 3 were received but never applied: {body}"
    );
}

/// A refusal is not a state change: the same request stays refused, so a
/// client that retries without reading the code makes no progress rather
/// than getting the truncated replay on its second attempt.
#[tokio::test]
async fn a_refused_resume_stays_refused_until_the_id_is_dropped() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, Some((hex::encode([11u8; 16]), 1))).await;

    for _ in 0..2 {
        open_events(&client, &id, &[("last-event-id", "1")])
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert_eq!(client.metrics.get("sunrise_sync_resume_conflict_total"), 2);
}

/// Every `Subscribe` voids the resume point, not just the first: a client
/// that restates its cursors mid-session is in the same position as one
/// that restated them at the start.
#[tokio::test]
async fn a_second_subscribe_voids_the_resume_point_again() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;
    assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

    // The stream that serves the first set clears the bar.
    let _ = read(&client, &id, &[]).await;
    subscribe(&client, &id, Some((hex::encode([12u8; 16]), 1))).await;

    open_events(&client, &id, &[("last-event-id", "1")])
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// A zero carries no claim — `relay_replay_after` already reads it as
/// "first connection" — so it is not the ambiguous pair and is served.
#[tokio::test]
async fn a_zero_last_event_id_is_not_a_resume_point() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, Some((hex::encode([13u8; 16]), 1))).await;
    assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

    let body = read(&client, &id, &[("last-event-id", "0")]).await;
    assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
    assert_eq!(client.metrics.get("sunrise_sync_resume_conflict_total"), 0);
}
