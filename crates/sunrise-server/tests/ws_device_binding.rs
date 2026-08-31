//! Device binding on `/sync`.
//!
//! `docs/06-server/auth.md` promises that revoking a device stops it
//! authenticating "even if the OIDC token is still valid". That held for REST,
//! which runs the full `bind_device` pipeline on every request, and did not
//! hold here: the `/sync` upgrade resolved the account from the token and never
//! consulted the `devices` table at all. A revoked device kept an authenticated
//! socket, and kept receiving the account's fan-out, until its bearer happened
//! to expire — which for a long-lived token is indistinguishable from never.
//!
//! Three properties, and the third is the one revocation exists for:
//!
//! 1. an unregistered device cannot open a session;
//! 2. a revoked device cannot open a new one;
//! 3. a session **already open** ends when the device is revoked, including a
//!    session that is only receiving and never sends another frame.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use sunrise_server::store::NewDevice;
use sunrise_server::{build_router, ServerConfig, ServerState, StaticVerifier, Subject};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, ErrorPayload, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const BEARER: &str = "alice-token";

/// A verifier whose subject carries no `device_id` claim, so the header is the
/// only thing naming a device — the shape a client using a plain OIDC provider
/// has.
fn verifier() -> StaticVerifier {
    StaticVerifier::default().with(BEARER, Subject::new("https://idp.example", "alice"))
}

async fn boot(recheck_ms: u64) -> (std::net::SocketAddr, ServerState) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ServerConfig {
        device_recheck_ms: recheck_ms,
        ..ServerConfig::default()
    };
    let state = ServerState::new(config).with_verifier(Arc::new(verifier()));
    let app = build_router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, state)
}

/// Register a device against the account the bearer resolves to, returning its
/// id. Goes through the store directly: what is under test is `/sync`, not the
/// registration route, which `device_binding.rs` already covers.
fn register(state: &ServerState) -> (String, String) {
    let account = state
        .store
        .resolve_account(
            &Subject::new("https://idp.example", "alice"),
            true,
            state.clock.now_ms(),
        )
        .expect("resolve account");
    let new = NewDevice {
        device_pub_s: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
        device_pub_d: None,
        device_cert: None,
        nickname: "test-device".to_string(),
        platform: "cli".to_string(),
        app_version: Some("1.0.0+test".to_string()),
    };
    let device = state
        .store
        .register_device(&account.account_id, &new, state.clock.now_ms())
        .expect("register device");
    (account.account_id, device.device_id)
}

async fn connect(addr: std::net::SocketAddr, device: Option<&str>) -> Result<Ws, String> {
    let mut req = format!("ws://{addr}/sync").into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {BEARER}").parse().unwrap());
    if let Some(d) = device {
        req.headers_mut()
            .insert("X-Sunrise-Device", d.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
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

/// Hello + HelloAck. The session loop -- and with it the revocation re-check
/// arm -- only starts once negotiation completes, so a test that skips this
/// never reaches the code it means to exercise.
async fn handshake(ws: &mut Ws) {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    let Message::Binary(buf) = msg else {
        panic!("expected a binary HelloAck")
    };
    let (h, _) = decode_frame(&buf).unwrap();
    assert_eq!(h.msg_kind, MsgKind::HelloAck);
}

#[tokio::test]
async fn an_unregistered_device_cannot_open_a_sync_session() {
    let (addr, _state) = boot(30_000).await;
    let err = connect(addr, Some("dev_nobody"))
        .await
        .expect_err("an unregistered device must be refused");
    assert!(
        err.contains("403") || err.to_lowercase().contains("forbidden"),
        "expected a 403 on the upgrade, got: {err}"
    );
}

#[tokio::test]
async fn a_revoked_device_cannot_open_a_sync_session() {
    let (addr, state) = boot(30_000).await;
    let (account_id, device_id) = register(&state);

    connect(addr, Some(&device_id))
        .await
        .expect("an active device connects");

    state
        .store
        .revoke_device(&account_id, &device_id, state.clock.now_ms())
        .expect("revoke");

    let err = connect(addr, Some(&device_id))
        .await
        .expect_err("a revoked device must be refused");
    assert!(
        err.contains("403") || err.to_lowercase().contains("forbidden"),
        "expected a 403 on the upgrade, got: {err}"
    );
}

/// The property that matters most, and the one the gap actually cost: the
/// socket a revoked device is *already holding*.
///
/// This session sends nothing after connecting, so there is no inbound frame to
/// hang a check on. Without the periodic re-check arm it would keep receiving
/// the account's fan-out for the life of its token.
#[tokio::test]
async fn revoking_a_device_ends_the_session_it_already_holds() {
    let (addr, state) = boot(50).await;
    let (account_id, device_id) = register(&state);
    let mut ws = connect(addr, Some(&device_id))
        .await
        .expect("an active device connects");
    handshake(&mut ws).await;

    state
        .store
        .revoke_device(&account_id, &device_id, state.clock.now_ms())
        .expect("revoke");

    // The server should volunteer a typed Error then a typed Close, without
    // being prompted by anything the client sends.
    let mut saw_error = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next()).await;
        let Ok(Some(Ok(msg))) = next else { break };
        let Message::Binary(buf) = msg else {
            continue;
        };
        let (header, payload) = decode_frame(&buf).expect("a decodable frame");
        match header.msg_kind {
            MsgKind::Error => {
                let payload = ErrorPayload::decode(&payload).expect("decodable error payload");
                assert_eq!(
                    payload.code,
                    sunrise_error::ErrorCode::AuthDeviceRevoked,
                    "a revoked device must be told why, and told the code that means \
                     'stop and ask the user' rather than 'renew and reconnect'"
                );
                saw_error = true;
            }
            MsgKind::Close => break,
            _ => {}
        }
    }
    assert!(
        saw_error,
        "the session must be ended with a typed AUTH_DEVICE_REVOKED, not dropped silently"
    );
}
