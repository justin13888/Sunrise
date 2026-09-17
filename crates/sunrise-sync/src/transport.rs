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
    /// This transport has no account API behind it.
    ///
    /// The loopback and in-process transports carry the op stream and nothing
    /// else; asking one of them to change an account's device list is a
    /// category error rather than a failure, and the caller distinguishes the
    /// two — an unsupported transport leaves the request queued for a
    /// transport that does support it, where a failure counts as an attempt.
    #[error("transport has no account API")]
    Unsupported,
}

/// What the relay said about a revocation it was asked to make.
///
/// Two arms rather than `Ok(())`, because the two are different facts about
/// the user's security posture and only one of them is the outcome they asked
/// for. Collapsing them is the defect PR #86 withdrew: it read a `404` as
/// success and logged "the relay has been told to stop accepting a revoked
/// device" about a request that had named a device the relay had never heard
/// of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeOutcome {
    /// The relay revoked it. It will take no more uploads from that device and
    /// will tear down any stream it is holding.
    Revoked,
    /// The relay holds no active device row carrying that vault id.
    ///
    /// Terminal — retrying cannot make a row appear — but **not** success. It
    /// covers a device that was already revoked, and it covers a device that
    /// registered before it sent a `vault_device_id`, which the relay is still
    /// accepting under a row this vault cannot name.
    Unknown,
}

/// What the relay committed a finalized blob as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobCommit {
    /// The relay's content address for the blob: the first sixteen bytes of
    /// BLAKE3 over its ciphertext. A client that already computed that hash
    /// knows this value before it asks, which is what lets another device fetch
    /// the blob without ever seeing a `finalize` response.
    pub blob_id: [u8; 16],
    /// Committed size in bytes.
    pub size_bytes: u64,
    /// Committed chunk count.
    pub chunk_count: u32,
}

