//! Mid-session token expiry on `/sync` (issue #7).
//!
//! `ws.rs` authenticated once, at the upgrade, and then never looked at the
//! credential again. A `/sync` session is long-lived and a bearer is not, so a
//! socket opened with a valid token stayed authenticated for as long as it
//! stayed open — hours past the token's own `exp`, indefinitely if the client
//! simply never disconnected. That is the failure these tests close.
//!
//! # What "expired" has to mean on the wire
//!
//! Not just "the socket dropped". A client that is disconnected without being
//! told why cannot tell an aged-out credential from a revoked one, and the two
//! call for opposite behaviour: renew silently, or stop and ask the user. So
//! the session ends with `Error` then `Close`, both carrying
//! [`ErrorCode::AuthTokenExpired`], per `docs/06-server/auth.md`.
//!
//! # Two independent clocks, on purpose
//!
//! Expiry is enforced twice, and the tests exercise both paths separately
//! because they fail in different circumstances:
//!
//! - **On every inbound frame**, against the injected `Clock`. This is what
//!   the spec asks for and what a busy session hits first.
//! - **On a monotonic timer arm** in the session select. An idle session sends
//!   nothing, so the inbound check never runs; without the timer a client
//!   could connect, go quiet, and hold an authenticated socket open forever on
//!   a dead credential. This is the case the obvious implementation misses.
//!
//! # The refresh frame
//!
//! `0x12 RefreshToken` is the escape from that close: the device's own OIDC
//! client gets a new access token from the issuer and hands it over in-band,
//! the server re-verifies it, and the deadline moves out with no reconnect.
//! The interesting tests are the refusals, and they are not all the same
//! refusal — see `handle_refresh`'s docs. A token that fails to verify leaves
//! the session running on the credential it still has; a token that verifies
//! but names a *different principal* ends it, because the channel namespace
//! was fixed at the upgrade and is never re-derived.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use sunrise_error::ErrorCode;
use sunrise_server::{build_router, Clock, ServerConfig, ServerState, StaticVerifier, Subject};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, Capability, CapabilityBits, ClosePayload, ErrorPayload, FrameFlags,
    Hello, HelloAck, MsgKind, RefreshTokenAckPayload, RefreshTokenPayload, SubscribeEntry,
    SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const ISSUER: &str = "https://idp.example";
const T0_MS: u64 = 1_704_067_200_000;

/// A clock the test pins, so "expired" is a fact about the credential rather
/// than a race against wall time.
#[derive(Debug)]
struct FixedClock(u64);

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

async fn boot(
    verifier: StaticVerifier,
    now_ms: u64,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ServerState::with_clock(ServerConfig::default(), Arc::new(FixedClock(now_ms)))
        .with_verifier(Arc::new(verifier));
    let app = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
}

async fn connect(addr: std::net::SocketAddr, bearer: &str) -> Ws {
    let mut req = format!("ws://{addr}/sync").into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .expect("upgrade")
}

fn hello() -> Hello {
    Hello {
        client_app_v: "1.0.0+test".into(),
        client_platform: "test".into(),
        wire_proto_supported: vec![1],
        doc_schema_min: 1,
        doc_schema_max: 1,
        crypto_suite_supported: vec![1],
        capabilities: REQUIRED_CLIENT_BITS.0
            | REQUIRED_SERVER_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

/// Hello + HelloAck only. Deliberately does not subscribe: an expired session
/// must be able to fail on the very first frame it is asked to act on.
async fn handshake(ws: &mut Ws) -> CapabilityBits {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    let Message::Binary(buf) = msg else {
        panic!("expected a binary HelloAck")
    };
    let (h, payload) = decode_frame(&buf).unwrap();
    assert_eq!(h.msg_kind, MsgKind::HelloAck);
    let ack: HelloAck = ciborium::de::from_reader(&payload[..]).unwrap();
    CapabilityBits(ack.capabilities)
}

async fn send_subscribe(ws: &mut Ws, stream: [u8; 16]) {
    let sub = SubscribePayload {
        streams: vec![SubscribeEntry {
            cursors: vec![],
            stream_id: stream,
        }],
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&sub, &mut buf).unwrap();
    let frame = encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &buf).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
}

/// The next binary frame, or `None` if `window` elapses first.
async fn next_frame(ws: &mut Ws, window: Duration) -> Option<(MsgKind, Vec<u8>)> {
    let deadline = tokio::time::sleep(window);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return None,
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Binary(buf))) => {
                        let (h, payload) = decode_frame(&buf).expect("decodable frame");
                        return Some((h.msg_kind, payload));
                    }
                    // A WebSocket-level close or a ping between our frames is
                    // not the end of the sequence; only the stream ending is.
                    Some(Ok(_)) => {}
                    _ => return None,
                }
            }
        }
    }
}

