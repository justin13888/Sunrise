//! The SSE transport's tests, one child module per section, and the
//! fixtures they share.
//!
//! Every module here is a descendant of `sse`, which is what lets a case
//! reach the private items it pins -- `take_event`, `frame_for`,
//! `binding_bytes`, the stream buffer -- and why none of them lives under
//! the crate's `tests/`. A child's `super::` paths resolve through
//! the imports here, so a case reads `super::read_body` wherever it sits.

mod binding;
mod blobs;
mod body_bounds;
mod buffer;
mod handshake;
mod mapping;
mod readers;
mod relay;
mod session;
mod stream;

use super::{
    await_head, binding_advice, cursor_json, decode_frame, field_str, field_u32, field_u64,
    parse_id, read_body, retry_after, server_date, AckPayload, BacklogPayload, CaughtUpPayload,
    ClosePayload, CursorEntry, ErrorPayload, HelloAck, MsgKind, OpBatchPayload,
    RefreshTokenAckPayload, RefreshTokenPayload, Reply, SseTransport, SubscribePayload,
    BLOB_IDLE_TIMEOUT, BODY_IDLE_TIMEOUT, CHUNK_HEAD_TIMEOUT, HEAD_TIMEOUT, MAX_BLOB_BYTES,
    MAX_EVENT_BYTES,
};
use crate::signer::DeviceSigner;
use crate::transport::{Transport, TransportError};
use std::sync::Arc;

/// A signer over a fixed key and a fixed clock — the shape a test needs
/// and, minus the fixtures, the shape `Core` provides.
#[derive(Debug)]
struct FixedSigner {
    key: ed25519_dalek::SigningKey,
    device_id: String,
    now_ms: u64,
}

impl DeviceSigner for FixedSigner {
    fn device_id(&self) -> String {
        self.device_id.clone()
    }
    fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer as _;
        self.key.sign(message).to_bytes()
    }
    fn now_ms(&self) -> u64 {
        self.now_ms
    }
}

fn signer(now_ms: u64) -> Arc<dyn DeviceSigner> {
    Arc::new(FixedSigner {
        key: ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]),
        device_id: "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y".to_owned(),
        now_ms,
    })
}

/// `NOW_MS` is the instant `DATE` names.
const DATE: &str = "Mon, 31 Aug 2026 00:00:00 GMT";
const NOW_MS: u64 = 1_788_134_400_000;

fn reply(status: u16, body: &[u8], date: Option<&str>) -> Reply {
    Reply {
        status: hyper::StatusCode::from_u16(status).expect("a status"),
        date: date.map(ToOwned::to_owned),
        retry_after: None,
        bytes: body.to_vec(),
    }
}

fn code_of(err: &TransportError) -> &'static str {
    match err {
        TransportError::Server { code, .. } => code,
        other => panic!("expected a server refusal, got {other}"),
    }
}

const SIG_INVALID: &[u8] = br#"{"type":"https://sunrise.app/problems/unauthenticated","status":401,"code":"AUTH_DEVICE_SIG_INVALID"}"#;

fn message_of(err: &TransportError) -> String {
    match err {
        TransportError::Server { message, .. } => message.clone(),
        other => panic!("expected a server refusal, got {other}"),
    }
}

/// The reply to `Hello` every relay-backed test starts from.
const SESSION_OK: &str = r#"{"session_id":"sess-1","server_app_v":"9.9.9","wire_proto":1,"crypto_suite":1,"doc_schema_floor":3,"capabilities":5,"server_time_ms":1788134400000}"#;

/// The public half of the key [`signer`] signs with.
fn signing_pub() -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
    sunrise_http_sig::device_pub_b64(&key.verifying_key().to_bytes())
}

/// The signature and date a recorded request carried, having first asserted
/// that it named the device that signed it.
fn binding_of(seen: &relay::Seen) -> (String, String) {
    assert_eq!(
        seen.header("x-sunrise-device"),
        Some("dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
        "{} {} must name the signing device",
        seen.method,
        seen.path
    );
    let signature = seen
        .header("x-sunrise-device-sig")
        .unwrap_or_else(|| panic!("{} {} must carry a signature", seen.method, seen.path));
    let date = seen
        .header("date")
        .unwrap_or_else(|| panic!("{} {} must carry a date", seen.method, seen.path));
    (signature.to_owned(), date.to_owned())
}

/// A JSON request's binding verifies against the value the relay parsed
/// back out of the body it was sent.
///
/// ADR-0022 signs the canonical form of the request *value*, so re-parsing
/// the emitted body and canonicalizing that is exactly what the relay's own
/// verifier does — which is the property being asserted, rather than
/// "three headers are present".
fn assert_json_binding(seen: &relay::Seen) {
    let value: Option<serde_json::Value> = if seen.body.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&seen.body).expect("a JSON body"))
    };
    let (signature, date) = binding_of(seen);
    sunrise_http_sig::verify(
        &signing_pub(),
        &signature,
        &seen.method,
        &seen.path,
        &date,
        value.as_ref(),
        NOW_MS,
    )
    .unwrap_or_else(|e| panic!("{} {} must verify: {e}", seen.method, seen.path));
}