/// Async wire transport. Both client and server sides implement this.
///
/// Each call sends or receives a complete *frame* — the framing layer in
/// `sunrise-wire-protocol::frame` is the unit of work.
///
/// # The four blob operations
///
/// They are on this trait and not on a client of their own for the reason
/// [`Transport::revoke_device`] is: the driver already holds exactly one
/// transport, already holds the device binding that signs every one of these
/// routes, and is already the only thing in the client that knows whether the
/// relay is reachable. A second HTTP client beside it would need its own copy
/// of the base URL, the bearer, the signer and the reachability state, and
/// would be a second thing to keep in step with all four.
///
/// They are *four* operations rather than one `upload_blob`, because the
/// sequencing is where the idempotency lives and that is a policy decision the
/// core takes and tests — see `sunrise_core::blob_sync`. A transport that ran
/// the whole two-phase commit itself would own the decision about what to do
/// with a half-finished upload, and would own it four times over once there is
/// a second transport.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one already-encoded frame.
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError>;

    /// Await one decoded frame. Returns `None` on graceful close.
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError>;

    /// Initiate graceful close.
    async fn close(&mut self) -> Result<(), TransportError>;

    /// Ask the relay to stop accepting `device_id`'s uploads and to end any
    /// session it is holding.
    ///
    /// `device_id` is the **vault-side** id — the one a `device_revoke` op
    /// names and the only one a revoking device holds. The relay's own id for
    /// a device is a ULID it mints at registration and never sends back
    /// through the op stream.
    ///
    /// This is out-of-band on purpose and is the only way it can be done. A
    /// `device_revoke` is an inner op sealed under the vault-meta Stream key,
    /// so the relay cannot read it; promoting the revoked id into the cleartext
    /// envelope header would tell the relay which of an account's devices had
    /// been revoked and when, for every account it serves, which is the
    /// metadata leak the blind-relay property exists to prevent.
    ///
    /// Defaulted to [`TransportError::Unsupported`] because the frame-only
    /// transports have no account API to call; `SseTransport` overrides it.
    async fn revoke_device(
        &mut self,
        _device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        Err(TransportError::Unsupported)
    }

    /// `POST /api/v1/blobs/init`: reserve an upload id for `chunk_count` chunks
    /// totalling `size_bytes` of ciphertext, under `stream_id`.
    ///
    /// The returned id is the **only** thing that keeps a retry from costing
    /// the relay a second pending directory, so a caller that stores it is
    /// expected to keep it and re-use it rather than calling this again.
    ///
    /// `size_bytes` is advisory — the relay checks the real total at finalize —
    /// but it is checked against the per-attachment ceiling here, so an
    /// oversized attachment is refused before a byte is uploaded.
    async fn blob_init(
        &mut self,
        _stream_id: &[u8; 16],
        _chunk_count: u32,
        _size_bytes: u64,
    ) -> Result<String, TransportError> {
        Err(TransportError::Unsupported)
    }

    /// `PUT /api/v1/blobs/{upload_id}/{chunk_idx}`: store one chunk of opaque
    /// ciphertext under a reserved upload.
    ///
    /// Idempotent at the relay: the chunk is written at a path derived from
    /// the upload id and the index, so re-sending one overwrites it in place.
    async fn blob_put_chunk(
        &mut self,
        _upload_id: &str,
        _chunk_idx: u32,
        _bytes: &[u8],
    ) -> Result<(), TransportError> {
        Err(TransportError::Unsupported)
    }

    /// `POST /api/v1/blobs/finalize`: check every stored chunk and commit.
    ///
    /// `chunk_hashes` are BLAKE3 over each sealed chunk, in order;
    /// `ciphertext_hash` is BLAKE3 over their concatenation. The relay re-hashes
    /// what it actually holds and refuses the commit on any disagreement, so
    /// these are a claim being checked rather than a claim being trusted.
    async fn blob_finalize(
        &mut self,
        _upload_id: &str,
        _ciphertext_hash: &[u8; 32],
        _chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        Err(TransportError::Unsupported)
    }

    /// `GET /api/v1/blobs/{blob_id}`: the committed chunks, concatenated.
    ///
    /// `Ok(None)` for a blob the relay does not hold — which covers "never
    /// uploaded", "uploaded by a device that has not finished yet" and "not
    /// this account's blob" alike, because the relay answers all three with one
    /// 404 so that the route cannot be used to probe for another account's
    /// ciphertext.
    ///
    /// The body carries no framing between chunks;
    /// `sunrise_crypto::split_sealed` recovers the boundaries.
    async fn blob_fetch(&mut self, _blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        Err(TransportError::Unsupported)
    }
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

    /// The frame-only defaults are a documented contract, not a placeholder.
    ///
    /// `LoopbackEnd` overrides `send_frame`, `recv_frame` and `close` and
    /// nothing else, which is exactly the shape the trait's defaults exist
    /// for. Each account-API method must answer [`TransportError::Unsupported`]
    /// so its caller can leave the request queued for a transport that has one
    /// — a default that silently returned `Ok` would erase the distinction
    /// between "this transport cannot" and "this attempt failed", which is the
    /// whole reason that variant is spelled out rather than folded into
    /// `Unavailable`.
    ///
    /// `revoke_device` and `blob_finalize` are here for completeness: their
    /// return types have no `Default`, so a mutant cannot rewrite them to `Ok`
    /// and the type system already holds those two. The other three are not
    /// held by anything else.
    #[tokio::test]
    async fn a_frame_only_transport_supports_no_account_api() {
        let mut t = LoopbackEnd {
            outbound: Arc::new(Mutex::new(Vec::new())),
            inbound: Arc::new(Mutex::new(Vec::new())),
            closed: false,
        };
        assert!(
            matches!(
                t.revoke_device([0u8; 16]).await,
                Err(TransportError::Unsupported)
            ),
            "a loopback has no account API to revoke against"
        );
        assert!(
            matches!(
                t.blob_init(&[0u8; 16], 1, 1).await,
                Err(TransportError::Unsupported)
            ),
            "an upload id a loopback minted would name a reservation no relay holds"
        );
        assert!(
            matches!(
                t.blob_put_chunk("upload", 0, &[0u8]).await,
                Err(TransportError::Unsupported)
            ),
            "a chunk a loopback accepted would be a chunk nothing stored"
        );
        assert!(
            matches!(
                t.blob_finalize("upload", &[0u8; 32], &[[0u8; 32]]).await,
                Err(TransportError::Unsupported)
            ),
            "a commit a loopback confirmed would be a commit no relay made"
        );
        assert!(
            matches!(
                t.blob_fetch(&[0u8; 16]).await,
                Err(TransportError::Unsupported)
            ),
            "`Ok(None)` here would read as 'the relay does not hold it' about a relay that was never asked"
        );
    }
}
