//! A session's upstream frames against the loopback relay: what each one
//! sends, what it is answered with, and what closing it drops.

use super::*;

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
/// JSON operations, [`SseTransport::call_bytes`] for the blob surface and
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
        relay::Canned::json(200, r#"{"server_first_seen_ms":1234}"#),
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

// ---- the three refusals a success path would swallow ----
//
// `Subscribe`, `OpBatch` and `RefreshToken` each guard their reply with
// `if !reply.status.is_success()`. Each guard is separate code and each
// survives separately: replacing any one of them with `false` turns a
// refusal into a success made out of a problem document, and the reply
// readers are total — `field_u64` answers `0` for a field that is not
// there — so nothing downstream notices.

/// A batch the relay refused is a refusal, never an ack.
///
/// The load-bearing one of the three, and therefore written first. Without
/// the guard, the problem document is parsed as an ack: `field_u64` reads
/// the missing `server_first_seen_ms` as `0`, an `Ack` naming this batch
/// reaches the driver, and the outbox retires ops the relay never stored.
/// That is data loss rather than a retry — after the ack the driver has
/// nothing left to re-send.
#[tokio::test]
async fn an_op_batch_the_relay_refused_is_never_acked() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(409, r#"{"code":"SYNC_OP_INVALID"}"#),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let payload = super::OpBatchPayload {
        ops: vec![b"one".to_vec()],
        batch_id: 7,
        stream_id: [0xabu8; 16],
    }
    .encode()
    .expect("an encodable batch");
    let err = t
        .send_frame(frame_of(super::MsgKind::OpBatch, &payload))
        .await
        .expect_err("the relay refused the batch");

    assert_eq!(code_of(&err), "SYNC_OP_INVALID");
    assert!(
        t.inbound.is_empty(),
        "a refused batch synthesizes no Ack, so the outbox keeps its ops"
    );
}

/// A `Subscribe` the relay refused leaves the client subscribed to what it
/// was subscribed to.
///
/// Without the guard the refusal reads as success, and the three pieces of
/// stream state are discarded for a stream set the relay never adopted: the
/// driver believes it is subscribed to streams that were refused, and the
/// resume point it would have needed to recover is gone.
#[tokio::test]
async fn a_subscribe_the_relay_refused_discards_no_resume_point() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(403, r#"{"code":"AUTH_DEVICE_REVOKED"}"#),
    ])
    .await;
    let mut t = open_session(&relay).await;
    t.last_event_id = Some("9".to_owned());

    let payload = super::SubscribePayload {
        streams: vec![sunrise_wire_protocol::SubscribeEntry {
            cursors: Vec::new(),
            stream_id: [0xabu8; 16],
        }],
    }
    .encode()
    .expect("an encodable subscribe");
    let err = t
        .send_frame(frame_of(super::MsgKind::Subscribe, &payload))
        .await
        .expect_err("the relay refused the subscribe");

    assert_eq!(code_of(&err), "AUTH_DEVICE_REVOKED");
    assert_eq!(
        t.last_event_id.as_deref(),
        Some("9"),
        "a refused subscribe changed no stream set, so it discards no resume point"
    );
}

/// A refused refresh is reported rather than acked with a deadline the
/// relay never set.
///
/// Without the guard, `field_u64` reads the absent `expires_at_ms` as `0`,
/// which this protocol spells *no deadline* — so a client whose refresh was
/// rejected would be told its credential never expires, and would stop
/// refreshing until the relay closed the stream under it.
#[tokio::test]
async fn a_token_refresh_the_relay_refused_is_never_acked() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(401, r#"{"code":"AUTH_TOKEN_INVALID"}"#),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let payload = super::RefreshTokenPayload {
        token: "a token the relay will not take".to_owned(),
    }
    .encode()
    .expect("an encodable refresh");
    let err = t
        .send_frame(frame_of(super::MsgKind::RefreshToken, &payload))
        .await
        .expect_err("the relay refused the refresh");

    assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
    assert!(
        t.inbound.is_empty(),
        "a refused refresh names no new deadline"
    );
}

/// A `Ping` is answered locally. The stream keeps itself alive with
/// comments, so a liveness probe costs no round trip — and a transport that
/// issued one would make the driver's ping interval a request rate.
#[tokio::test]
async fn a_ping_is_answered_locally_with_nothing_on_the_wire() {
    let relay = relay::start(Vec::new()).await;
    let mut t = SseTransport::connect(&relay.base);
    t.send_frame(frame_of(super::MsgKind::Ping, &[]))
        .await
        .expect("a local answer");

    let (head, _) = super::decode_frame(
        &next_frame(&mut t)
            .await
            .expect("a reply is waiting")
            .expect("a Pong"),
    )
    .expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::Pong);
    assert!(relay.seen().is_empty(), "and the relay was never asked");
}

