//! A session's upstream frames against the loopback relay: what each one
//! sends, what it is answered with, and what closing it drops.

use super::*;

/// The relay's ack for batch 7 on stream `ab…ab`, carrying every field
/// `OpsResponse` requires.
pub(super) const OPS_OK: &str =
    r#"{"batch_id":7,"stream_id":"abababababababababababababababab","server_first_seen_ms":1234}"#;

/// A relay that refuses an operation is reported as a refusal, with its own
/// code. Reading a refusal as a success is the failure mode worth pinning:
/// the driver would then wait on a session that was never opened.
#[tokio::test]
async fn a_refused_operation_is_reported_with_the_relay_s_own_code() {
    let relay = relay::start(vec![relay::Canned::json(
        503,
        r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#,
    )])
    .await;
    let mut t = dial(&relay);
    let err = t
        .send_frame(hello_frame())
        .await
        .expect_err("the relay refused");
    assert_eq!(code_of(&err), "RELAY_STORAGE_UNAVAILABLE");
}

/// Every route this transport reaches presents the bearer it was built
/// with.
///
/// Three separate pieces of code attach it — [`SseTransport::call`] for the
/// JSON operations, the generated client (`rest`) for the blob surface and
/// [`SseTransport::open_events`] for the stream — so this asserts once per
/// site rather than once for the transport. Nothing else that scores this
/// crate reaches them: the only other bearer-setting tests are in
/// `crates/sunrise-e2e`, which `mise.toml:1005`'s `cargo mutants -p
/// sunrise-sync` does not build and `.cargo/mutants.toml` excludes besides.
/// Deleting any one of the three bodies leaves the rest of this crate
/// green, and a transport that dropped the bearer would earn a `401` on
/// every route it makes.
#[tokio::test]
async fn every_route_presents_the_bearer_the_transport_was_built_with() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![half_read_stream()]),
        relay::Canned::bytes(200, b"the ciphertext"),
    ])
    .await;
    let mut t = SseTransport::connect_with_bearer(&relay.base, Some("the-bearer"))
        .with_device_signer(signer(NOW_MS));

    t.send_frame(hello_frame()).await.expect("a session opens");
    next_frame(&mut t)
        .await
        .expect("the ack is waiting")
        .expect("a HelloAck");
    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("an event became a frame");
    assert_eq!(
        t.blob_fetch(&[0x11u8; 16])
            .await
            .expect("the fetch succeeds"),
        Some(b"the ciphertext".to_vec())
    );

    let seen = relay.seen();
    assert_eq!(seen[0].path, "/api/v1/sync/session");
    assert_eq!(
        seen[0].header("authorization"),
        Some("Bearer the-bearer"),
        "the JSON operations"
    );
    assert_eq!(seen[1].path, "/api/v1/sync/events");
    assert_eq!(
        seen[1].header("authorization"),
        Some("Bearer the-bearer"),
        "the stream"
    );
    assert_eq!(
        seen[2].path,
        format!("/api/v1/blobs/blb_{}", "11".repeat(16))
    );
    assert_eq!(
        seen[2].header("authorization"),
        Some("Bearer the-bearer"),
        "the blob surface"
    );
}

/// A transport built without one presents no `Authorization` at all, on
/// the same three routes.
///
/// The negative half of the case above, and it is not redundant with it: a
/// header attached unconditionally — a `Bearer` built from an empty string,
/// a default, a `unwrap_or_default` where the `Option` is read — satisfies
/// every assertion there and is invisible to all of them. `connect` exists
/// for the self-host relay running `NullVerifier`, where the request that
/// carries a credential is the wrong request, so "no header" is the
/// contract rather than the absence of one.
#[tokio::test]
async fn a_transport_built_without_a_bearer_presents_none() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![half_read_stream()]),
        relay::Canned::bytes(200, b"the ciphertext"),
    ])
    .await;
    let mut t = SseTransport::connect(&relay.base).with_device_signer(signer(NOW_MS));

    t.send_frame(hello_frame()).await.expect("a session opens");
    next_frame(&mut t)
        .await
        .expect("the ack is waiting")
        .expect("a HelloAck");
    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("an event became a frame");
    assert_eq!(
        t.blob_fetch(&[0x11u8; 16])
            .await
            .expect("the fetch succeeds"),
        Some(b"the ciphertext".to_vec())
    );

    let seen = relay.seen();
    assert_eq!(seen[0].path, "/api/v1/sync/session");
    assert_eq!(seen[0].header("authorization"), None, "the JSON operations");
    assert_eq!(seen[1].path, "/api/v1/sync/events");
    assert_eq!(seen[1].header("authorization"), None, "the stream");
    assert_eq!(
        seen[2].path,
        format!("/api/v1/blobs/blb_{}", "11".repeat(16))
    );
    assert_eq!(seen[2].header("authorization"), None, "the blob surface");
}

