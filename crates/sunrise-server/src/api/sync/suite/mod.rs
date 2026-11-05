//! The ported WebSocket suite, a unit test of the whole sync surface.
//!
//! It drives all five operations over HTTP through `crate::api::testing`, which
//! is `#[cfg(test)] pub(crate)` and therefore reachable only from inside the
//! crate — so this is a unit test in its own directory rather than anything
//! under `tests/`. It covers every half of this module at once, which is why it
//! sits beside them instead of inside one of them.
//!
//! Every scenario the five `ws_*` integration files covered lives here,
//! against the operations that replaced the frames. They are unit tests now
//! rather than integration ones because `Service::call` needs no port: what
//! used to require booting a listener and dialling it is an in-process call,
//! so the same coverage costs no sockets and no timing.
//!
//! Three scenarios changed shape rather than moving, and each says so where
//! it sits: `Ping` is a keep-alive comment now, a malformed batch is refused
//! by the schema rather than nacked, and the handshake round trip is an HTTP
//! response rather than a frame exchange.
//!
//! One file per surface; this one holds only what more than one of them uses.

mod auth;
mod cursors;
mod dedup;
mod device_binding;
mod extension;
mod fanout;
mod handshake;
mod invariants;
mod push_wake;
mod token_expiry;

use crate::api::error::codes;
use crate::api::testing::{
    code_of, now_rfc2822, register_device, send_signed, send_signed_with, Client, BEARER,
};
use crate::relay::RingCaps;
use crate::relay_log::DurableCaps;
use crate::state::{Clock, ServerState};
use crate::{ServerConfig, StaticVerifier, Subject};
use kynos::http::{Method, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use sunrise_cbor::magic::{write_prefix, MagicKind, MAGIC_LEN};
use sunrise_cbor::version::{ENVELOPE_FORMAT_FLOOR, ENVELOPE_FORMAT_V, WIRE_PROTO_V};
use sunrise_wire_protocol::{Capability, CapabilityBits};

const STREAM_BYTES: [u8; 16] = [0x11; 16];
const ISSUER: &str = "https://idp.example";
const T0_MS: u64 = 1_704_067_200_000;

fn stream_hex() -> String {
    hex::encode(STREAM_BYTES)
}

/// A clock a test moves by hand, so expiry is a value rather than a wait.
#[derive(Debug)]
struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }
    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

fn hello() -> serde_json::Value {
    serde_json::json!({
        "client_app_v": "0.1.0",
        "client_platform": "test",
        "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
        "doc_schema_min": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
        "doc_schema_max": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
        "crypto_suite_supported": [u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)],
        // `SrvTokenRefresh` is agreed by AND, so a client that wants a
        // refresh has to offer it too. A real one does; this mirrors that.
        "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
        "trace": "01J000000000000000000000000",
    })
}

async fn session_as(client: &Client, bearer: &str) -> String {
    let res = client
        .send_as(
            Method::POST,
            "/api/v1/sync/session",
            Some(bearer),
            Some(&hello()),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned()
}

async fn establish(client: &Client) -> String {
    session_as(client, BEARER).await
}

async fn subscribe_as(
    client: &Client,
    bearer: &str,
    id: &str,
    cursor: Option<(String, u64)>,
) -> StatusCode {
    let cursors = cursor.map_or_else(Vec::new, |(device_id, last_applied_seq)| {
        vec![serde_json::json!({
            "device_id": device_id,
            "last_applied_seq": last_applied_seq,
        })]
    });
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(bearer),
            Some(&serde_json::json!({
                "streams": [{ "stream_id": stream_hex(), "cursors": cursors }]
            })),
            &[("x-sunrise-session", id)],
        )
        .await
        .status
}

async fn subscribe(client: &Client, id: &str, cursor: Option<(String, u64)>) {
    assert_eq!(
        subscribe_as(client, BEARER, id, cursor).await,
        StatusCode::NO_CONTENT
    );
}

