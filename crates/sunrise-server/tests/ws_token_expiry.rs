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

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use sunrise_error::ErrorCode;
use sunrise_server::{build_router, Clock, ServerConfig, ServerState, StaticVerifier, Subject};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, ClosePayload, ErrorPayload, FrameFlags, Hello, MsgKind,
    SubscribeEntry, SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
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
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

/// Hello + HelloAck only. Deliberately does not subscribe: an expired session
/// must be able to fail on the very first frame it is asked to act on.
async fn handshake(ws: &mut Ws) {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    let Message::Binary(buf) = msg else {
        panic!("expected a binary HelloAck")
    };
    assert_eq!(decode_frame(&buf).unwrap().0.msg_kind, MsgKind::HelloAck);
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
    handshake(&mut ws).await;

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
    handshake(&mut ws).await;

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
    handshake(&mut ws).await;

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
