//! The blob surface: attachment upload and fetch, and the snapshot pin that
//! names one. `session` holds the trait methods these back.
//!
//! Each route goes through the generated relay client (`rest`), under the
//! bounds `rest` states for it. A committed blob's size and chunk count, and a
//! snapshot pin's size, are the description's required fields: a reply
//! missing one does not decode, where the hand-written reader took it as zero.

use super::rest::{bind, settle, Settled, BLOB_FETCH, CHUNK_UPLOAD, DOCUMENT_CALL};
use super::SseTransport;
use crate::transport::{BlobCommit, SnapshotPin, TransportError};
use sunrise_relay_client::api::{self, types};

impl SseTransport {
    pub(super) async fn init_blob(
        &self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        let body = types::InitRequest {
            stream_id: hex::encode(stream_id),
            chunk_count: chunk_count.into(),
            size_bytes: wire_int(size_bytes)?,
        };
        let binding = self.sign_json("POST", "/api/v1/blobs/init", Some(&body))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .init_blob_upload(bind!(api::InitBlobUploadParams::default(), binding), &body)
            .await;
        match settle(&exchange, result)? {
            Settled::Value(init) => Ok(init.upload_id),
            Settled::Refused(reply) => Err(self.refuse(&reply)),
        }
    }

    pub(super) async fn put_blob_chunk(
        &self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let path = format!("/api/v1/blobs/{upload_id}/{chunk_idx}");
        let binding = self.sign_bytes("PUT", &path, bytes);
        // The head waits on the chunk arriving at the relay, so it gets the
        // upload's own patience; the reply is an ack, read as a document.
        let exchange = self.exchange(CHUNK_UPLOAD)?;
        let result = exchange
            .client()
            .put_blob_chunk(
                upload_id,
                chunk_idx.into(),
                bind!(api::PutBlobChunkParams::default(), binding),
                &hyper::body::Bytes::copy_from_slice(bytes),
            )
            .await;
        match settle(&exchange, result)? {
            Settled::Value(()) => Ok(()),
            Settled::Refused(reply) => Err(self.refuse(&reply)),
        }
    }

    pub(super) async fn finalize_blob(
        &self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        let body = types::FinalizeRequest {
            upload_id: upload_id.to_owned(),
            content_hash: hex::encode(ciphertext_hash),
            chunk_hashes: chunk_hashes.iter().map(hex::encode).collect(),
        };
        let binding = self.sign_json("POST", "/api/v1/blobs/finalize", Some(&body))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .finalize_blob_upload(
                bind!(api::FinalizeBlobUploadParams::default(), binding),
                &body,
            )
            .await;
        let commit = match settle(&exchange, result)? {
            Settled::Value(commit) => commit,
            Settled::Refused(reply) => return Err(self.refuse(&reply)),
        };
        Ok(BlobCommit {
            blob_id: blob_id_of(&commit.blob_id).ok_or_else(|| {
                TransportError::Protocol("blobs/finalize returned no blob id".to_owned())
            })?,
            size_bytes: reply_int(commit.size_bytes, "blobs/finalize size_bytes")?,
            chunk_count: reply_int(commit.chunk_count, "blobs/finalize chunk_count")?,
        })
    }