/// One op envelope from `device` at `seq`.
///
/// Header-shaped rather than a real sealed envelope: that is exactly the
/// surface the relay reads — `stream_id`, `device_id`, `seq`, all cleartext
/// — and building real ones would put `sunrise-crypto`, the crate holding
/// the keys a relay must not have, into the server's test graph.
fn envelope(device: [u8; 16], seq: u64) -> String {
    use base64::Engine as _;
    use ciborium::value::Value;
    let entries: Vec<(Value, Value)> = vec![
        (Value::Integer(1.into()), Value::Integer(3.into())),
        (
            Value::Integer(2.into()),
            Value::Bytes(STREAM_BYTES.to_vec()),
        ),
        (Value::Integer(3.into()), Value::Bytes(device.to_vec())),
        (Value::Integer(4.into()), Value::Integer(seq.into())),
        (
            Value::Integer(10.into()),
            Value::Bytes(vec![u8::try_from(seq & 0xff).unwrap(); 32]),
        ),
    ];
    let mut out = vec![0u8; MAGIC_LEN];
    write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_V);
    ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
    base64::engine::general_purpose::STANDARD.encode(out)
}

/// [`envelope`] as a writer one container newer would emit it (ADR-0045
/// §5): this build's floor in the prefix, `ENVELOPE_FORMAT_V + 1` in field
/// 1, and a field this build does not know.
fn newer_container_envelope(device: [u8; 16], seq: u64) -> String {
    use base64::Engine as _;
    use ciborium::value::Value;
    let entries: Vec<(Value, Value)> = vec![
        (
            Value::Integer(1.into()),
            Value::Integer((ENVELOPE_FORMAT_V + 1).into()),
        ),
        (
            Value::Integer(2.into()),
            Value::Bytes(STREAM_BYTES.to_vec()),
        ),
        (Value::Integer(3.into()), Value::Bytes(device.to_vec())),
        (Value::Integer(4.into()), Value::Integer(seq.into())),
        (
            Value::Integer(10.into()),
            Value::Bytes(vec![u8::try_from(seq & 0xff).unwrap(); 32]),
        ),
        (Value::Integer(23.into()), Value::Text("future".into())),
    ];
    let mut out = vec![0u8; MAGIC_LEN];
    write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
    ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
    base64::engine::general_purpose::STANDARD.encode(out)
}

async fn publish_as(
    client: &Client,
    bearer: &str,
    id: &str,
    ops: Vec<String>,
    batch_id: u64,
) -> StatusCode {
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(bearer),
            Some(&serde_json::json!({
                "stream_id": stream_hex(),
                "batch_id": batch_id,
                "ops": ops,
            })),
            &[("x-sunrise-session", id)],
        )
        .await
        .status
}

async fn publish(client: &Client, id: &str, ops: Vec<String>, batch_id: u64) -> StatusCode {
    publish_as(client, BEARER, id, ops, batch_id).await
}

/// [`publish`], keeping the `Ack` body — the dedup tests assert on
/// `server_first_seen_ms`, which a status code cannot carry.
async fn publish_acked(
    client: &Client,
    id: &str,
    ops: Vec<String>,
    batch_id: u64,
) -> serde_json::Value {
    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": stream_hex(),
                "batch_id": batch_id,
                "ops": ops,
            })),
            &[("x-sunrise-session", id)],
        )
        .await;
    res.assert_status(StatusCode::OK);
    res.json()
}

async fn read_as(client: &Client, bearer: &str, id: &str, extra: &[(&str, &str)]) -> String {
    let mut headers = vec![("x-sunrise-session", id), ("authorization", bearer)];
    headers.extend_from_slice(extra);
    client
        .read_stream(
            "/api/v1/sync/events",
            &headers,
            std::time::Duration::from_millis(250),
        )
        .await
}

async fn read(client: &Client, id: &str, extra: &[(&str, &str)]) -> String {
    read_as(client, BEARER, id, extra).await
}

/// `GET /sync/events`, expecting a refusal rather than a stream.
///
/// [`Client::send_with`] collects the whole body, and a stream that is
/// *not* refused never ends — so the call is bounded. A timeout here is
/// the assertion failing: the server served a request it was meant to
/// refuse.
async fn open_events(
    client: &Client,
    id: &str,
    extra: &[(&str, &str)],
) -> crate::api::testing::Res {
    let mut headers = vec![("x-sunrise-session", id)];
    headers.extend_from_slice(extra);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.send_with(
            Method::GET,
            "/api/v1/sync/events",
            Some(BEARER),
            None,
            &headers,
        ),
    )
    .await
    .expect("the stream had to be refused; a served stream never ends")
}

/// A vault id for the device under test, and one for the device revoking
/// it. Crockford base-32 of 16 bytes, the way a `device_revoke` names one.
const SESSION_PHONE_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
/// The revoking device's.
const SESSION_LAPTOP_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";
