//! The client half of `/sync` authentication, end to end (issue #7).
//!
//! `sunrise-server/tests/ws_auth.rs` proves the *server* refuses an
//! unauthenticated upgrade — but it dials with raw `tokio_tungstenite` and
//! hand-built headers. The production client dials through
//! [`sunrise_sync::WsTransport`], which sent no `Authorization` header at all,
//! so the two halves had never met: every real deployment would have refused
//! every real client, and nothing in the suite would have noticed.
//!
//! These tests put the real transport in front of the real relay.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;

use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::{ServerConfig, StaticVerifier, Subject};
use sunrise_sync::{TokenSource, Transport, TransportError, WsTransport};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};

const ISSUER: &str = "https://idp.example";

fn hello() -> Hello {
    Hello {
        client_app_v: "1.0.0+test".into(),
        client_platform: "test".into(),
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        doc_schema_min: u32::from(DOC_SCHEMA_FLOOR),
        doc_schema_max: u32::from(DOC_SCHEMA_V),
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

/// A relay that actually checks bearers, unlike the harness default.
async fn spawn_authenticating_relay() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    spawn_relay_with(ServerConfig::default(), |s| {
        s.with_verifier(Arc::new(
            StaticVerifier::default().with("alice-token", Subject::new(ISSUER, "alice")),
        ))
    })
    .await
}

/// Drive the Sunrise handshake over an established transport.
async fn handshake(t: &mut WsTransport) {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    t.send_frame(encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap())
        .await
        .expect("send hello");
    let buf = t
        .recv_frame()
        .await
        .expect("recv")
        .expect("a HelloAck, not a close");
    assert_eq!(decode_frame(&buf).unwrap().0.msg_kind, MsgKind::HelloAck);
}

/// The headline: the production transport reaches a relay that authenticates.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_transport_authenticates_against_a_real_relay() {
    let (addr, h) = spawn_authenticating_relay().await;
    let mut t = WsTransport::connect_with_bearer(&format!("ws://{addr}/sync"), Some("alice-token"))
        .await
        .expect("a valid bearer must upgrade");
    handshake(&mut t).await;
    h.abort();
}

/// And the same transport with no bearer is refused — which is what the whole
/// codebase did, silently, before the header existed.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_transport_without_a_bearer_is_refused() {
    let (addr, h) = spawn_authenticating_relay().await;
    let res = WsTransport::connect(&format!("ws://{addr}/sync")).await;
    assert!(
        matches!(res, Err(TransportError::Unavailable(_))),
        "an unauthenticated upgrade must fail at the transport"
    );
    h.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_bearer_is_refused() {
    let (addr, h) = spawn_authenticating_relay().await;
    let res =
        WsTransport::connect_with_bearer(&format!("ws://{addr}/sync"), Some("not-alices")).await;
    assert!(matches!(res, Err(TransportError::Unavailable(_))));
    h.abort();
}

/// The reason the credential is a shared cell rather than a captured `String`.
///
/// A transport factory is built once and called on every reconnect for the life
/// of the process. Writing a renewed token into the source must reach the
/// *next* connect — otherwise a client keeps presenting the token it started
/// with, and reconnects begin failing an hour in with no code change to blame.
#[tokio::test(flavor = "multi_thread")]
async fn a_renewed_token_reaches_the_next_connect() {
    let (addr, h) = spawn_authenticating_relay().await;
    let url = format!("ws://{addr}/sync");

    // The factory shape the CLI and the bindings both build: read per attempt.
    let credential = TokenSource::new(Some("stale-token".into()));
    let dial = {
        let credential = credential.clone();
        let url = url.clone();
        move || {
            let bearer = credential.get();
            let url = url.clone();
            async move { WsTransport::connect_with_bearer(&url, bearer.as_deref()).await }
        }
    };

    assert!(
        dial().await.is_err(),
        "the stale token is not one this relay knows"
    );

    credential.set(Some("alice-token".into()));
    let mut t = dial()
        .await
        .expect("the renewed token must reach the next connect");
    handshake(&mut t).await;
    h.abort();
}
