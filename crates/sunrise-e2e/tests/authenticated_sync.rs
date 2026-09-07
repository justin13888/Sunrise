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
use sunrise_sync::{SseTransport, TokenSource, Transport, TransportError};
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

/// Drive the Sunrise handshake over a transport, returning what it answered.
///
/// The credential is checked when the handshake is *sent*, not when the
/// transport is built: ADR-0023 replaced the upgrade — the one moment the old
/// socket had to authenticate at — with ordinary requests, so there is no
/// connect to fail. The refusal simply arrives one step later.
async fn handshake(t: &mut SseTransport) -> Result<(), TransportError> {
    handshake_with(t, hello()).await
}

/// [`handshake`], with a `Hello` the caller composed.
async fn handshake_with(t: &mut SseTransport, hello: Hello) -> Result<(), TransportError> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello, &mut payload).unwrap();
    t.send_frame(encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap())
        .await?;
    let buf = t.recv_frame().await?.expect("a HelloAck, not a close");
    assert_eq!(decode_frame(&buf).unwrap().0.msg_kind, MsgKind::HelloAck);
    Ok(())
}

/// The headline: the production transport reaches a relay that authenticates.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_transport_authenticates_against_a_real_relay() {
    let (addr, h) = spawn_authenticating_relay().await;
    let mut t = SseTransport::connect_with_bearer(&format!("http://{addr}"), Some("alice-token"));
    handshake(&mut t)
        .await
        .expect("a valid bearer must establish");
    h.abort();
}

/// And the same transport with no bearer is refused — which is what the whole
/// codebase did, silently, before the header existed.
#[tokio::test(flavor = "multi_thread")]
async fn the_real_transport_without_a_bearer_is_refused() {
    let (addr, h) = spawn_authenticating_relay().await;
    let mut t = SseTransport::connect(&format!("http://{addr}"));
    assert!(
        matches!(handshake(&mut t).await, Err(TransportError::Server { .. })),
        "an unauthenticated session must be refused"
    );
    h.abort();
}

/// A negotiation refusal reaches the real client as the refusal's own code.
///
/// The two halves had never met here either. The relay computed a typed code
/// for each of the four negotiation failures and then threw it away, sending
/// `VALIDATION_INVALID`; `SseTransport::refuse` does not recognise that name,
/// so it fell through to the status map and every refusal arrived as
/// `SYNC_OP_INVALID`. A client cannot tell "update the app" from "update the
/// relay" out of that, which is what `docs/05-sync/wire-protocol.md`
/// §Versioning and `docs/10-cross-cutting/protocol-versioning.md` §4 both
/// promise it can.
#[tokio::test(flavor = "multi_thread")]
async fn a_negotiation_refusal_reaches_the_client_as_its_own_code() {
    let (addr, h) = spawn_authenticating_relay().await;
    let mut t = SseTransport::connect_with_bearer(&format!("http://{addr}"), Some("alice-token"));
    let unnegotiable = Hello {
        wire_proto_supported: vec![9999],
        ..hello()
    };
    match handshake_with(&mut t, unnegotiable).await {
        Err(TransportError::Server { code, .. }) => assert_eq!(
            code, "SYNC_PROTOCOL_VERSION_MISMATCH",
            "the client has to be able to say which half is stale"
        ),
        other => panic!("expected a typed server refusal, got {other:?}"),
    }
    h.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_bearer_is_refused() {
    let (addr, h) = spawn_authenticating_relay().await;
    let mut t = SseTransport::connect_with_bearer(&format!("http://{addr}"), Some("not-alices"));
    assert!(matches!(
        handshake(&mut t).await,
        Err(TransportError::Server { .. })
    ));
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
    let url = format!("http://{addr}");

    // The factory shape the CLI and the bindings both build: read per attempt.
    let credential = TokenSource::new(Some("stale-token".into()));
    let dial = {
        let credential = credential.clone();
        let url = url.clone();
        move || {
            let bearer = credential.get();
            let url = url.clone();
            async move {
                let mut t = SseTransport::connect_with_bearer(&url, bearer.as_deref());
                handshake(&mut t).await.map(|()| t)
            }
        }
    };

    assert!(
        dial().await.is_err(),
        "the stale token is not one this relay knows"
    );

    credential.set(Some("alice-token".into()));
    let _ = dial()
        .await
        .expect("the renewed token must reach the next connect");
    h.abort();
}
