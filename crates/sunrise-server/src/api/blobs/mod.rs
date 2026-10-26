//! Blob two-phase commit on the typed surface (`/api/v1/blobs/...`).
//!
//! Per `docs/06-server/api.md` §Blobs and `docs/02-domain/attachments.md`.
//! Three phases, a read, and a delete:
//!
//! 1. `POST /blobs/init` reserves an upload id and hands back one URL per chunk.
//! 2. `PUT /blobs/{upload_id}/{chunk_idx}` stores one chunk of **opaque
//!    ciphertext** under the caller's pending area.
//! 3. `POST /blobs/finalize` re-hashes every stored chunk with BLAKE3, checks
//!    each against the client's per-chunk hash and the concatenation against
//!    the client's `content_hash`, and only then commits under the content
//!    address.
//! 4. `GET /blobs/{blob_id}` streams the committed chunks back.
//! 5. `DELETE /blobs/{blob_id}` tombstones a committed blob; its ciphertext
//!    stays readable until maintenance collects it.
//!
//! Everything here is E2EE ciphertext. The server never holds a key and can
//! never read a byte of it; hashing is an *integrity* check, not a content
//! check, which is why the client sends the hashes it computed over the same
//! ciphertext.
//!
//! # The chunk body is the one that is not JSON
//!
//! A chunk `PUT` carries raw ciphertext, so there is no JSON value to
//! canonicalize. ADR-0022 generalises rather than adding a second scheme: what
//! is signed is the body's canonical form, and for binary the bytes already are
//! it. [`SignedBinary`] is that path, and the signature therefore covers those
//! exact bytes — the property the blob store's own content hashing asserts from
//! the other direction.
//!
//! # Isolation
//!
//! Every call into [`crate::blob::BlobBackend`] is keyed by the account's
//! BLAKE3 hash, so both the pending area and the committed store are
//! per-account. Content addressing across accounts would be a cross-tenant
//! read primitive: one account could name another's blob by its hash.
//! Per-account keys make that impossible by construction rather than by a
//! check someone can forget, and dedup still works where it should — across
//! one account's devices.
//!
//! # Where each step lives
//!
//! The upload — `init`, `put_chunk`, `finalize` and what `finalize` settles —
//! is here, with the bounds and storage keys every step shares. The read is
//! [`fetch`] and the tombstone is [`delete`].

pub mod delete;
pub mod fetch;
#[cfg(test)]
mod tests;

pub use delete::{BlobDeleteRequest, BlobDeleteResponse};

use crate::api::account_deletion::refuse_if_pending;
use crate::api::error::{codes, ApiError};
use crate::api::ids::{parse_hash, parse_upload_id};
use crate::api::ids::{BlobId, ChunkHash, UploadId};
use crate::api::ratelimit::policy::Budget;
use crate::api::ratelimit::Throttled;
use crate::api::signed::{Caller, Signed, SignedBinary};
use crate::blob::{AccountKey, Manifest};
use crate::state::ServerState;
use crate::store::CommittedBlob;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::path::Path;
use kynos::response::status::NoContent;
use serde::{Deserialize, Serialize};
use sunrise_telemetry::{Attr, Count};

/// Largest single chunk the relay accepts, per
/// `docs/02-domain/attachments.md` §Lazy fetch ("chunks are 1 MiB each").
const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// Most chunks one blob may have. 4096 x 1 MiB is 4 GiB of addressable space,
/// far above the size policy below, and it bounds the work `finalize` does.
const MAX_CHUNK_COUNT: u32 = 4096;

/// Hard per-attachment ceiling from `docs/02-domain/attachments.md` §Size
/// policy: 100 MB on managed cloud, configurable on self-host.
const MAX_BLOB_BYTES: u64 = 100 * 1000 * 1000;

/// `POST /api/v1/blobs/init` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct InitRequest {
    /// Stream id this blob belongs to.
    pub stream_id: String,
    /// Total chunk count.
    pub chunk_count: u32,
    /// Total ciphertext size in bytes. Advisory: the server enforces the
    /// per-attachment ceiling here and the real size at finalize.
    pub size_bytes: u64,
}

