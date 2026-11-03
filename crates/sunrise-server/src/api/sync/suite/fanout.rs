//! A published batch reaching subscribed streams, and the metrics fan-out
//! records.

use super::*;

/// Was `ws_subscribe_receives_op_batch_fanout`.
#[tokio::test]
async fn a_subscribed_stream_receives_a_published_batch() {
    let client = Client::new(ServerConfig::default());
    let reader = establish(&client).await;
    let writer = establish(&client).await;
    subscribe(&client, &reader, None).await;

    assert_eq!(publish(&client, &writer, vec![], 1).await, StatusCode::OK);

    let body = read(&client, &reader, &[]).await;
    assert!(body.contains("\"kind\":\"ops\""), "no fan-out in: {body}");
}

/// `sunrise_sync_fanout_latency_seconds` (#366): a batch fanned out to a
/// stream that is already open is timed once, and the histogram reaches
/// the scrape.
///
/// The stream opens first and the publish lands inside its read window,
/// so the batch travels the live path; one published before the stream
/// opened would arrive by replay, which is not a fan-out and is not timed.
#[tokio::test]
async fn a_live_fanout_is_timed_and_scraped() {
    const FANOUT: &str = "sunrise_sync_fanout_latency_seconds";
    let client = Client::new(ServerConfig::default());
    let reader = establish(&client).await;
    let writer = establish(&client).await;
    subscribe(&client, &reader, None).await;

    let (body, published) = tokio::join!(read(&client, &reader, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        publish(&client, &writer, vec![], 1).await
    });
    assert_eq!(published, StatusCode::OK);
    assert!(body.contains("\"kind\":\"ops\""), "no fan-out in: {body}");
    assert_eq!(client.metrics.histogram_count(FANOUT, &[]), 1);

    let scrape = client.send_as(Method::GET, "/metrics", None, None).await;
    let text = String::from_utf8_lossy(&scrape.bytes);
    assert!(
        text.contains("# TYPE sunrise_sync_fanout_latency_seconds histogram")
            && text.contains("sunrise_sync_fanout_latency_seconds_count 1"),
        "the histogram is not on /metrics:\n{text}"
    );
}

/// #435: the ops written to a stream are counted on both paths — the
/// durable replay a stream opens with and the live fan-out after it — and
/// a stream is counted as open while it is open.
#[tokio::test]
async fn delivered_ops_and_open_streams_reach_the_scrape() {
    const DELIVERED: &str = "sunrise_sync_ops_delivered_total";
    let client = Client::new(ServerConfig {
        bind: "127.0.0.1:8443".to_owned(),
        ..ServerConfig::default()
    });
    let writer = establish(&client).await;
    let reader = establish(&client).await;
    subscribe(&client, &reader, None).await;
    let device = [7u8; 16];
    // Stored before the stream opens, so the stream replays it.
    assert_eq!(
        publish(
            &client,
            &writer,
            vec![envelope(device, 1), envelope(device, 2)],
            1
        )
        .await,
        StatusCode::OK
    );

    let (body, published, scrape) = tokio::join!(
        read(&client, &reader, &[]),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            publish(&client, &writer, vec![envelope(device, 3)], 2).await
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            client.send_as(Method::GET, "/metrics", None, None).await
        }
    );
    assert_eq!(published, StatusCode::OK);
    assert_eq!(body.matches("\"kind\":\"ops\"").count(), 2, "{body}");
    assert_eq!(client.metrics.get(DELIVERED), 3, "two replayed, one live");
    let text = String::from_utf8_lossy(&scrape.bytes);
    assert!(
        text.contains("sunrise_sync_streams_active 1\n"),
        "the reader's stream was open during the scrape:\n{text}"
    );
    assert!(
        text.contains("# TYPE sunrise_sync_ops_delivered_total counter\n"),
        "{text}"
    );
}