/// A `Subscribe` restates the client's cursors, which drops both the open
/// stream and the resume point that went with it.
///
/// The resume point is the load-bearing half. `Last-Event-ID` says "I
/// received everything up to here"; the cursors say "I have *applied*
/// everything up to here". They differ exactly when delivery succeeded and
/// application did not, which is precisely when the driver resubscribes —
/// so keeping the id would resume past the ops the cursors are asking for.
///
/// The stream is opened **for real** before the `Subscribe`, rather than
/// the three fields being set by hand: with `events` already `None` on
/// entry, `self.events = None` is deletable and every assertion below still
/// holds. A client that kept the old body would go on reading events for a
/// stream set it no longer subscribes to, and never open the new one.
#[tokio::test]
async fn a_subscribe_drops_the_open_stream_and_its_resume_point() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![half_read_stream()]),
        relay::Canned::json(200, "{}"),
    ])
    .await;
    let mut t = open_session(&relay).await;
    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("an event became a frame");
    assert!(
        t.events.is_some(),
        "the stream is open before the subscribe"
    );
    assert_eq!(t.last_event_id.as_deref(), Some("9"));
    assert!(!t.buf.is_empty(), "and half an event is still buffered");

    let payload = super::SubscribePayload {
        streams: vec![sunrise_wire_protocol::SubscribeEntry {
            cursors: vec![super::CursorEntry {
                device_id: [0x7bu8; 16],
                last_applied_seq: 42,
            }],
            stream_id: [0xabu8; 16],
        }],
    }
    .encode()
    .expect("an encodable subscribe");
    t.send_frame(frame_of(super::MsgKind::Subscribe, &payload))
        .await
        .expect("the subscribe lands");

    assert!(
        t.events.is_none(),
        "the stream set changed, so the body opened against the old one goes"
    );
    assert!(
        t.last_event_id.is_none(),
        "a restated cursor is the stricter statement, so the resume point goes"
    );
    assert!(t.buf.is_empty(), "and the stale stream's bytes with it");

    let seen = relay.seen();
    assert_eq!(seen[2].path, "/api/v1/sync/subscribe");
    assert_json_binding(&seen[2]);
    let sent: serde_json::Value = serde_json::from_slice(&seen[2].body).expect("a JSON body");
    assert_eq!(sent["streams"][0]["stream_id"], "ab".repeat(16));
    assert_eq!(
        sent["streams"][0]["cursors"][0]["device_id"],
        "7b".repeat(16)
    );
    assert_eq!(sent["streams"][0]["cursors"][0]["last_applied_seq"], 42);
}

/// A batch goes up as base64 and comes back acked with the relay's
/// first-seen time, which is what makes a re-send after a lost ack
/// idempotent rather than a second publication.
#[tokio::test]
async fn an_op_batch_is_published_and_acked_against_the_batch_that_sent_it() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(200, OPS_OK),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let payload = super::OpBatchPayload {
        ops: vec![b"one".to_vec(), b"two".to_vec()],
        batch_id: 7,
        stream_id: [0xabu8; 16],
    }
    .encode()
    .expect("an encodable batch");
    t.send_frame(frame_of(super::MsgKind::OpBatch, &payload))
        .await
        .expect("the batch is published");

    let (head, payload) = super::decode_frame(
        &next_frame(&mut t)
            .await
            .expect("an ack is waiting")
            .expect("an Ack"),
    )
    .expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::Ack);
    let ack = super::AckPayload::decode(&payload).expect("an AckPayload");
    assert_eq!(ack.batch_id, 7);
    assert_eq!(ack.stream_id, [0xabu8; 16]);
    assert_eq!(ack.server_first_seen_ms, 1234);

    let seen = relay.seen();
    assert_eq!(seen[1].path, "/api/v1/sync/ops");
    assert_json_binding(&seen[1]);
    assert_eq!(
        seen[1].header("x-sunrise-session"),
        Some("sess-1"),
        "every post-handshake operation names the session the handshake minted; \
             the relay's SessionHeader extractor refuses one that does not"
    );
    let sent: serde_json::Value = serde_json::from_slice(&seen[1].body).expect("a JSON body");
    assert_eq!(sent["batch_id"], 7);
    assert_eq!(sent["ops"], serde_json::json!(["b25l", "dHdv"]));
}

/// A refreshed credential is acknowledged with the new deadline, so the
/// driver knows when to refresh again rather than waiting for a close.
#[tokio::test]
async fn a_token_refresh_is_acked_with_the_new_deadline() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(200, r#"{"expires_at_ms":1788138000000}"#),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let payload = super::RefreshTokenPayload {
        token: "the-new-bearer".to_owned(),
    }
    .encode()
    .expect("an encodable refresh");
    t.send_frame(frame_of(super::MsgKind::RefreshToken, &payload))
        .await
        .expect("the refresh lands");

    let (head, payload) = super::decode_frame(
        &next_frame(&mut t)
            .await
            .expect("an ack is waiting")
            .expect("a RefreshTokenAck"),
    )
    .expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::RefreshTokenAck);
    let ack = super::RefreshTokenAckPayload::decode(&payload).expect("the ack payload");
    assert_eq!(ack.expires_at_ms, 1_788_138_000_000);

    let seen = relay.seen();
    assert_eq!(seen[1].path, "/api/v1/sync/session/refresh");
    assert_json_binding(&seen[1]);
}