/// `POST /api/v1/blobs/init` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct InitResponse {
    /// Server-assigned upload id, used by the chunk PUTs and by `finalize`:
    /// `up_` + 32 lowercase hex characters.
    #[schema(pattern = "^up_[0-9a-f]{32}$")]
    pub upload_id: String,
    /// Per-chunk upload URLs. Self-host returns relative paths under
    /// `/api/v1/blobs/<upload_id>/<idx>`, which the PUT operation serves.
    pub chunk_urls: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct FinalizeRequest {
    /// Upload id from `init`: `up_` + 32 lowercase hex characters.
    #[schema(pattern = "^up_[0-9a-f]{32}$")]
    pub upload_id: String,
    /// BLAKE3 of the concatenated chunk ciphertext, 64 lowercase hex.
    #[schema(pattern = "^[0-9a-f]{64}$")]
    pub content_hash: String,
    /// Per-chunk BLAKE3 hashes, 64 lowercase hex each, in order. Its length is
    /// the chunk count: `init`'s advisory count is not trusted here.
    pub chunk_hashes: Vec<ChunkHash>,
}

/// `POST /api/v1/blobs/finalize` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct FinalizeResponse {
    /// Canonical blob id: `blb_` + the first 16 bytes of `content_hash` in
    /// lowercase hex. Content-addressed, so re-uploading identical ciphertext
    /// converges on the same id.
    #[schema(pattern = "^blb_[0-9a-f]{32}$")]
    pub blob_id: String,
    /// Committed size in bytes.
    pub size_bytes: u64,
    /// Committed chunk count.
    pub chunk_count: u32,
}

/// `{upload_id}/{chunk_idx}` in a chunk upload path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct ChunkPath {
    /// The upload this chunk belongs to.
    pub upload_id: UploadId,
    /// Zero-based index within the upload.
    pub chunk_idx: u32,
}

/// `{blob_id}` in a blob fetch path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct BlobIdPath {
    /// The blob being read.
    pub blob_id: BlobId,
}

/// Reserve an upload id and hand back one URL per chunk.
#[kynos::post("/api/v1/blobs/init", operation_id = "initBlobUpload")]
pub async fn init(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<InitRequest>,
) -> Result<Json<InitResponse>, Throttled> {
    if body.chunk_count == 0 || body.chunk_count > MAX_CHUNK_COUNT {
        return Err(
            ApiError::validation(format!("chunk_count must be 1..={MAX_CHUNK_COUNT}")).into(),
        );
    }
    if body.size_bytes > MAX_BLOB_BYTES {
        return Err(ApiError::validation(format!(
            "size_bytes exceeds the {MAX_BLOB_BYTES}-byte per-attachment limit"
        ))
        .into());
    }
    if body.stream_id.trim().is_empty() {
        return Err(ApiError::validation("stream_id required").into());
    }
    refuse_if_pending(&state, &caller).await?;

    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).map_err(|_| ApiError::internal())?;
    // Counted until `finalize`, or until it goes an hour untouched.
    state
        .limiter
        .open_upload(&state, "/api/v1/blobs/init", &caller, raw)?;
    let upload_id = format!("up_{}", hex::encode(raw));
    // Create the pending area eagerly so a chunk PUT is a write, not a
    // mkdir-then-write race between two concurrent chunk uploads. The open
    // time is kept with it, because `finalize` measures the upload from here
    // and the limiter's ledger only exists with `[limits]` on.
    state
        .blobs
        .open_upload(account(&caller), raw, state.clock.now_ms())
        .await
        .map_err(ApiError::from)?;
    let chunk_urls = (0..body.chunk_count)
        .map(|i| format!("/api/v1/blobs/{upload_id}/{i}"))
        .collect();
    state.metrics.incr("sunrise_blob_init_total");
    Ok(Json(InitResponse {
        upload_id,
        chunk_urls,
    }))
}

/// Store one chunk of opaque ciphertext.
#[kynos::put("/api/v1/blobs/{upload_id}/{chunk_idx}", operation_id = "putBlobChunk")]
pub async fn put_chunk(
    Inject(state): Inject<ServerState>,
    Path(path): Path<ChunkPath>,
    SignedBinary { caller, bytes }: SignedBinary,
) -> Result<NoContent, Throttled> {
    let upload = parse_upload_id(&path.upload_id.0)?;
    if path.chunk_idx >= MAX_CHUNK_COUNT {
        return Err(
            ApiError::validation(format!("chunk index must be < {MAX_CHUNK_COUNT}")).into(),
        );
    }
    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
        return Err(
            ApiError::validation(format!("chunk must be 1..={MAX_CHUNK_BYTES} bytes")).into(),
        );
    }
    refuse_if_pending(&state, &caller).await?;
    state
        .limiter
        .charge(
            &state,
            "/api/v1/blobs/{upload_id}/{chunk_idx}",
            Budget::BlobUpload,
            &caller,
            bytes.len() as u64,
        )
        .await?;
    state.limiter.touch_upload(&state, &caller, upload);
    let write_span = sunrise_telemetry::span(
        "blob.chunk_write",
        [
            Attr::count(Count::Chunks, 1),
            Attr::count(Count::Bytes, bytes.len() as u64),
        ],
    );
    state
        .blobs
        .put_pending(account(&caller), upload, path.chunk_idx, &bytes)
        .await
        .map_err(|e| {
            write_span.fail("chunk write failed");
            ApiError::from(e)
        })?;
    drop(write_span);
    state.metrics.incr("sunrise_blob_chunk_total");
    state.metrics.add_with(
        "sunrise_blob_bytes_total",
        &[("direction", "upload")],
        bytes.len() as u64,
    );
    Ok(NoContent)
}

