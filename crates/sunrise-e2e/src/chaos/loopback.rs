//! In-process channel-pair [`Transport`], the substrate the chaos harness wraps
//! with [`Toxic`](super::toxic::Toxic).
//!
//! [`loopback_pair`] returns two connected [`LoopbackEnd`]s backed by a pair of
//! `tokio::sync::mpsc` channels (one per direction). Delivery is async and FIFO
//! — a receiver `.await`s the next frame rather than polling a shared `Vec`.
//!
//! Close semantics follow the [`Transport`] contract (`recv_frame` yields
//! `Ok(None)` on graceful close): closing one end drops its outbound sender, so
//! the peer's `recv_frame` observes end-of-stream and returns `Ok(None)`.
//! Dropping a [`LoopbackEnd`] outright has the same effect.

use async_trait::async_trait;
use sunrise_sync::transport::{Transport, TransportError};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// One end of a bidirectional in-process transport. Construct a connected pair
/// with [`loopback_pair`].
#[derive(Debug)]
pub struct LoopbackEnd {
    /// Outbound sender toward the peer. `None` once this end is closed.
    tx: Option<UnboundedSender<Vec<u8>>>,
    /// Inbound receiver from the peer.
    rx: UnboundedReceiver<Vec<u8>>,
}

/// Create a connected pair of loopback endpoints. Frames sent on one arrive, in
/// order, on the other.
#[must_use]
pub fn loopback_pair() -> (LoopbackEnd, LoopbackEnd) {
    // a_to_b carries A's sends to B; b_to_a carries B's sends to A.
    let (a_to_b_tx, a_to_b_rx) = mpsc::unbounded_channel();
    let (b_to_a_tx, b_to_a_rx) = mpsc::unbounded_channel();
    let a = LoopbackEnd {
        tx: Some(a_to_b_tx),
        rx: b_to_a_rx,
    };
    let b = LoopbackEnd {
        tx: Some(b_to_a_tx),
        rx: a_to_b_rx,
    };
    (a, b)
}

#[async_trait]
impl Transport for LoopbackEnd {
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        match &self.tx {
            Some(tx) => tx
                .send(frame)
                .map_err(|_| TransportError::Unavailable("loopback peer closed".into())),
            None => Err(TransportError::Cancelled),
        }
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        // `recv` yields `None` once every sender (the peer's `tx`) is dropped,
        // which is exactly the graceful-close signal the Transport trait wants.
        Ok(self.rx.recv().await)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        // Drop our outbound sender so the peer's `recv_frame` returns `Ok(None)`.
        self.tx = None;
        Ok(())
    }
}
