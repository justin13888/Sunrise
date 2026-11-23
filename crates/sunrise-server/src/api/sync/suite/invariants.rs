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

/// The second stream of the two-stream scenarios below.
const OTHER_STREAM: [u8; 16] = [0x22; 16];

/// Subscribe `id` to [`STREAM_BYTES`] and [`OTHER_STREAM`], in that order,
/// with no cursors.
async fn subscribe_both(client: &Client, id: &str) {
    let streams: Vec<serde_json::Value> = [STREAM_BYTES, OTHER_STREAM]
        .iter()
        .map(|s| serde_json::json!({ "stream_id": hex::encode(s), "cursors": [] }))
        .collect();
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({ "streams": streams })),
            &[("x-sunrise-session", id)],
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

/// Publish an empty batch on `stream`.
async fn publish_on(client: &Client, id: &str, stream: [u8; 16], batch_id: u64) {
    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": hex::encode(stream), "batch_id": batch_id, "ops": [],
            })),
            &[("x-sunrise-session", id)],
        )
        .await;
    res.assert_status(StatusCode::OK);
}

/// Each `ops` event of an SSE body, in order: its stream, and the `id:` it
/// carried.
fn ops_events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut id = None;
            let mut data = None;
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("id:") {
                    id = Some(v.trim().to_owned());
                } else if let Some(v) = line.strip_prefix("data:") {
                    data = serde_json::from_str::<serde_json::Value>(v.trim()).ok();
                }
            }
            let data = data?;
            if data["kind"] != "ops" {
                return None;
            }
            Some((data["stream_id"].as_str()?.to_owned(), id?))
        })
        .collect()
}

/// The `ops` events of `body` on `stream`.
fn on(events: &[(String, String)], stream: [u8; 16]) -> usize {
    let stream = hex::encode(stream);
    events.iter().filter(|(s, _)| *s == stream).count()
}

/// #1263, the replay half. Frame ids are one sequence across every channel,
/// and a session's streams are replayed one after the other, so a client
/// that drops after the first stream's replay holds an id above frames of
/// the second it was never sent. Resuming on that id has to send them.
#[tokio::test]
async fn a_drop_between_two_replayed_streams_resumes_both() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    // Ids 1 and 4 on the first stream, 2 and 3 on the second.
    publish_on(&client, &writer, STREAM_BYTES, 1).await;
    publish_on(&client, &writer, OTHER_STREAM, 2).await;
    publish_on(&client, &writer, OTHER_STREAM, 3).await;
    publish_on(&client, &writer, STREAM_BYTES, 4).await;

    let reader = establish(&client).await;
    subscribe_both(&client, &reader).await;
    let first = ops_events(&read(&client, &reader, &[]).await);
    assert_eq!(
        (on(&first, STREAM_BYTES), on(&first, OTHER_STREAM)),
        (2, 2),
        "{first:?}"
    );

    // The drop: after the last frame of the first stream, before any of the
    // second. This is the id the client holds there.
    let (_, held) = first
        .iter()
        .rfind(|(s, _)| *s == hex::encode(STREAM_BYTES))
        .expect("the first stream replayed")
        .clone();
    let resumed = ops_events(&read(&client, &reader, &[("last-event-id", &held)]).await);
    assert_eq!(
        on(&resumed, OTHER_STREAM),
        2,
        "the second stream's frames were never sent: {resumed:?}"
    );
    assert_eq!(
        on(&resumed, STREAM_BYTES),
        0,
        "the first stream's were: {resumed:?}"
    );
}

