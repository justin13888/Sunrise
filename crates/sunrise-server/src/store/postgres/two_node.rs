//! Two relay nodes over one Postgres, end to end through the sync surface
//! (ADR-0062 §2–§4, #481): what a client sees when the node it publishes on is
//! not the node it reads from, and when the node it reads from dies.
//!
//! Each node is a whole `ServerState` with its own pool, hub, listener and
//! request service, driven in process through `api::testing` as the sync
//! suite is; they share nothing but the database. "Killing" a node stops its
//! listener and drops its service, which is everything a dead process takes
//! with it that another node could notice. Needs `SUNRISE_TEST_POSTGRES_URL`,
//! as every test of this backend does.

use std::sync::Arc;
use std::time::Duration;

use kynos::http::{Method, StatusCode};

use super::tests::{isolated, url};
use super::{PgFanout, PgStore};
use crate::api::testing::{register_device, send_signed, Client, BEARER};
use crate::state::{ServerState, SystemClock};
use crate::ServerConfig;

const STREAM: [u8; 16] = [0x11; 16];
const PHONE_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
const LAPTOP_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";

/// How long a test reads a stream: long enough for a commit, a notice and a
/// read on the other node, with room for a slow runner.
const WINDOW: Duration = Duration::from_millis(1500);

/// How long after a stream opens a test publishes into it.
const SETTLE: Duration = Duration::from_millis(300);

/// One node over `store`'s database.
fn node(store: PgStore, config: ServerConfig) -> (Client, PgFanout) {
    let (state, feed) = ServerState::on_postgres(config, store, Arc::new(SystemClock)).unwrap();
    (Client::from_state(state), feed)
}

/// Two nodes over one fresh database, or `None` without one.
async fn cluster(config: &ServerConfig) -> Option<[(Client, PgFanout); 2]> {
    let url = url()?;
    let connect = isolated(&url).await;
    let a = PgStore::open(connect.clone()).await.unwrap();
    let b = PgStore::open(connect).await.unwrap();
    Some([node(a, config.clone()), node(b, config.clone())])
}

fn hello() -> serde_json::Value {
    use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
    serde_json::json!({
        "client_app_v": "0.1.0",
        "client_platform": "test",
        "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
        "doc_schema_min": u32::from(DOC_SCHEMA_V),
        "doc_schema_max": u32::from(DOC_SCHEMA_V),
        "crypto_suite_supported": [u32::from(CRYPTO_SUITE_V)],
        "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0,
        "trace": "01J000000000000000000000000",
    })
}

async fn establish(client: &Client) -> String {
    let res = client
        .send_as(
            Method::POST,
            "/api/v1/sync/session",
            Some(BEARER),
            Some(&hello()),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()["session_id"].as_str().unwrap().to_owned()
}

async fn subscribe(client: &Client, session: &str) {
    let body = serde_json::json!({
        "streams": [{ "stream_id": hex::encode(STREAM), "cursors": [] }]
    });
    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&body),
            &[("x-sunrise-session", session)],
        )
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
}

async fn publish(client: &Client, session: &str, batch_id: u64) -> StatusCode {
    let body = serde_json::json!({
        "stream_id": hex::encode(STREAM),
        "batch_id": batch_id,
        "ops": [],
    });
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&body),
            &[("x-sunrise-session", session)],
        )
        .await
        .status
}

async fn read(client: &Client, session: &str, resume: Option<&str>) -> String {
    let mut headers = vec![("x-sunrise-session", session)];
    if let Some(id) = resume {
        headers.push(("last-event-id", id));
    }
    client
        .read_stream("/api/v1/sync/events", &headers, WINDOW)
        .await
}

/// The event ids of a stream's `ops` events, in the order they arrived.
fn ids(body: &str) -> Vec<u64> {
    body.lines()
        .filter_map(|l| l.strip_prefix("id: "))
        .map(|id| id.trim().parse().unwrap())
        .collect()
}

