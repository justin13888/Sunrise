//! The three refusals a success path would swallow.
//!
//! `Subscribe`, `OpBatch` and `RefreshToken` each guard their reply with
//! `if !reply.status.is_success()`. Each guard is separate code and each
//! survives separately: replacing any one of them with `false` reads a
//! problem document as the reply. The typed decode now refuses that document
//! too (#1264), but as a protocol error that loses the relay's own code, so
//! the assertions below pin the code rather than only the failure.

use super::session::OPS_OK;
use super::*;

/// A batch the relay refused is a refusal, never an ack.
///
/// The load-bearing one of the three, and therefore written first. Before
/// the replies were decoded into their typed models, a missing guard parsed
/// the problem document as an ack: the missing `server_first_seen_ms` read
/// as `0`, an `Ack` naming this batch reached the driver, and the outbox
/// retired ops the relay never stored. That is data loss rather than a
/// retry — after the ack the driver has nothing left to re-send.
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
/// Before the typed decode, a missing guard read the absent `expires_at_ms`
/// as `0`, which this protocol spells *no deadline* — so a client whose refresh was
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

// ---- the success replies that used to read a missing field as zero ----
//
// #1264: a `200` whose body lacks a field the relay's description requires
// is malformed, and is refused as one rather than read as `0`.

/// An ack missing a field `OpsResponse` requires is a protocol error, never
/// an `Ack`: read as `0`, a missing `server_first_seen_ms` would retire the
/// batch from the outbox with a first-seen time the relay never stated.
#[tokio::test]
async fn an_op_batch_ack_missing_a_required_field_is_never_acked() {
    let good: serde_json::Value = serde_json::from_str(OPS_OK).expect("a JSON fixture");
    for field in ["batch_id", "stream_id", "server_first_seen_ms"] {
        let mut body = good.clone();
        body.as_object_mut()
            .expect("an object")
            .remove(field)
            .expect("the fixture carries every required field");
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(200, &body.to_string()),
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
            .expect_err("an ack missing a required field is malformed");

        assert!(matches!(err, TransportError::Protocol(_)), "{field}: {err}");
        assert!(
            t.inbound.is_empty(),
            "a malformed ack synthesizes no Ack, so the outbox keeps its ops: {field}"
        );
    }
}

/// An ack whose echo names another batch or another stream is no ack of the
/// batch sent, so it reaches the driver as a protocol error rather than as an
/// `Ack` that would retire this batch from the outbox.
#[tokio::test]
async fn an_op_batch_ack_naming_another_batch_is_never_acked() {
    let good: serde_json::Value = serde_json::from_str(OPS_OK).expect("a JSON fixture");
    for (field, value) in [
        ("batch_id", serde_json::json!(8)),
        ("stream_id", serde_json::json!("cd".repeat(16))),
    ] {
        let mut body = good.clone();
        body[field] = value;
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(200, &body.to_string()),
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
            .expect_err("an ack of another batch is not an ack of this one");

        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("not the batch sent")),
            "{field}: {err}"
        );
        assert!(t.inbound.is_empty(), "the outbox keeps its ops: {field}");
    }
}

/// A refresh reply with no `expires_at_ms` is a protocol error rather than a
/// deadline of `0`, which this protocol spells *no deadline*.
#[tokio::test]
async fn a_token_refresh_reply_missing_its_deadline_is_never_acked() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(200, r#"{"refresh_after_ms":1788137000000}"#),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let payload = super::RefreshTokenPayload {
        token: "the-new-bearer".to_owned(),
    }
    .encode()
    .expect("an encodable refresh");
    let err = t
        .send_frame(frame_of(super::MsgKind::RefreshToken, &payload))
        .await
        .expect_err("a reply naming no deadline is malformed");

    assert!(matches!(err, TransportError::Protocol(_)), "{err}");
    assert!(
        t.inbound.is_empty(),
        "a malformed refresh reply names no new deadline"
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

/// The resume point is opaque, and a relay serving several streams writes one
/// position per stream into it (ADR-0023's third amendment). A client that
/// read it as a number would drop it, or keep only the part it could parse,
/// and resume each stream from somewhere the relay never said.
#[tokio::test]
async fn a_per_stream_resume_position_is_presented_verbatim() {
    const POSITION: &str = "0123456789abcdef:0=9,1=3";
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![hyper::body::Bytes::from(format!(
            "id: {POSITION}\ndata: {}\n\n",
            serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
        ))]),
        relay::Canned::stream(Vec::new()),
    ])
    .await;
    let mut t = open_session(&relay).await;

    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("a frame");
    assert_eq!(t.last_event_id.as_deref(), Some(POSITION));
    assert!(next_frame(&mut t)
        .await
        .expect("a relay that closes the stream is not an error")
        .is_none());
    let _ = next_frame(&mut t).await;

    assert_eq!(relay.seen()[2].header("last-event-id"), Some(POSITION));
}