/// #1263, the live half. The relay publishes each channel in log order but
/// promises nothing across channels, so a live frame of one stream can reach
/// a session before a lower-id frame of another — one still on the cluster's
/// bus, or on another node. A client that drops right after the higher one
/// has to be sent the lower one when it resumes.
#[tokio::test]
async fn a_drop_after_a_live_frame_overtakes_another_stream_resumes_both() {
    use crate::relay::RelayFrame;
    use http_body_util::BodyExt as _;
    use kynos::http::body::Body;
    use kynos::http::{HeaderName, HeaderValue, Request};

    let state = ServerState::new(ServerConfig::default());
    let client = Client::from_state(state.clone());
    let reader = establish(&client).await;
    subscribe_both(&client, &reader).await;
    // The stream that serves the set: nothing to replay, no id yet.
    let _ = read(&client, &reader, &[]).await;
    let session = client
        .sessions
        .get(&reader, client.clock_now_ms())
        .await
        .expect("the session store answers")
        .expect("the session is live");

    let mut request = Request::new(Body::empty());
    *request.uri_mut() = "/api/v1/sync/events".parse().unwrap();
    request.headers_mut().insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static(BEARER),
    );
    request.headers_mut().insert(
        HeaderName::from_static("x-sunrise-session"),
        HeaderValue::from_str(&reader).unwrap(),
    );
    let mut body = client.call(request).await.into_body();
    let mut seen = String::new();
    let mut next = async |seen: &mut String| {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .expect("the stream sent nothing")
            .expect("the stream ended")
            .expect("a readable frame");
        if let Some(data) = frame.data_ref() {
            seen.push_str(&String::from_utf8_lossy(data));
        }
    };
    while seen.matches("\"kind\":\"caught_up\"").count() < 2 {
        next(&mut seen).await;
    }

    // Both frames commit, the second stream's first, so its id is the lower.
    let append = |stream: [u8; 16], batch_id: u64| {
        state.store.relay_append(
            (session.account, stream),
            b"frame",
            &[],
            0,
            None,
            batch_id,
            1_000,
            crate::relay_log::DurableCaps::default(),
        )
    };
    let id_of = |appended| match appended {
        crate::relay_log::Appended::Fresh { frame_id, .. } => frame_id,
        crate::relay_log::Appended::Duplicate { .. } => panic!("a fresh frame"),
    };
    let lower = id_of(append(OTHER_STREAM, 1).await.expect("appended"));
    let higher = id_of(append(STREAM_BYTES, 2).await.expect("appended"));
    assert!(lower < higher);
    // Only the higher one reaches this stream live.
    state.relay.publish(
        (session.account, STREAM_BYTES),
        RelayFrame {
            id: higher,
            ..RelayFrame::opaque(session.conn.wrapping_add(1), b"frame".to_vec())
        },
    );
    while ops_events(&seen).is_empty() {
        next(&mut seen).await;
    }
    let live = ops_events(&seen);
    assert_eq!(on(&live, STREAM_BYTES), 1, "{live:?}");
    // The drop.
    drop(body);

    let (_, held) = live[0].clone();
    let resumed = ops_events(&read(&client, &reader, &[("last-event-id", &held)]).await);
    assert_eq!(
        on(&resumed, OTHER_STREAM),
        1,
        "the lower frame of the other stream was never sent: {resumed:?}"
    );
    assert_eq!(
        on(&resumed, STREAM_BYTES),
        0,
        "the live frame was: {resumed:?}"
    );
}

/// An id the session cannot apply resumes nothing: a bare id cannot say
/// which of two streams it is about, so every stream replays from its
/// cursors. Over-delivery is a no-op at the op-log gate; a skip is loss.
#[tokio::test]
async fn a_bare_id_on_a_two_stream_session_replays_from_the_cursors() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    publish_on(&client, &writer, STREAM_BYTES, 1).await;
    publish_on(&client, &writer, OTHER_STREAM, 2).await;

    let reader = establish(&client).await;
    subscribe_both(&client, &reader).await;
    let _ = read(&client, &reader, &[]).await;

    let resumed = ops_events(&read(&client, &reader, &[("last-event-id", "2")]).await);
    assert_eq!(
        (on(&resumed, STREAM_BYTES), on(&resumed, OTHER_STREAM)),
        (1, 1),
        "{resumed:?}"
    );
}

/// And a position from another stream set — the same two streams, listed
/// the other way round by a later `Subscribe` — is not read against this
/// one, where its indices would name the other stream.
#[tokio::test]
async fn a_position_from_another_stream_set_replays_from_the_cursors() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    publish_on(&client, &writer, STREAM_BYTES, 1).await;
    publish_on(&client, &writer, OTHER_STREAM, 2).await;

    let reader = establish(&client).await;
    subscribe_both(&client, &reader).await;
    let first = ops_events(&read(&client, &reader, &[]).await);
    let (_, held) = first.last().expect("a replay").clone();

    // The same streams, the other way round.
    let streams: Vec<serde_json::Value> = [OTHER_STREAM, STREAM_BYTES]
        .iter()
        .map(|s| serde_json::json!({ "stream_id": hex::encode(s), "cursors": [] }))
        .collect();
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({ "streams": streams })),
            &[("x-sunrise-session", &reader)],
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let _ = read(&client, &reader, &[]).await;

    let resumed = ops_events(&read(&client, &reader, &[("last-event-id", &held)]).await);
    assert_eq!(
        (on(&resumed, STREAM_BYTES), on(&resumed, OTHER_STREAM)),
        (1, 1),
        "{resumed:?}"
    );
}
