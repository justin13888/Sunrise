//! `GET /api/v1/blobs/{blob_id}`: the read, streamed one committed chunk at a
//! time.

use super::{account, not_found, BlobIdPath};
use crate::api::error::ApiError;
use crate::api::ids::parse_blob_id;
use crate::api::ratelimit::policy::Budget;
use crate::api::ratelimit::Throttled;
use crate::api::signed::SignedParts;
use crate::blob::{AccountKey, BlobBackend, Manifest};
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::params::path::Path;
use kynos::http::media::OctetStream;
use kynos::response::stream::binary::BinaryStream;
use std::sync::Arc;
use sunrise_telemetry::{Attr, Count};

/// Stream a committed blob's ciphertext back.
///
/// One chunk is read from disk at a time rather than the whole blob being
/// assembled first. The surface this replaces built a single `Vec<u8>` of up to
/// `MAX_BLOB_BYTES` before writing a byte, so one fetch of a large attachment
/// pinned 100 MB of the relay's memory for as long as the client took to read
/// it — and a handful of concurrent fetches was an availability problem rather
/// than a slow response.
#[kynos::get("/api/v1/blobs/{blob_id}", operation_id = "fetchBlob")]
pub async fn fetch(
    Inject(state): Inject<ServerState>,
    Path(path): Path<BlobIdPath>,
    SignedParts(caller): SignedParts,
) -> Result<BinaryStream<ChunkStream, OctetStream>, Throttled> {
    let blob = parse_blob_id(&path.blob_id.0)?;
    let account = account(&caller);
    let Manifest {
        chunk_count,
        size_bytes,
    } = state
        .blobs
        .manifest(account, blob)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(not_found)?;
    if !state
        .blobs
        .has_all(account, blob, chunk_count)
        .await
        .map_err(ApiError::from)?
    {
        return Err(not_found().into());
    }
    // The whole blob up front, from the manifest: once the status is sent
    // the body cannot be refused part-way, and a fetch abandoned early is a
    // client's to repeat.
    state
        .limiter
        .charge(
            &state,
            "/api/v1/blobs/{blob_id}",
            Budget::BlobDownload,
            &caller,
            size_bytes,
        )
        .await?;
    state.metrics.incr("sunrise_blob_fetch_total");
    // Opened here, under the request, and ended when the body is dropped —
    // after the request's own span, since the body outlives the handler.
    let read_span = sunrise_telemetry::span(
        "blob.chunk_read",
        [
            Attr::count(Count::Chunks, u64::from(chunk_count)),
            Attr::count(Count::Bytes, size_bytes),
        ],
    );
    Ok(BinaryStream::new(chunk_stream(
        Arc::clone(&state.blobs),
        account,
        blob,
        chunk_count,
        state.metrics.clone(),
        read_span,
    )))
}

/// The stream [`fetch`] returns: one chunk per poll, read on demand.
type ChunkStream = futures_util::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>;

/// Read `chunk_count` chunks of `blob` from `blobs`, one at a time.
///
/// A chunk that vanishes between the `has_all` check and its read ends the body
/// with an error rather than silently short-reading: the success status is
/// already committed by then, so truncating quietly would hand the client a
/// blob that fails its own content hash with nothing to say why.
///
/// Each chunk is counted in `sunrise_blob_bytes_total{direction="download"}`
/// as it is read, so a fetch abandoned part-way counts what was read for it
/// rather than the whole blob.
///
/// `span` lives as long as the stream, so the `blob.chunk_read` it holds ends
/// when the body is finished or abandoned, and is marked failed by a read that
/// fails.
fn chunk_stream(
    blobs: Arc<dyn BlobBackend>,
    account: AccountKey,
    blob: [u8; 16],
    chunk_count: u32,
    metrics: crate::Metrics,
    span: sunrise_telemetry::SpanGuard,
) -> ChunkStream {
    use futures_util::StreamExt as _;
    let span = Arc::new(span);
    futures_util::stream::unfold(0u32, move |idx| {
        let blobs = Arc::clone(&blobs);
        let metrics = metrics.clone();
        let span = Arc::clone(&span);
        async move {
            if idx >= chunk_count {
                return None;
            }
            let item = match blobs.get_committed(account, blob, idx).await {
                Ok(Some(bytes)) => {
                    metrics.add_with(
                        "sunrise_blob_bytes_total",
                        &[("direction", "download")],
                        bytes.len() as u64,
                    );
                    Ok(bytes::Bytes::from(bytes))
                }
                Ok(None) => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("chunk {idx} vanished mid-read"),
                )),
                Err(_) => Err(std::io::Error::other("chunk read failed")),
            };
            if item.is_err() {
                span.fail("chunk read failed");
            }
            Some((item, idx + 1))
        }
    })
    .boxed()
}