/// Assert the session ended the documented way: `Error` then `Close`, both
/// carrying `AUTH_TOKEN_EXPIRED`.
async fn assert_expired_close(ws: &mut Ws) {
    let (kind, payload) = next_frame(ws, Duration::from_secs(3))
        .await
        .expect("an expired session must say so, not just drop");
    assert_eq!(kind, MsgKind::Error, "the Error frame comes first");
    assert_eq!(
        ErrorPayload::decode(&payload).unwrap().code,
        ErrorCode::AuthTokenExpired
    );

    let (kind, payload) = next_frame(ws, Duration::from_secs(3))
        .await
        .expect("and is followed by a Close");
    assert_eq!(kind, MsgKind::Close);
    let close = ClosePayload::decode(&payload).unwrap();
    assert_eq!(close.code, ErrorCode::AuthTokenExpired);
    assert!(
        close.is_recoverable(),
        "an expiry is something the client fixes by renewing, not by prompting the user"
    );
}

/// The busy path: the session checks `exp` before acting on an inbound frame,
/// so the Subscribe is never honoured.
#[tokio::test]
async fn an_inbound_frame_on_an_expired_token_ends_the_session() {
    let verifier = StaticVerifier::default().with_expiring(
        "alice-token",
        Subject::new(ISSUER, "alice"),
        T0_MS, // deadline exactly at "now" — expiry is inclusive
    );
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    send_subscribe(&mut ws, [7u8; 16]).await;
    assert_expired_close(&mut ws).await;
    h.abort();
}

/// The idle path, and the reason the timer arm exists. This client sends
/// nothing after the handshake, so the per-frame check never runs. Without the
/// deadline arm the socket would stay authenticated indefinitely.
#[tokio::test]
async fn an_idle_session_is_closed_when_its_token_expires() {
    // The deadline is real time away, so only the monotonic arm can fire it.
    let verifier = StaticVerifier::default().with_expiring(
        "alice-token",
        Subject::new(ISSUER, "alice"),
        T0_MS + 400,
    );
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    // Nothing is sent from here on.
    assert_expired_close(&mut ws).await;
    h.abort();
}

/// The negative control. Isolation must not be achieved by closing everything:
/// a verifier that issues no deadline — self-host's `NullVerifier`, and any
/// `StaticVerifier` bearer registered without one — must keep its session.
///
/// `None` is "no deadline applies", not "expired at time zero". Conflating the
/// two would take self-host offline.
#[tokio::test]
async fn a_session_with_no_deadline_is_never_closed() {
    let verifier = StaticVerifier::default().with("alice-token", Subject::new(ISSUER, "alice"));
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    send_subscribe(&mut ws, [7u8; 16]).await;
    let (kind, _) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the subscribe must be honoured");
    assert_eq!(
        kind,
        MsgKind::StreamUpdate,
        "expected the CaughtUp marker, not a close"
    );
    assert!(
        next_frame(&mut ws, Duration::from_millis(600))
            .await
            .is_none(),
        "a session with no deadline must stay open"
    );
    h.abort();
}

