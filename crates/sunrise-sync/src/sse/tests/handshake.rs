//! The `Hello` route against the loopback relay: the session it opens, the
//! replies it refuses, and the bounds that keep it from parking the driver.

use super::*;

/// The handshake reaches the relay signed, and its reply becomes the
/// `HelloAck` the driver negotiated against.
#[tokio::test]
async fn hello_opens_a_session_and_its_reply_becomes_a_hello_ack() {
    let relay = relay::start(vec![relay::Canned::json(200, SESSION_OK)]).await;
    let mut t = dial(&relay);
    t.send_frame(hello_frame()).await.expect("a session opens");

    let (head, payload) = super::decode_frame(
        &next_frame(&mut t)
            .await
            .expect("a reply is waiting")
            .expect("a HelloAck"),
    )
    .expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::HelloAck);
    let ack: super::HelloAck =
        ciborium::de::from_reader(&payload[..]).expect("a decodable HelloAck");
    assert_eq!(ack.server_app_v, "9.9.9");
    assert_eq!(ack.wire_proto, 1);
    assert_eq!(ack.crypto_suite, 1);
    assert_eq!(ack.doc_schema_floor, 3);
    assert_eq!(ack.capabilities, 5);
    assert_eq!(ack.server_time_ms, NOW_MS);

    let seen = relay.seen();
    assert_eq!(seen.len(), 1, "one request, and the reply came from it");
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/api/v1/sync/session");
    assert_json_binding(&seen[0]);
    let sent: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("a JSON body");
    assert_eq!(sent["client_app_v"], "1.4.2");
    assert_eq!(sent["wire_proto_supported"], serde_json::json!([1]));
    assert_eq!(sent["trace"], "01ARZ3NDEKTSV4RRFFQ69G5FAV");
}

/// A `Hello` reply this transport cannot read is a failed handshake, and
/// says so.
///
/// Two shapes, and the second is the one that used to be quiet. A reply
/// that is not JSON has always been a protocol error the driver sees. A
/// reply that *is* JSON but names no `session_id` was accepted:
/// `self.session` stayed `None` and a `HelloAck` went to the driver as
/// though the handshake had succeeded, so every later `call` went up with
/// no `x-sunrise-session` for the relay to refuse and the next stream open
/// reported "no sync session: send Hello first" — the wrong cause, since
/// `Hello` was sent and acked.
///
/// `crates/sunrise-server/src/api/sync/credential.rs:59#SessionResponse` declares
/// `session_id` non-`Option`, so such a reply is malformed by the server's
/// own contract. Both shapes now fail where they are read, and neither
/// leaves an ack behind: the driver's reconnect is what decides what
/// happens next, which is what it does for every other failed handshake.
#[tokio::test]
async fn a_hello_reply_this_build_cannot_read_is_refused_rather_than_acked() {
    for body in [
        "not json at all",
        r#"{"server_app_v":"9.9.9","wire_proto":1,"crypto_suite":1,"doc_schema_floor":3}"#,
    ] {
        let relay = relay::start(vec![relay::Canned::json(200, body)]).await;
        let mut t = dial(&relay);
        let err = t
            .send_frame(hello_frame())
            .await
            .expect_err("a reply this build cannot read is not a session");
        assert!(matches!(err, TransportError::Protocol(_)), "{err}");
        assert!(
            t.session.is_none(),
            "and nothing is remembered from a handshake that failed"
        );
        assert!(
            t.inbound.is_empty(),
            "no HelloAck reaches the driver for a handshake that did not happen"
        );
    }
}