/// A batch published on node 1 reaches a stream held on node 2, carrying its
/// log id; and a session opened on node 1 is honoured on node 2.
#[tokio::test]
async fn a_batch_published_on_one_node_reaches_a_stream_on_the_other() {
    let Some([(one, feed_one), (two, feed_two)]) = cluster(&ServerConfig::default()).await else {
        return;
    };
    let writer = establish(&one).await;
    let reader = establish(&two).await;
    subscribe(&two, &reader).await;

    let (body, published) = tokio::join!(read(&two, &reader, None), async {
        tokio::time::sleep(SETTLE).await;
        publish(&one, &writer, 1).await
    });
    assert_eq!(published, StatusCode::OK);
    let live = body.split("caught_up").nth(1).expect("caught up");
    assert!(
        live.contains("\"kind\":\"ops\""),
        "no cross-node fan-out: {body}"
    );
    assert_eq!(ids(live).len(), 1, "one frame, with its log id: {body}");

    assert_eq!(
        publish(&two, &writer, 2).await,
        StatusCode::OK,
        "node 2 honours a session node 1 opened"
    );
    feed_one.shutdown();
    feed_two.shutdown();
}

/// A stream on node 1 dies with node 1; its client resumes on node 2 from its
/// last event id and receives what it missed, once each, and nothing twice.
#[tokio::test]
async fn a_stream_resumes_on_another_node_with_no_gap_and_no_duplicate() {
    let Some([(one, feed_one), (two, feed_two)]) = cluster(&ServerConfig::default()).await else {
        return;
    };
    let writer = establish(&one).await;
    let reader = establish(&one).await;
    subscribe(&one, &reader).await;
    assert_eq!(publish(&one, &writer, 1).await, StatusCode::OK);

    let (first, published) = tokio::join!(read(&one, &reader, None), async {
        tokio::time::sleep(SETTLE).await;
        publish(&one, &writer, 2).await
    });
    assert_eq!(published, StatusCode::OK);
    let seen = ids(&first);
    assert_eq!(seen.len(), 2, "one replayed, one live: {first}");
    assert!(seen[0] < seen[1]);

    // Node 1 dies. What is published meanwhile goes through node 2, on the
    // session node 1 opened.
    feed_one.shutdown();
    drop(one);
    assert_eq!(publish(&two, &writer, 3).await, StatusCode::OK);
    assert_eq!(publish(&two, &writer, 4).await, StatusCode::OK);

    let last = seen[1].to_string();
    let resumed = read(&two, &reader, Some(&last)).await;
    let after = ids(&resumed);
    assert_eq!(after.len(), 2, "exactly the two it missed: {resumed}");
    assert!(
        after.iter().all(|id| *id > seen[1]),
        "nothing it already had"
    );
    assert!(after[0] < after[1], "in log order");
    assert!(!resumed.contains("\"kind\":\"gap\""), "{resumed}");
    feed_two.shutdown();
}

/// One pairing message through `client`'s `/api/v1/pairing/{route}`.
async fn pairing(
    client: &Client,
    route: &str,
    body: &serde_json::Value,
) -> crate::api::testing::Res {
    client
        .send_as(
            Method::POST,
            &format!("/api/v1/pairing/{route}"),
            Some(BEARER),
            Some(body),
        )
        .await
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Send `message` as `role`'s message `index` of `pair` through `client`.
async fn pair_send(
    client: &Client,
    pair: &str,
    role: &str,
    index: u32,
    message: &[u8],
) -> crate::api::testing::Res {
    let body = serde_json::json!({
        "pair_id": pair, "role": role, "index": index, "message": b64(message),
    });
    pairing(client, "send", &body).await
}

/// What the other side of `pair` sent from `after` on, read through `client`.
async fn pair_receive(client: &Client, pair: &str, role: &str, after: u32) -> Vec<String> {
    let body = serde_json::json!({ "pair_id": pair, "role": role, "after": after });
    let res = pairing(client, "receive", &body).await;
    res.assert_status(StatusCode::OK);
    res.json()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().to_owned())
        .collect()
}