async fn send_refresh(ws: &mut Ws, token: &str) {
    let payload = RefreshTokenPayload {
        token: token.to_string(),
    }
    .encode()
    .unwrap();
    let frame = encode_frame(MsgKind::RefreshToken, FrameFlags::EMPTY, &payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
}

/// The happy path, and the reason the frame exists: a renewal costs no
/// reconnect. The session opens on a token about to expire, refreshes onto one
/// that is not, and outlives the first deadline it would otherwise have died
/// at.
#[tokio::test]
async fn a_refresh_extends_the_session_without_a_reconnect() {
    let verifier = StaticVerifier::default()
        .with_expiring("short", Subject::new(ISSUER, "alice"), T0_MS + 500)
        .with_expiring("long", Subject::new(ISSUER, "alice"), T0_MS + 3_600_000);
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "short").await;
    let _ = handshake(&mut ws).await;

    send_refresh(&mut ws, "long").await;

    // The refresh is acknowledged explicitly, carrying the deadline the server
    // actually adopted. Silence used to be the only signal of success, which
    // an old server ignoring the frame produces too.
    let (kind, payload) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("a successful refresh must be acknowledged");
    assert_eq!(kind, MsgKind::RefreshTokenAck);
    let ack = RefreshTokenAckPayload::decode(&payload).unwrap();
    assert_eq!(
        ack.expires_at_ms,
        T0_MS + 3_600_000,
        "the ack reports the new token's expiry, not the old one"
    );

    // Well past the original 500 ms deadline. Without the refresh this window
    // is where `an_idle_session_is_closed_when_its_token_expires` fires.
    assert!(
        next_frame(&mut ws, Duration::from_millis(1_200))
            .await
            .is_none(),
        "the renewed session must still be open past its original deadline"
    );

    // And it is still a working session, not merely an unclosed socket.
    send_subscribe(&mut ws, [7u8; 16]).await;
    let (kind, _) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the subscribe must be honoured");
    assert_eq!(kind, MsgKind::StreamUpdate);
    h.abort();
}

/// A refresh the verifier rejects is refused — and the session survives on the
/// credential it already holds, which is still valid. Tearing down here would
/// turn a recoverable client bug into a dropped session.
#[tokio::test]
async fn a_refresh_with_an_unverifiable_token_is_refused_but_keeps_the_session() {
    let verifier = StaticVerifier::default().with("alice-token", Subject::new(ISSUER, "alice"));
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    send_refresh(&mut ws, "not-a-token-this-server-knows").await;
    let (kind, payload) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the refusal must be reported");
    assert_eq!(kind, MsgKind::Error);
    assert_eq!(
        ErrorPayload::decode(&payload).unwrap().code,
        ErrorCode::AuthTokenInvalid
    );

    // Still live.
    send_subscribe(&mut ws, [7u8; 16]).await;
    let (kind, _) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the session must survive a rejected refresh");
    assert_eq!(kind, MsgKind::StreamUpdate);
    h.abort();
}

/// The account-takeover guard. A perfectly valid token for a *different*
/// principal must not renew this session: the relay channel namespace was
/// derived from the upgrade's token and is never re-derived, so continuing
/// would relay alice's traffic under bob's authority.
#[tokio::test]
async fn a_refresh_naming_another_principal_ends_the_session() {
    let verifier = StaticVerifier::default()
        .with("alice-token", Subject::new(ISSUER, "alice"))
        .with("bob-token", Subject::new(ISSUER, "bob"));
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    send_refresh(&mut ws, "bob-token").await;
    let (kind, payload) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the mismatch must be reported");
    assert_eq!(kind, MsgKind::Error);
    assert_eq!(
        ErrorPayload::decode(&payload).unwrap().code,
        ErrorCode::AuthTokenInvalid
    );
    assert!(
        next_frame(&mut ws, Duration::from_secs(2)).await.is_none(),
        "and the session must end, not merely decline the renewal"
    );
    h.abort();
}

