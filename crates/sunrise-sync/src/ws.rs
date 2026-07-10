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
    /// Dial `url` (e.g. `wss://relay.example/sync`) and complete the WS
    /// handshake. The application-level Sunrise `Hello` handshake is driven by
    /// the sync driver on top of this transport.
    ///
    /// # Errors
    /// [`TransportError::Unavailable`] if the TCP/TLS/WS handshake fails.
    pub async fn connect(url: &str) -> Result<Self, TransportError> {
        let (ws, _resp) = connect_async(url)
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

    #[test]
    fn transport_is_object_safe() {
        // Compile-time assertion that `WsTransport` fits the driver's
        // `Box<dyn Transport>` factory shape.
        fn _assert(_: Box<dyn Transport>) {}
    }
}