/// A pairing whose new device reaches node 1 and whose existing device
/// reaches node 2 completes: the existing device trades the code on node 2,
/// and all six messages cross between the nodes in order.
#[tokio::test]
async fn a_pairing_completes_with_its_two_devices_on_different_nodes() {
    let Some([(one, feed_one), (two, feed_two)]) = cluster(&ServerConfig::default()).await else {
        return;
    };
    let pair = b64(&[0x5a; 16]);
    let opened = pair_send(&one, &pair, "new_device", 0, b"noise-1").await;
    opened.assert_status(StatusCode::OK);
    let code = opened.json()["code"].as_str().unwrap().to_owned();

    let resolved = pairing(&two, "resolve", &serde_json::json!({ "code": code })).await;
    resolved.assert_status(StatusCode::OK);
    assert_eq!(resolved.json()["pair_id"], pair.as_str());

    // The exchange, each device on its own node: the new device writes the
    // first, third and fifth messages, the existing device the rest.
    for step in 0..6u32 {
        let (writer, reader, from, to) = if step % 2 == 0 {
            (&one, &two, "new_device", "existing_device")
        } else {
            (&two, &one, "existing_device", "new_device")
        };
        let message = format!("message-{step}");
        if step > 0 {
            pair_send(writer, &pair, from, step / 2, message.as_bytes())
                .await
                .assert_status(StatusCode::OK);
        }
        assert_eq!(
            pair_receive(reader, &pair, to, step / 2).await,
            vec![b64(if step == 0 {
                b"noise-1"
            } else {
                message.as_bytes()
            })],
            "step {step} reached the other node"
        );
    }
    let spent = pairing(&one, "resolve", &serde_json::json!({ "code": code })).await;
    spent.assert_status(StatusCode::NOT_FOUND);

    pairing(
        &two,
        "abort",
        &serde_json::json!({ "pair_id": pair, "role": "existing_device" }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    let body = serde_json::json!({ "pair_id": pair, "role": "new_device", "after": 0 });
    pairing(&one, "receive", &body)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    feed_one.shutdown();
    feed_two.shutdown();
}

/// The pair-attempt limit is the cluster's, not each node's: ten opens spread
/// over two nodes spend the account's hour, and the eleventh is refused on
/// either.
#[tokio::test]
async fn the_pair_attempt_limit_holds_across_nodes() {
    let Some([(one, feed_one), (two, feed_two)]) = cluster(&ServerConfig::default()).await else {
        return;
    };
    for seed in 0..10u8 {
        let node = if seed % 2 == 0 { &one } else { &two };
        pair_send(node, &b64(&[seed; 16]), "new_device", 0, b"noise-1")
            .await
            .assert_status(StatusCode::OK);
    }
    for node in [&one, &two] {
        pair_send(node, &b64(&[0xee; 16]), "new_device", 0, b"noise-1")
            .await
            .assert_status(StatusCode::TOO_MANY_REQUESTS);
    }
    feed_one.shutdown();
    feed_two.shutdown();
}

/// Revoking a device on node 1 ends the stream it holds on node 2 within the
/// recheck interval: the recheck reads the shared table.
#[tokio::test]
async fn a_revocation_on_one_node_ends_a_stream_on_the_other() {
    let config = ServerConfig {
        device_recheck_ms: 20,
        ..ServerConfig::default()
    };
    let Some([(one, feed_one), (two, feed_two)]) = cluster(&config).await else {
        return;
    };
    let (phone, phone_key) = register_device(&one, 44, "phone", Some(PHONE_VAULT_ID)).await;
    let (laptop, laptop_key) = register_device(&one, 45, "laptop", Some(LAPTOP_VAULT_ID)).await;
    let opened = send_signed(
        &two,
        "POST",
        "/api/v1/sync/session",
        &phone,
        &phone_key,
        Some(&hello()),
    )
    .await;
    opened.assert_status(StatusCode::CREATED);
    let session = opened.json()["session_id"].as_str().unwrap().to_owned();
    subscribe(&two, &session).await;

    let (body, revoked) = tokio::join!(read(&two, &session, None), async {
        tokio::time::sleep(SETTLE).await;
        send_signed(
            &one,
            "DELETE",
            &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
            &laptop,
            &laptop_key,
            None,
        )
        .await
        .status
    });
    assert_eq!(revoked, StatusCode::NO_CONTENT);
    assert!(body.contains("AUTH_DEVICE_REVOKED"), "{body}");
    feed_one.shutdown();
    feed_two.shutdown();
}