    pub(super) async fn fetch_blob(
        &self,
        blob_id: &[u8; 16],
    ) -> Result<Option<Vec<u8>>, TransportError> {
        let id = format!("blb_{}", hex::encode(blob_id));
        let binding = self.sign_bytes("GET", &format!("/api/v1/blobs/{id}"), &[]);
        let exchange = self.exchange(BLOB_FETCH)?;
        let result = exchange
            .client()
            .fetch_blob(id, bind!(api::FetchBlobParams::default(), binding))
            .await;
        let reply = match settle(&exchange, result)? {
            // `Vec::from` takes the buffer over when this `Bytes` is its sole
            // owner, which a freshly fetched body is; `to_vec` would copy the
            // whole attachment while the original was still alive.
            Settled::Value(bytes) => return Ok(Some(Vec::from(bytes))),
            Settled::Refused(reply) => reply,
        };
        // The relay answers "no such blob", "not yours" and "not finished yet"
        // with one 404, deliberately — a distinguishable response would make
        // this route an oracle for whether another account holds a given
        // ciphertext. All three mean the same thing to a fetching device: the
        // bytes are not here, try later.
        if reply.status.as_u16() == 404 {
            return Ok(None);
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn latest_snapshot(
        &self,
        stream_id: &[u8; 16],
    ) -> Result<Option<SnapshotPin>, TransportError> {
        let stream = hex::encode(stream_id);
        let binding = self.sign_empty("GET", &format!("/api/v1/snapshots/{stream}"))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .fetch_stream_snapshot(
                stream,
                bind!(api::FetchStreamSnapshotParams::default(), binding),
            )
            .await;
        let reply = match settle(&exchange, result)? {
            Settled::Value(pin) => return snapshot_pin_of(&pin).map(Some),
            Settled::Refused(reply) => reply,
        };
        // No pin, and an account the relay does not know, are one 404.
        if reply.status.as_u16() == 404 {
            return Ok(None);
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn pin_snapshot(
        &self,
        stream_id: &[u8; 16],
        blob_id: &[u8; 16],
    ) -> Result<SnapshotPin, TransportError> {
        let stream = hex::encode(stream_id);
        let body = types::PinSnapshotRequest {
            blob_id: format!("blb_{}", hex::encode(blob_id)),
        };
        let binding = self.sign_json("PUT", &format!("/api/v1/snapshots/{stream}"), Some(&body))?;
        let exchange = self.exchange(DOCUMENT_CALL)?;
        let result = exchange
            .client()
            .pin_stream_snapshot(
                stream,
                bind!(api::PinStreamSnapshotParams::default(), binding),
                &body,
            )
            .await;
        match settle(&exchange, result)? {
            Settled::Value(pin) => snapshot_pin_of(&pin),
            Settled::Refused(reply) => Err(self.refuse(&reply)),
        }
    }
}

/// The sixteen bytes a `blb_` + 32-hex blob id names, or `None` for anything
/// else.
pub(super) fn blob_id_of(id: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    hex::decode_to_slice(id.strip_prefix("blb_")?, &mut out).ok()?;
    Some(out)
}

/// A `StreamSnapshot`'s blob id and size.
fn snapshot_pin_of(pin: &types::StreamSnapshot) -> Result<SnapshotPin, TransportError> {
    Ok(SnapshotPin {
        blob_id: blob_id_of(&pin.blob_id)
            .ok_or_else(|| TransportError::Protocol("a snapshot pin names no blob".to_owned()))?,
        size_bytes: reply_int(pin.size_bytes, "a snapshot pin's size_bytes")?,
    })
}

/// An unsigned count in the generated model's integer type.
///
/// spargen models every integer as `i64`, `format: uint64` included, so a
/// `u64` past `i64::MAX` has no wire form here. No blob is that large: the
/// relay refuses one past `MAX_BLOB_BYTES` at `init`.
fn wire_int(n: u64) -> Result<i64, TransportError> {
    i64::try_from(n).map_err(|_| TransportError::Protocol(format!("{n} does not fit the wire")))
}

/// A count the relay sent, in the type this side holds it in.
///
/// A negative or oversized value is a malformed reply rather than a number to
/// quietly default: the hand-written reader this replaced took a missing or
/// out-of-range count as zero, and a commit that claims zero bytes is a
/// different fact from one the relay did not state.
fn reply_int<T: TryFrom<i64>>(n: i64, what: &str) -> Result<T, TransportError> {
    T::try_from(n).map_err(|_| TransportError::Protocol(format!("{what} is out of range: {n}")))
}
