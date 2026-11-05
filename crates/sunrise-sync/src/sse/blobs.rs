//! The blob surface: attachment upload and fetch, and the snapshot pin that
//! names one. `session` holds the trait methods these back.

use super::http::{
    field_u64, protocol, BLOB_IDLE_TIMEOUT, BODY_IDLE_TIMEOUT, CHUNK_HEAD_TIMEOUT, HEAD_TIMEOUT,
    MAX_BLOB_BYTES, MAX_EVENT_BYTES,
};
use super::SseTransport;
use crate::transport::{BlobCommit, SnapshotPin, TransportError};

impl SseTransport {
    pub(super) async fn init_blob(
        &self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        let reply = self
            .call(
                "POST",
                "/api/v1/blobs/init",
                Some(serde_json::json!({
                    "stream_id": hex::encode(stream_id),
                    "chunk_count": chunk_count,
                    "size_bytes": size_bytes,
                })),
            )
            .await?;
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        serde_json::from_slice::<serde_json::Value>(&reply.bytes)
            .ok()
            .and_then(|v| {
                v.get("upload_id")
                    .and_then(|u| u.as_str())
                    .map(ToOwned::to_owned)
            })
            .ok_or_else(|| TransportError::Protocol("blobs/init returned no upload_id".to_owned()))
    }

    pub(super) async fn put_blob_chunk(
        &self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let reply = self
            .call_bytes(
                "PUT",
                &format!("/api/v1/blobs/{upload_id}/{chunk_idx}"),
                Some("application/octet-stream"),
                bytes,
                // The head waits on the chunk arriving at the relay, so it
                // gets the upload's own patience rather than a document's.
                CHUNK_HEAD_TIMEOUT,
                // The *request* carries a chunk; the *reply* is an ack, and
                // these bounds are the reply's. An ack is an empty `204` or a
                // short JSON document, so it is bounded exactly as every other
                // document reply is — by [`MAX_EVENT_BYTES`], which denies a
                // relay the choice of how much memory this client holds, and
                // by [`BODY_IDLE_TIMEOUT`], because a relay that has already
                // sent the head of an ack and then goes quiet has stalled. A
                // refused `PUT` lands on the same two numbers, by a different
                // route: `call_bytes` reads every non-2xx body under the
                // document bounds regardless of what it was passed.
                MAX_EVENT_BYTES,
                BODY_IDLE_TIMEOUT,
            )
            .await?;
        if reply.status.is_success() {
            return Ok(());
        }
        Err(self.refuse(&reply))
    }

    pub(super) async fn finalize_blob(
        &self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        let hashes: Vec<String> = chunk_hashes.iter().map(hex::encode).collect();
        let reply = self
            .call(
                "POST",
                "/api/v1/blobs/finalize",
                Some(serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex::encode(ciphertext_hash),
                    "chunk_hashes": hashes,
                })),
            )
            .await?;
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        let body: serde_json::Value =
            serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
        let blob_id = body
            .get("blob_id")
            .and_then(|b| b.as_str())
            .and_then(|s| s.strip_prefix("blb_"))
            .and_then(|hexpart| {
                let mut out = [0u8; 16];
                hex::decode_to_slice(hexpart, &mut out).ok().map(|()| out)
            })
            .ok_or_else(|| {
                TransportError::Protocol("blobs/finalize returned no blob id".to_owned())
            })?;
        Ok(BlobCommit {
            blob_id,
            size_bytes: body
                .get("size_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            chunk_count: body
                .get("chunk_count")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
        })
    }

    pub(super) async fn fetch_blob(
        &self,
        blob_id: &[u8; 16],
    ) -> Result<Option<Vec<u8>>, TransportError> {
        let reply = self
            .call_bytes(
                "GET",
                &format!("/api/v1/blobs/blb_{}", hex::encode(blob_id)),
                None,
                &[],
                // A bodiless `GET`: its head is a document like any other.
                HEAD_TIMEOUT,
                // The bounds on a *fetched attachment*, which is the one
                // reply on this transport that is content rather than a
                // document: [`MAX_BLOB_BYTES`] is the relay's own ceiling, and
                // [`BLOB_IDLE_TIMEOUT`] is what keeps an ordinary gap in a
                // hundred-megabyte transfer from being read as a dead peer.
                // They apply to a 2xx and nothing else. The `404` below and
                // whatever `refuse` parses are problem documents, and
                // `call_bytes` reads them under the document bounds — sizing
                // them from here would let a relay answer a fetch `404` with a
                // hundred megabytes, and be waited on for an attachment's
                // patience per frame while it did.
                MAX_BLOB_BYTES,
                BLOB_IDLE_TIMEOUT,
            )
            .await?;
        if reply.status.is_success() {
            return Ok(Some(reply.bytes));
        }
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
        let reply = self
            .call(
                "GET",
                &format!("/api/v1/snapshots/{}", hex::encode(stream_id)),
                None,
            )
            .await?;
        // No pin, and an account the relay does not know, are one 404.
        if reply.status.as_u16() == 404 {
            return Ok(None);
        }
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        snapshot_pin_of(&reply.bytes).map(Some)
    }

    pub(super) async fn pin_snapshot(
        &self,
        stream_id: &[u8; 16],
        blob_id: &[u8; 16],
    ) -> Result<SnapshotPin, TransportError> {
        let reply = self
            .call(
                "PUT",
                &format!("/api/v1/snapshots/{}", hex::encode(stream_id)),
                Some(serde_json::json!({ "blob_id": format!("blb_{}", hex::encode(blob_id)) })),
            )
            .await?;
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        snapshot_pin_of(&reply.bytes)
    }
}

/// A `StreamSnapshot` document's blob id and size.
fn snapshot_pin_of(bytes: &[u8]) -> Result<SnapshotPin, TransportError> {
    let body: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| protocol(&e))?;
    let blob_id = body
        .get("blob_id")
        .and_then(|b| b.as_str())
        .and_then(|s| s.strip_prefix("blb_"))
        .and_then(|hexpart| {
            let mut out = [0u8; 16];
            hex::decode_to_slice(hexpart, &mut out).ok().map(|()| out)
        })
        .ok_or_else(|| TransportError::Protocol("a snapshot pin names no blob".to_owned()))?;
    Ok(SnapshotPin {
        blob_id,
        size_bytes: field_u64(&body, "size_bytes"),
    })
}