/// A byte-body request's binding verifies over the octets themselves: those
/// bytes already are the canonical form, and routing them through the value
/// verifier would canonicalize them twice.
fn assert_byte_binding(seen: &relay::Seen) {
    let (signature, date) = binding_of(seen);
    sunrise_http_sig::verify_canonical(
        &signing_pub(),
        &signature,
        &seen.method,
        &seen.path,
        &date,
        &seen.body,
        NOW_MS,
    )
    .unwrap_or_else(|e| panic!("{} {} must verify: {e}", seen.method, seen.path));
}

/// A bound transport pointed at `relay`.
fn dial(relay: &relay::Relay) -> SseTransport {
    SseTransport::connect(&relay.base).with_device_signer(signer(NOW_MS))
}

fn hello_frame() -> Vec<u8> {
    let hello = super::Hello {
        client_app_v: "1.4.2".to_owned(),
        client_platform: "linux-x86_64".to_owned(),
        wire_proto_supported: vec![1],
        doc_schema_min: 1,
        doc_schema_max: 2,
        crypto_suite_supported: vec![1],
        capabilities: 0,
        trace: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello, &mut payload).expect("a serialisable Hello");
    frame_of(super::MsgKind::Hello, &payload)
}

fn frame_of(kind: super::MsgKind, payload: &[u8]) -> Vec<u8> {
    super::encode_frame(kind, super::FrameFlags::EMPTY, payload).expect("an encodable frame")
}

/// The frame cap, written out rather than read from the constant it pins.
///
/// A test that sizes its own stream from [`MAX_EVENT_BYTES`] moves with it:
/// it would build an eight-byte stream against an eight-byte cap and pass,
/// which makes it a test of the comparison and of nothing else. Spelling
/// the number here is what lets the cases in `stream` and `blobs` say the
/// cap is *this* large.
const FRAME_CAP: usize = 8 * 1024 * 1024;

/// [`SseTransport::recv_frame`], bounded.
///
/// A read has no deadline of its own — in production the driver's own
/// supervision is what bounds it — so a relay that goes quiet mid-exchange
/// would hang the whole test binary rather than fail one test. Every read
/// through it is answered by a scripted relay in milliseconds, so anything near
/// this deadline is a defect.
///
/// It bounds *waiting*, not spinning. A defect that put the drain loop into
/// a cycle with no `await` in it would block the runtime thread outright,
/// and no timer running on that thread can fire to interrupt it; catching
/// that class needs a deadline outside the process. See the frame-cap cases
/// in `stream` for the part of this surface that is bounded by the code itself.
async fn next_frame(t: &mut SseTransport) -> Result<Option<Vec<u8>>, TransportError> {
    tokio::time::timeout(std::time::Duration::from_secs(30), t.recv_frame())
        .await
        .expect("a read against a scripted relay must not hang")
}

/// One stream chunk that leaves the transport mid-stream: a whole event the
/// client will take, followed by the beginning of a second it cannot.
///
/// Reading it through [`next_frame`] puts all three pieces of stream state
/// into the only configuration where dropping them means anything —
/// `events` `Some`, `last_event_id` `Some("9")`, `buf` non-empty. A test
/// that sets those fields by hand instead leaves `events` `None`, and every
/// `self.events = None;` in `sse` stays deletable.
fn half_read_stream() -> hyper::body::Bytes {
    hyper::body::Bytes::from(format!(
        "id: 9\ndata: {}\n\ndata: half an ev",
        serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
    ))
}

/// Open a session against `relay` and swallow the `HelloAck`, so a test
/// about something else starts where that test left off.
async fn open_session(relay: &relay::Relay) -> SseTransport {
    let mut t = dial(relay);
    t.send_frame(hello_frame()).await.expect("a session opens");
    next_frame(&mut t)
        .await
        .expect("the ack is waiting")
        .expect("a HelloAck");
    t
}