/// A `Close` ends the session in both directions at once: nothing more goes
/// up, the open stream is dropped, and the read side reports end of stream
/// rather than reopening.
///
/// The stream is opened first, so `self.events = None` in the `Close` arm
/// is executed with something to clear. `self.closed` alone satisfies the
/// end-of-stream assertion, so without an open body this case says nothing
/// about the body being released — and a `Close` that left it held would
/// keep the relay's connection alive for a session that is over.
#[tokio::test]
async fn a_close_frame_ends_the_session_in_both_directions() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![half_read_stream()]),
    ])
    .await;
    let close = super::ClosePayload {
        code: sunrise_error::ErrorCode::AuthTokenExpired,
        reason: "done".to_owned(),
    }
    .encode()
    .expect("an encodable close");
    let mut t = open_session(&relay).await;
    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("an event became a frame");
    assert!(t.events.is_some(), "the stream is open before the close");

    t.send_frame(frame_of(super::MsgKind::Close, &close))
        .await
        .expect("the close is local");

    assert!(
        t.events.is_none(),
        "the session is over, so the body it was reading is released"
    );
    assert!(
        matches!(
            t.send_frame(frame_of(super::MsgKind::Close, &close)).await,
            Err(TransportError::Cancelled)
        ),
        "a closed transport sends nothing more"
    );
    assert!(
        next_frame(&mut t)
            .await
            .expect("a closed transport is not an error")
            .is_none(),
        "and reads as end of stream"
    );
    assert_eq!(
        relay.seen().len(),
        2,
        "the close itself costs no request: the session and the stream are all of them"
    );
}

/// Closing the transport drops the stream and the half-event behind it.
///
/// Keeping the buffer would hand a reopened stream the tail of the old
/// one's last event, which parses as neither — and keeping the body would
/// hold the relay's connection open past the close. Both need a stream that
/// is really open, which is why this dials one rather than assigning `buf`.
#[tokio::test]
async fn closing_the_transport_drops_the_stream_and_its_buffer() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![half_read_stream()]),
    ])
    .await;
    let mut t = open_session(&relay).await;
    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("an event became a frame");
    assert!(t.events.is_some() && !t.buf.is_empty());

    Transport::close(&mut t).await.expect("close succeeds");

    assert!(t.events.is_none(), "the stream goes");
    assert!(t.buf.is_empty(), "the partial event goes with the stream");
    assert!(
        next_frame(&mut t).await.expect("not an error").is_none(),
        "a closed transport reads as end of stream"
    );
    assert!(
        matches!(
            t.send_frame(hello_frame()).await,
            Err(TransportError::Cancelled)
        ),
        "and refuses to send"
    );
    assert_eq!(
        relay.seen().len(),
        2,
        "the session and the stream open, and closing adds nothing to them"
    );
}

/// A frame the wire codec cannot decode is refused before any route is
/// chosen.
///
/// `send_frame` decodes the frame header first, and every arm below that
/// point assumes it succeeded. A build that let a malformed frame through
/// would dispatch on whatever `msg_kind` the garbage happened to hold, and
/// a build that reported it as `Unavailable` would have the driver retry a
/// frame that will never decode.
#[tokio::test]
async fn a_frame_that_does_not_decode_is_refused_before_any_route() {
    let relay = relay::start(Vec::new()).await;
    let mut t = SseTransport::connect(&relay.base);
    let err = t
        .send_frame(vec![0xffu8; 8])
        .await
        .expect_err("that is not a frame");
    assert!(matches!(err, TransportError::Protocol(_)), "{err}");
    assert!(
        relay.seen().is_empty(),
        "nothing reaches the relay for a frame that never decoded"
    );
}

/// A frame kind that has no operation upstream is refused here rather than
/// turned into a request the relay would have to reject.
#[tokio::test]
async fn a_frame_with_no_upstream_operation_is_refused_before_it_is_sent() {
    let relay = relay::start(Vec::new()).await;
    let mut t = SseTransport::connect(&relay.base);
    let err = t
        .send_frame(frame_of(super::MsgKind::Pong, &[]))
        .await
        .expect_err("nothing carries a Pong upstream");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("Pong")),
        "{err}"
    );
    assert!(relay.seen().is_empty());
}

/// The three revocation outcomes are kept apart.
///
/// "Revoked" and "no such row" are both terminal and are not the same
/// answer: the second one covers a device still being accepted under a row
/// from before `vault_device_id` existed, and reporting it as a revocation
/// would tell a user their lost device had been locked out when it had not.
#[tokio::test]
async fn a_revocation_keeps_revoked_unknown_and_refused_apart() {
    let relay = relay::start(vec![
        relay::Canned::empty(204),
        relay::Canned::empty(404),
        relay::Canned::json(500, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#),
    ])
    .await;
    let mut t = dial(&relay);
    let device = [0x11u8; 16];

    assert_eq!(
        t.revoke_device(device).await.expect("a revocation"),
        crate::transport::RevokeOutcome::Revoked
    );
    assert_eq!(
        t.revoke_device(device).await.expect("no such row"),
        crate::transport::RevokeOutcome::Unknown
    );
    let err = t
        .revoke_device(device)
        .await
        .expect_err("the relay would not");
    assert_eq!(code_of(&err), "AUTH_DEVICE_REVOKE_FAILED");

    let seen = relay.seen();
    assert_eq!(seen[0].method, "DELETE");
    assert_eq!(
        seen[0].path,
        format!(
            "/api/v1/devices/by-vault-id/{}",
            sunrise_id::crockford::encode_bytes(&device)
        ),
        "the route names the vault id, not the ULID the relay minted"
    );
    assert_json_binding(&seen[0]);
    assert_eq!(
        seen[0].header("content-type"),
        None,
        "a `call` with no body advertises no media type either"
    );
}