/// #435: a batch that pushes the channel past its durable budget evicts
/// the oldest frame, and the eviction is counted.
#[tokio::test]
async fn a_retention_eviction_is_counted() {
    let state = ServerState::new(ServerConfig::default()).with_durable_caps(DurableCaps {
        max_bytes: 1,
        max_age_ms: u64::MAX,
    });
    let client = Client::from_state(state);
    let writer = establish(&client).await;
    for batch_id in 1..=3u64 {
        assert_eq!(
            publish(
                &client,
                &writer,
                vec![envelope([7u8; 16], batch_id)],
                batch_id
            )
            .await,
            StatusCode::OK
        );
    }
    // A one-byte budget keeps only the newest frame: each append after
    // the first evicts the one before it.
    assert_eq!(client.metrics.get("sunrise_relay_log_evicted_total"), 2);

    let scrape = client.send_as(Method::GET, "/metrics", None, None).await;
    scrape.assert_status(StatusCode::OK);
    let text = String::from_utf8_lossy(&scrape.bytes);
    assert!(
        text.contains(
            "# TYPE sunrise_relay_log_evicted_total counter\nsunrise_relay_log_evicted_total 2\n"
        ),
        "the evictions are not on /metrics:\n{text}"
    );
}

/// A writer that also holds a live stream is one of the receivers the
/// publish counts. Its stream skips its own batch, and that skip has to
/// close its copy on the clock: otherwise the count never reaches zero and
/// the peer's delivery is never observed.
#[tokio::test]
async fn a_fanout_the_writers_own_stream_skips_is_observed_once() {
    const FANOUT: &str = "sunrise_sync_fanout_latency_seconds";
    let client = Client::new(ServerConfig::default());
    let reader = establish(&client).await;
    let writer = establish(&client).await;
    subscribe(&client, &reader, None).await;
    subscribe(&client, &writer, None).await;

    let (to_reader, to_writer, published) = tokio::join!(
        read(&client, &reader, &[]),
        read(&client, &writer, &[]),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            publish(&client, &writer, vec![], 1).await
        }
    );
    assert_eq!(published, StatusCode::OK);
    assert!(
        to_reader.contains("\"kind\":\"ops\""),
        "no fan-out in: {to_reader}"
    );
    assert!(
        !to_writer.contains("\"kind\":\"ops\""),
        "the writer was echoed its own batch: {to_writer}"
    );
    assert_eq!(client.metrics.histogram_count(FANOUT, &[]), 1);
}

/// The writer's own stream is the only one open: the batch reached no
/// peer, so there was no fan-out to time, and the skip records nothing.
#[tokio::test]
async fn a_batch_only_its_writers_stream_received_is_not_observed() {
    const FANOUT: &str = "sunrise_sync_fanout_latency_seconds";
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    subscribe(&client, &writer, None).await;

    let (to_writer, published) = tokio::join!(read(&client, &writer, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        publish(&client, &writer, vec![], 1).await
    });
    assert_eq!(published, StatusCode::OK);
    assert!(
        !to_writer.contains("\"kind\":\"ops\""),
        "the writer was echoed its own batch: {to_writer}"
    );
    assert_eq!(client.metrics.histogram_count(FANOUT, &[]), 0);
}

/// Was `ws_malformed_op_batch_nacked_no_fanout`.
///
/// Shape changed: a `Nack` frame becomes a refusal status, and the body is
/// caught before the handler rather than after a decode.
///
/// The exact status and code rather than `is_client_error`: the loose form
/// was satisfied by any 4xx, including the 401 an unauthenticated request
/// produces and the 422 a schema rejection produces — neither of which is
/// this refusal, and both of which would have hidden the handler no longer
/// decoding the ops at all.
#[tokio::test]
async fn a_malformed_batch_is_refused_and_never_fans_out() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": stream_hex(),
                "batch_id": 1,
                "ops": ["not base64!!"],
            })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json()["code"],
        serde_json::json!(codes::VALIDATION_INVALID)
    );

    let body = read(&client, &id, &[]).await;
    assert!(
        !body.contains("\"kind\":\"ops\""),
        "a refused batch must not fan out: {body}"
    );
}
