//! Transport trait — abstracts over the WS / HTTP/2 long-poll wire.
//!
//! Concrete implementations live elsewhere:
//! - `sunrise-server` uses tokio-tungstenite for the WS server side.
//! - The desktop / web / TUI clients each plug their own concrete WS client.
//! - In-process simulation transports are used for the multi-device tests.

use async_trait::async_trait;
use thiserror::Error;

/// Transport-level errors.
#[derive(Debug, Error)]
pub enum TransportError {
    /// Network unreachable / connection refused.
    #[error("transport unavailable: {0}")]
    Unavailable(String),
    /// Authoritative server-issued error frame.
    #[error("server error: {code} {message}")]
    Server {
        /// Stable wire-error code.
        code: &'static str,
        /// Human-readable summary.
        message: String,
    },
    /// Encode/decode error somewhere in the framing or CBOR layer.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Operation cancelled (e.g., shutdown).
    #[error("transport cancelled")]
    Cancelled,
}

/// Async wire transport. Both client and server sides implement this.
///
/// Each call sends or receives a complete *frame* — the framing layer in
/// `sunrise-wire-protocol::frame` is the unit of work.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one already-encoded frame.
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError>;

    /// Await one decoded frame. Returns `None` on graceful close.
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError>;

    /// Initiate graceful close.
    async fn close(&mut self) -> Result<(), TransportError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// In-process transport that pairs two endpoints via shared queues.
    /// Used for unit-test convergence scenarios.
    struct LoopbackEnd {
        outbound: Arc<Mutex<Vec<Vec<u8>>>>,
        inbound: Arc<Mutex<Vec<Vec<u8>>>>,
        closed: bool,
    }

    #[async_trait]
    impl Transport for LoopbackEnd {
        async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
            if self.closed {
                return Err(TransportError::Cancelled);
            }
            self.outbound.lock().await.push(frame);
            Ok(())
        }

        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            if self.closed {
                return Ok(None);
            }
            let mut q = self.inbound.lock().await;
            Ok(q.pop())
        }

        async fn close(&mut self) -> Result<(), TransportError> {
            self.closed = true;
            Ok(())
        }
    }

    #[tokio::test]
    async fn loopback_send_recv() {
        let a_to_b = Arc::new(Mutex::new(Vec::new()));
        let b_to_a = Arc::new(Mutex::new(Vec::new()));
        let mut a = LoopbackEnd {
            outbound: a_to_b.clone(),
            inbound: b_to_a.clone(),
            closed: false,
        };
        let mut b = LoopbackEnd {
            outbound: b_to_a.clone(),
            inbound: a_to_b.clone(),
            closed: false,
        };
        a.send_frame(vec![1, 2, 3]).await.unwrap();
        let got = b.recv_frame().await.unwrap();
        assert_eq!(got, Some(vec![1, 2, 3]));
    }
}