/// Same guard, one step subtler: the principal matches but the token's
/// `device_id` claim does not. `auth::request::bind_device` refuses that on the
/// REST path; a session must not be able to change device mid-flight either.
#[tokio::test]
async fn a_refresh_naming_another_device_ends_the_session() {
    let mut other_device = Subject::new(ISSUER, "alice");
    other_device.device_id = Some("SOMEONE-ELSE".into());
    let verifier = StaticVerifier::default()
        .with("alice-token", Subject::new(ISSUER, "alice"))
        .with("alice-other-device", other_device);
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    send_refresh(&mut ws, "alice-other-device").await;
    let (kind, _) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the mismatch must be reported");
    assert_eq!(kind, MsgKind::Error);
    assert!(
        next_frame(&mut ws, Duration::from_secs(2)).await.is_none(),
        "the session must end"
    );
    h.abort();
}

/// A dead session cannot be resurrected. The per-frame expiry check runs before
/// dispatch, so a refresh arriving after the deadline is not a refresh at all —
/// it is the inbound frame that closes the session. The client's recourse is to
/// reconnect, which re-runs the whole upgrade pipeline: `allow_signup`, the
/// `(iss, sub)` account lookup, and the channel-namespace derivation.
#[tokio::test]
async fn a_refresh_after_the_deadline_does_not_revive_the_session() {
    let verifier = StaticVerifier::default()
        .with_expiring("short", Subject::new(ISSUER, "alice"), T0_MS)
        .with_expiring("long", Subject::new(ISSUER, "alice"), T0_MS + 3_600_000);
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "short").await;
    let _ = handshake(&mut ws).await;

    send_refresh(&mut ws, "long").await;
    assert_expired_close(&mut ws).await;
    h.abort();
}

/// A malformed payload under a well-formed frame is a client bug, not an auth
/// failure: report it and keep going.
#[tokio::test]
async fn a_refresh_with_an_undecodable_payload_is_refused() {
    let verifier = StaticVerifier::default().with("alice-token", Subject::new(ISSUER, "alice"));
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    let frame = encode_frame(MsgKind::RefreshToken, FrameFlags::EMPTY, b"\xff not cbor").unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
    let (kind, payload) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the refusal must be reported");
    assert_eq!(kind, MsgKind::Error);
    assert_eq!(
        ErrorPayload::decode(&payload).unwrap().code,
        ErrorCode::SyncOpInvalid
    );
    h.abort();
}

/// The relay advertises `SrvTokenRefresh`, so a client can tell in advance
/// that an in-band refresh will be answered rather than ignored.
#[tokio::test]
async fn the_relay_advertises_the_token_refresh_capability() {
    let verifier = StaticVerifier::default().with("alice-token", Subject::new(ISSUER, "alice"));
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let agreed = handshake(&mut ws).await;
    assert!(
        agreed.has(Capability::SrvTokenRefresh),
        "the capability must come back agreed"
    );
    h.abort();
}

/// The self-host verifier issues no expiry, and the ack says so with a zero
/// rather than a stale or invented deadline.
#[tokio::test]
async fn an_ack_for_a_token_with_no_expiry_reports_zero() {
    let verifier = StaticVerifier::default().with("alice-token", Subject::new(ISSUER, "alice"));
    let (addr, h) = boot(verifier, T0_MS).await;
    let mut ws = connect(addr, "alice-token").await;
    let _ = handshake(&mut ws).await;

    send_refresh(&mut ws, "alice-token").await;
    let (kind, payload) = next_frame(&mut ws, Duration::from_secs(2))
        .await
        .expect("the refresh must be acknowledged");
    assert_eq!(kind, MsgKind::RefreshTokenAck);
    assert_eq!(
        RefreshTokenAckPayload::decode(&payload)
            .unwrap()
            .expires_at_ms,
        0,
        "no deadline is 0, which the client reads as no deadline"
    );
    h.abort();
}
