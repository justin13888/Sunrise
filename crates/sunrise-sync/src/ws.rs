//! Concrete WebSocket client [`Transport`] (feature `ws`).
//!
//! Wraps [`tokio_tungstenite`] so the sync driver can talk to the relay's
//! `/sync` endpoint. Frames are already fully encoded by
//! `sunrise-wire-protocol::frame` before they reach this layer, so the
//! transport is a thin byte pipe: every [`Transport::send_frame`] rides a
//! single WebSocket **binary** message and every [`Transport::recv_frame`]
//! yields the next binary message's payload. Control frames (ping / pong /
//! text) are transparently skipped — tungstenite answers pings itself on the
//! next write — so the driver only ever sees protocol frames.
//!
//! This module is gated behind the `ws` feature so no-feature builds (TUI with
//! sync off, unit tests of the pure state machine) don't pull in the
//! networking stack.

use crate::transport::{Transport, TransportError};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};

/// WebSocket client transport over `ws://` / `wss://`.
pub struct WsTransport {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl std::fmt::Debug for WsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsTransport").finish_non_exhaustive()
    }
}

impl WsTransport {
    /// Dial `url` (e.g. `wss://relay.example/sync`) unauthenticated.
    ///
    /// Only a self-host relay running `NullVerifier` accepts this. Every other
    /// deployment refuses the upgrade with `401`, so prefer
    /// [`WsTransport::connect_with_bearer`].
    ///
    /// # Errors
    /// [`TransportError::Unavailable`] if the TCP/TLS/WS handshake fails.
    pub async fn connect(url: &str) -> Result<Self, TransportError> {
        Self::connect_with_bearer(url, None).await
    }

    /// Dial `url` and complete the WS handshake, presenting `bearer` as
    /// `Authorization: Bearer …` on the **upgrade request**.
    ///
    /// The relay authenticates at the upgrade — `docs/06-server/auth.md` — so
    /// the header has to be on the handshake, not on a later frame. Nothing
    /// here sent one, which is why every real deployment refused the
    /// connection and only the self-host `NullVerifier` path worked.
    ///
    /// The application-level Sunrise `Hello` handshake is driven by the sync
    /// driver on top of this transport.
    ///
    /// # Errors
    /// [`TransportError::Unavailable`] if the URL is not a valid WebSocket
    /// request target, if the bearer cannot be rendered as a header value, or
    /// if the TCP/TLS/WS handshake fails. A relay that rejects the credential
    /// surfaces here too, as the `401` on the upgrade.
    pub async fn connect_with_bearer(
        url: &str,
        bearer: Option<&str>,
    ) -> Result<Self, TransportError> {
        let mut request = url
            .into_client_request()
            .map_err(|e| TransportError::Unavailable(e.to_string()))?;
        if let Some(token) = bearer {
            // A token with a newline or a non-ASCII byte in it would otherwise
            // be a header-injection vector; `HeaderValue::try_from` is what
            // refuses it. The error deliberately does not quote the token.
            let value = HeaderValue::try_from(format!("Bearer {token}")).map_err(|_| {
                TransportError::Unavailable("bearer token is not a valid header value".to_string())
            })?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }
        let (ws, _resp) = connect_async(request)
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))?;
        Ok(Self { ws })
    }
}

#[async_trait]
impl Transport for WsTransport {
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.ws
            .send(Message::Binary(frame))
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes)),
                // Graceful close (peer or stream end): no more frames.
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                // Control / text frames are not protocol frames; skip. Pings are
                // answered internally by tungstenite on the next flush.
                Some(Ok(
                    Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_),
                )) => {}
                Some(Err(e)) => return Err(TransportError::Unavailable(e.to_string())),
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.ws
            .close(None)
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The feature compiles and a bad URL surfaces a transport error rather
    /// than panicking. A real socket round-trip is covered by the e2e slice.
    #[tokio::test]
    async fn connect_bad_url_errors() {
        let res = WsTransport::connect("ws://127.0.0.1:1/sync").await;
        assert!(matches!(res, Err(TransportError::Unavailable(_))));
    }

    /// A token containing a header separator must be refused locally rather
    /// than concatenated into the request. Injecting a CR/LF here would let a
    /// credential smuggle extra headers into the upgrade.
    #[tokio::test]
    async fn a_bearer_that_is_not_a_valid_header_value_is_refused() {
        for bad in ["line\r\nX-Injected: yes", "with\nnewline", "nul\0byte"] {
            let res = WsTransport::connect_with_bearer("ws://127.0.0.1:1/sync", Some(bad)).await;
            match res {
                Err(TransportError::Unavailable(msg)) => {
                    assert!(
                        !msg.contains("X-Injected") && !msg.contains(bad),
                        "the error must not quote the credential: {msg}"
                    );
                }
                other => panic!("expected a refusal for {bad:?}, got {other:?}"),
            }
        }
    }

    /// An ordinary token reaches the dial. The connection still fails (nothing
    /// is listening), but it fails at the *socket*, not at header construction
    /// — which is what distinguishes "the header was built" from "the header
    /// was rejected".
    #[tokio::test]
    async fn an_ordinary_bearer_gets_as_far_as_the_socket() {
        let res =
            WsTransport::connect_with_bearer("ws://127.0.0.1:1/sync", Some("eyJhbGciOiJSUzI1NiJ9"))
                .await;
        let Err(TransportError::Unavailable(msg)) = res else {
            panic!("expected a connection failure")
        };
        assert!(
            !msg.contains("header value"),
            "should have failed dialling, not building the header: {msg}"
        );
    }

    #[test]
    fn transport_is_object_safe() {
        // Compile-time assertion that `WsTransport` fits the driver's
        // `Box<dyn Transport>` factory shape.
        fn _assert(_: Box<dyn Transport>) {}
    }
}