/// Check every stored chunk and commit the blob under its content address.
///
/// # Memory
///
/// Every chunk is resident at once. The verification loop reads each chunk
/// back, hashes it, and keeps it so the commit that follows does not have to
/// read it a second time; the `MAX_BLOB_BYTES` check runs *after* that loop,
/// on a total the loop has already accumulated in full. So the 100 MB
/// per-attachment ceiling does not bound what this holds — `MAX_CHUNK_COUNT` x
/// `MAX_CHUNK_BYTES`, 4096 x 1 MiB, does, and the peak before the ceiling can
/// fire is about 4 GiB.
///
/// `fetch` streams one chunk at a time, and its doc records why: a single
/// buffered blob pinned 100 MB of the relay's memory and concurrent fetches
/// made that an availability problem. This path has the same shape and a
/// larger bound. It is stated rather than fixed because checking the size
/// inside the loop changes which requests this endpoint accepts, which is a
/// decision to take deliberately and not a tidy-up.
#[kynos::post("/api/v1/blobs/finalize", operation_id = "finalizeBlobUpload")]
pub async fn finalize(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<FinalizeRequest>,
) -> Result<Json<FinalizeResponse>, ApiError> {
    let upload = parse_upload_id(&body.upload_id)?;
    let content_hash = parse_hash(&body.content_hash, "content_hash")?;
    if body.chunk_hashes.is_empty() || body.chunk_hashes.len() > MAX_CHUNK_COUNT as usize {
        return Err(ApiError::validation(format!(
            "chunk_hashes must hold 1..={MAX_CHUNK_COUNT} entries"
        )));
    }
    let expected: Vec<[u8; 32]> = body
        .chunk_hashes
        .iter()
        .map(|h| parse_hash(&h.0, "chunk_hashes"))
        .collect::<Result<_, _>>()?;
    refuse_if_pending(&state, &caller).await?;

    let account = account(&caller);
    // Re-hash what is actually on disk. The client's hashes are a claim; this
    // is the check. A chunk that never arrived, arrived truncated, or arrived
    // corrupted all fail here rather than becoming an unreadable attachment
    // that only surfaces months later on another device.
    let mut hasher = blake3::Hasher::new();
    let mut chunks = Vec::with_capacity(expected.len());
    let mut size_bytes: u64 = 0;
    let read_span = sunrise_telemetry::span(
        "blob.chunk_read",
        [Attr::count(Count::Chunks, expected.len() as u64)],
    );
    for (idx, want) in expected.iter().enumerate() {
        let idx = u32::try_from(idx).map_err(|_| ApiError::internal())?;
        let bytes = state
            .blobs
            .get_pending(account, upload, idx)
            .await?
            .ok_or_else(|| {
                ApiError::conflict(
                    codes::BLOB_CHUNK_MISSING,
                    format!("chunk {idx} was never uploaded"),
                )
            })?;
        if blake3::hash(&bytes).as_bytes() != want {
            state.metrics.incr("sunrise_blob_hash_mismatch_total");
            return Err(hash_mismatch(format!("chunk {idx} hash mismatch")));
        }
        hasher.update(&bytes);
        size_bytes = size_bytes.saturating_add(bytes.len() as u64);
        chunks.push(bytes);
    }
    read_span.set(Attr::count(Count::Bytes, size_bytes));
    drop(read_span);
    if hasher.finalize().as_bytes() != &content_hash {
        state.metrics.incr("sunrise_blob_hash_mismatch_total");
        return Err(hash_mismatch("content_hash mismatch"));
    }
    if size_bytes > MAX_BLOB_BYTES {
        return Err(ApiError::validation(format!(
            "blob exceeds the {MAX_BLOB_BYTES}-byte per-attachment limit"
        )));
    }

    // Commit under the content address. Written chunk-by-chunk, then the
    // manifest LAST: a reader that finds no manifest sees no blob, so a crash
    // mid-commit leaves an invisible partial rather than a short read.
    let blob = blob_key(&content_hash);
    let write_span = sunrise_telemetry::span(
        "blob.chunk_write",
        [
            Attr::count(Count::Chunks, chunks.len() as u64),
            Attr::count(Count::Bytes, size_bytes),
        ],
    );
    for (idx, bytes) in chunks.iter().enumerate() {
        let idx = u32::try_from(idx).map_err(|_| ApiError::internal())?;
        state
            .blobs
            .put_committed(account, blob, idx, bytes)
            .await
            .map_err(|e| {
                write_span.fail("chunk write failed");
                ApiError::from(e)
            })?;
    }
    drop(write_span);
    let chunk_count = u32::try_from(chunks.len()).map_err(|_| ApiError::internal())?;
    state
        .blobs
        .write_manifest(
            account,
            blob,
            Manifest {
                chunk_count,
                size_bytes,
            },
        )
        .await?;
    settle(&state, &caller, account, upload, blob, size_bytes).await?;
    Ok(Json(FinalizeResponse {
        blob_id: format!("blb_{}", hex::encode(blob)),
        size_bytes,
        chunk_count,
    }))
}

