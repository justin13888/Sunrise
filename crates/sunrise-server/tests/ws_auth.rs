//! Authentication and tenancy isolation on `/sync`.
//!
//! Before this suite, `/sync` upgraded any socket and every session resolved to
//! one synthetic account, so a Subscribe from any client was served frames
//! belonging to every other client on the server. `ServerState` carried a
//! `TokenVerifier` whose only call sites were its own unit tests.
//!
//! These tests boot a real server, drive a real WebSocket, and assert the
//! property that matters: **a client can only ever receive frames published
//! within its own verified account.** The account half of the channel key is
//! derived from the token, not from anything the client sends, so there is no
//! frame a client can craft to reach another tenant.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use sunrise_server::{build_router, ServerConfig, ServerState, StaticVerifier, Subject};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, OpBatchPayload, SubscribeEntry,
    SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Two accounts, each with its own bearer.
fn two_tenant_verifier() -> StaticVerifier {
    StaticVerifier::default()
        .with("alice-token", Subject::new("https://idp.example", "alice"))
        .with("bob-token", Subject::new("https://idp.example", "bob"))
}

async fn boot() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state =
        ServerState::new(ServerConfig::default()).with_verifier(Arc::new(two_tenant_verifier()));
    let app = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, handle)
}

/// Connect with an optional bearer. Returns `Err` if the upgrade is refused.
async fn connect(
    addr: std::net::SocketAddr,
    bearer: Option<&str>,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/sync").into_client_request().unwrap();
    if let Some(b) = bearer {
        req.headers_mut()
            .insert("Authorization", format!("Bearer {b}").parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
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

/// Handshake and subscribe to `stream`, draining the CaughtUp marker.
async fn handshake_and_subscribe(ws: &mut Ws, stream: [u8; 16]) {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
    // HelloAck
    let _ = ws.next().await.unwrap().unwrap();

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
    // CaughtUp for the (empty) backlog.
    let _ = ws.next().await.unwrap().unwrap();
}

async fn publish(ws: &mut Ws, stream: [u8; 16], marker: u8) {
    let batch = OpBatchPayload {
        ops: vec![vec![marker; 8]],
        batch_id: u64::from(marker),
        stream_id: stream,
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&batch, &mut buf).unwrap();
    let frame = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &buf).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
}

/// Collect frames of `kind` arriving within a short window.
async fn collect_kind(ws: &mut Ws, kind: MsgKind, window_ms: u64) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let deadline = tokio::time::sleep(std::time::Duration::from_millis(window_ms));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            msg = ws.next() => {
                let Some(Ok(Message::Binary(buf))) = msg else { break };
                if let Ok((h, payload)) = decode_frame(&buf) {
                    if h.msg_kind == kind {
                        out.push(payload.clone());
                    }
                }
            }
        }
    }
    out
}

#[tokio::test]
async fn unauthenticated_upgrade_is_refused() {
    let (addr, h) = boot().await;
    let err = connect(addr, None)
        .await
        .expect_err("a socket with no bearer must not be upgraded");
    // tungstenite surfaces a non-101 response as an HTTP error.
    assert!(
        format!("{err}").contains("401")
            || matches!(err, tokio_tungstenite::tungstenite::Error::Http(_)),
        "expected a 401 on upgrade, got {err}"
    );
    h.abort();
}

#[tokio::test]
async fn unknown_bearer_is_refused() {
    let (addr, h) = boot().await;
    assert!(
        connect(addr, Some("forged-token")).await.is_err(),
        "an unrecognised bearer must not be upgraded"
    );
    h.abort();
}

#[tokio::test]
async fn valid_bearer_is_accepted() {
    let (addr, h) = boot().await;
    let mut ws = connect(addr, Some("alice-token"))
        .await
        .expect("a valid bearer must upgrade");
    handshake_and_subscribe(&mut ws, [9u8; 16]).await;
    h.abort();
}

/// The headline property. Alice and Bob subscribe to the **same stream id**;
/// Alice publishes; Bob must receive nothing, because the account half of the
/// channel key comes from his own token.
#[tokio::test]
async fn one_tenant_never_receives_another_tenants_frames() {
    let (addr, h) = boot().await;
    let stream = [7u8; 16];

    let mut alice = connect(addr, Some("alice-token")).await.unwrap();
    let mut bob = connect(addr, Some("bob-token")).await.unwrap();
    handshake_and_subscribe(&mut alice, stream).await;
    handshake_and_subscribe(&mut bob, stream).await;

    publish(&mut alice, stream, 0xAA).await;

    let leaked = collect_kind(&mut bob, MsgKind::OpBatch, 300).await;
    assert!(
        leaked.is_empty(),
        "bob received {} frame(s) published by alice on an identical stream id — \
         cross-tenant isolation is broken",
        leaked.len()
    );
    h.abort();
}

/// The other half: isolation must not be achieved by breaking delivery.
/// Two connections on the *same* account still fan out to each other.
#[tokio::test]
async fn two_sessions_of_one_tenant_still_fan_out() {
    let (addr, h) = boot().await;
    let stream = [3u8; 16];

    let mut sender = connect(addr, Some("alice-token")).await.unwrap();
    let mut receiver = connect(addr, Some("alice-token")).await.unwrap();
    handshake_and_subscribe(&mut sender, stream).await;
    handshake_and_subscribe(&mut receiver, stream).await;

    publish(&mut sender, stream, 0xBB).await;

    let got = collect_kind(&mut receiver, MsgKind::OpBatch, 500).await;
    assert_eq!(
        got.len(),
        1,
        "a second session on the same account must receive the frame"
    );
    h.abort();
}