/// A handshake whose reply body never finishes arriving is reported rather
/// than waited on for good.
///
/// The `Hello` route is the one with nothing above it. The stream route's
/// stall, in `stream`, was at least *cancelled*:
/// `crates/sunrise-core/src/sync_driver.rs:884-891#session` selects over the
/// read, so a stalled refusal there became a silent retry loop. This one is
/// not. `crates/sunrise-core/src/sync_driver.rs:744#handshake` awaits the
/// `Hello` send bare, outside any `tokio::select!`, `session(...)` is itself
/// awaited bare at `crates/sunrise-core/src/sync_driver.rs:474#run`, and
/// `Client::builder(...).build(https)` sets no timeout — so an unbounded read
/// here parks the driver task with no error, no `SessionEnd::Disconnected`, no
/// backoff and no log line. That is the "the client isn't syncing" state
/// `crates/sunrise-core/src/sync_driver.rs:422-424#run` calls the commonest
/// support question, and the only symptom is silence.
///
/// The relay answers `200` — a refusal is not needed to reach this, and a
/// success makes the point better: the handshake is going *well* right up
/// to the body that never comes.
#[tokio::test]
async fn a_stalled_handshake_reply_is_reported_rather_than_parking_the_driver() {
    let relay = relay::start(vec![relay::Canned::json_then_stall(
        200,
        r#"{"session_id":"sess-"#,
    )])
    .await;
    let mut t = dial(&relay);

    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.send_frame(hello_frame()),
    )
    .await
    .expect("the read is bounded by the client, not by this harness")
    .expect_err("the relay answered and then went quiet");

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
        "a stalled handshake is reported as unavailable, which is what the driver \
             turns into a disconnect and a backoff: {err}"
    );
    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("no data for 2s")),
        "and it is the *document* deadline that ran, named in the diagnostic: {err}"
    );
    let elapsed = started.elapsed();
    // A lower bound only: the diagnostic above already says it was the two
    // seconds and not the blob route's fifteen, and the harness timeout
    // bounds the wait, so an upper bound here would only measure how loaded
    // the machine is.
    assert!(
        elapsed >= std::time::Duration::from_millis(1_500),
        "it came back on the client's own deadline, and not before it — an upper bound \
             alone leaves every shorter value passing: {elapsed:?}"
    );
    assert!(
        t.session.is_none(),
        "no session is recorded out of a reply that never arrived"
    );
}

/// A handshake whose reply never *starts* is reported rather than waited on
/// for good.
///
/// The case above, one await earlier. There the head arrived and the body
/// stalled; here the peer accepts the connection and then sends nothing at
/// all — no status line — which is what a relay, forward proxy or captive
/// portal that completes the handshake and then goes quiet looks like from
/// this side. The body bound never starts, because there is no body, and
/// before [`super::HEAD_TIMEOUT`] nothing else did either: the `Hello` send
/// parked the driver with silence as its only symptom.
///
/// The peer holds every connection it accepts and reads nothing, so the
/// request sits in the socket buffer and the client is left waiting on the
/// head alone. It spends [`super::HEAD_TIMEOUT`] of real wall clock, for the
/// reason [`a_stalled_blob_fetch_is_reported_rather_than_parking_the_caller`]
/// spends its fifteen seconds; the mechanism is pinned in milliseconds by
/// `a_head_that_never_comes_is_reported_on_its_own_bound`.
#[tokio::test]
async fn a_handshake_whose_reply_never_starts_is_reported_rather_than_parking_the_driver() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("the bound address")
    );
    let silent = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let mut t = SseTransport::connect(&base).with_device_signer(signer(NOW_MS));

    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        t.send_frame(hello_frame()),
    )
    .await
    .expect("the wait is bounded by the client, not by this harness")
    .expect_err("the peer never answered");
    silent.abort();

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m == "no response head within 15s"),
        "a silent peer is reported as unavailable, which is what the driver turns into a \
             disconnect and a backoff, and the diagnostic names the head bound that ran: {err}"
    );
    let elapsed = started.elapsed();
    // A lower bound only: the diagnostic names the fifteen seconds that ran,
    // and the harness timeout bounds the wait.
    assert!(
        elapsed >= std::time::Duration::from_secs(11),
        "it came back on the head bound, and not before it: {elapsed:?}"
    );
    assert!(
        t.session.is_none(),
        "no session is recorded out of a reply that never began"
    );
}

/// A handshake reply past the event cap is refused rather than held.
///
/// The other half of the same bound. Nothing legitimate on this route is
/// anywhere near eight mebibytes — every reply it reads is a small JSON
/// document — so the cap is not sizing the reply; it is denying a relay the
/// choice of how much memory this client holds. Before the read was capped
/// that choice was the relay's alone.
#[tokio::test]
async fn a_handshake_reply_past_the_event_cap_is_refused_rather_than_held() {
    let oversized = format!(r#"{{"session_id":"{}"}}"#, "x".repeat(FRAME_CAP));
    assert!(oversized.len() > FRAME_CAP, "the reply is past the cap");
    let relay = relay::start(vec![relay::Canned::json(200, &oversized)]).await;
    let mut t = dial(&relay);

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.send_frame(hello_frame()),
    )
    .await
    .expect("a capped read returns")
    .expect_err("the reply is past the cap");

    assert!(
        matches!(&err, TransportError::Unavailable(_)),
        "the relay sent more than was allowed, which is the relay's failure: {err}"
    );
    assert!(
        t.session.is_none(),
        "and nothing is read out of a body that was refused"
    );
}