/// What `finalize` owes once the manifest has made `blob` readable: the
/// ledger row and the tombstone, the upload's pending area and ledger entry,
/// and the upload's metrics.
async fn settle(
    state: &ServerState,
    caller: &Caller,
    account: AccountKey,
    upload: [u8; 16],
    blob: [u8; 16],
    size_bytes: u64,
) -> Result<(), ApiError> {
    let account_id = &caller.principal.account.account_id;
    // Readable now, so counted in the bytes at rest. A failure here leaves a
    // readable blob the ledger does not know, which the maintenance pass
    // backfills from its manifest.
    state
        .store
        .record_blob(&CommittedBlob {
            account_id: account_id.clone(),
            blob_key: blob,
            size_bytes,
        })
        .await?;
    // The same ciphertext uploaded again is the attachment coming back — a
    // detach undone, or the same file attached elsewhere — so any tombstone
    // on its content address no longer describes it.
    state.store.clear_tombstone(account_id, &blob).await?;
    // Read before the pending area, which holds it, is discarded. A reading
    // that is not there is no sample, never a failed finalize.
    let opened_at_ms = state
        .blobs
        .upload_opened_at(account, upload)
        .await
        .ok()
        .flatten();
    // Best effort: a leftover pending area costs storage, never correctness,
    // and the maintenance pass sweeps whatever this leaves. The whole upload,
    // not only its chunks.
    let _ = state.blobs.discard_upload(account, upload).await;
    state.limiter.close_upload(caller, upload);

    state.metrics.incr("sunrise_blob_finalize_total");
    if let Some(opened_at_ms) = opened_at_ms {
        #[allow(clippy::cast_precision_loss)]
        let seconds = state.clock.now_ms().saturating_sub(opened_at_ms) as f64 / 1000.0;
        state.metrics.observe(
            "sunrise_blob_upload_duration_seconds",
            &[],
            crate::metrics::TRANSFER_BUCKETS,
            seconds,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// The key every blob of the caller's account is filed under: the first 16
/// bytes of `BLAKE3(account_id)`, the same key the relay log files the
/// account's frames under. A hash rather than the id, so no backend ever holds
/// an account id in a path or an object key.
///
/// An upload is likewise named by its **parsed bytes**, never by the URL
/// segment they were parsed out of: `up_AB…` and `up_ab…` would otherwise be
/// one upload with two spellings, and a backend that stored them apart would
/// hide chunks PUT under one from a `finalize` using the other.
pub(crate) fn account(caller: &Caller) -> AccountKey {
    crate::account_ns::account_key(&caller.principal.account.account_id)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The blob's 16-byte storage key: the first half of its BLAKE3 content hash.
/// Content-addressed, so identical ciphertext converges on one stored copy.
const fn blob_key(content_hash: &[u8; 32]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        out[i] = content_hash[i];
        i += 1;
    }
    out
}

/// `400 BLOB_HASH_MISMATCH`, per `docs/06-server/api.md` §Blob errors: the
/// upload is well-formed but does not hash to what it claims.
fn hash_mismatch(message: impl Into<String>) -> ApiError {
    ApiError::validation_coded(codes::BLOB_HASH_MISMATCH, message)
}

/// One 404 for "no such blob" and for "not this account's blob" alike: a
/// distinguishable response would turn `GET /blobs/<hash>` into an oracle for
/// whether another account holds that ciphertext.
pub(crate) fn not_found() -> ApiError {
    ApiError::not_found(codes::BLOB_NOT_FOUND, "blob not found")
}
