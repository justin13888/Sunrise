//! Blob two-phase commit on the typed surface (`/api/v1/blobs/...`).
//!
//! Per `docs/06-server/api.md` §Blobs and `docs/02-domain/attachments.md`.
//! Three phases and a read:
//!
//! 1. `POST /blobs/init` reserves an upload id and hands back one URL per chunk.
//! 2. `PUT /blobs/{upload_id}/{chunk_idx}` stores one chunk of **opaque
//!    ciphertext** under the caller's pending area.
//! 3. `POST /blobs/finalize` re-hashes every stored chunk with BLAKE3, checks
//!    each against the client's per-chunk hash and the concatenation against
//!    the client's `content_hash`, and only then commits under the content
//!    address.
//! 4. `GET /blobs/{blob_id}` streams the committed chunks back.
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
//! Both the pending area and the committed store are rooted at a per-account
//! directory keyed by a BLAKE3 of the account id. Content addressing across
//! accounts would be a cross-tenant read primitive: one account could name
//! another's blob by its hash. Per-account roots make that impossible by
//! construction rather than by a check someone can forget, and dedup still
//! works where it should — across one account's devices.

use crate::api::error::{codes, ApiError};
use crate::api::signed::{Caller, Signed, SignedBinary, SignedParts};
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::media::OctetStream;
use kynos::extract::params::path::Path;
use kynos::response::status::NoContent;
use kynos::response::stream::binary::BinaryStream;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sunrise_storage::BlobStore;

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
    /// Server-assigned upload id, used by the chunk PUTs and by `finalize`.
    pub upload_id: String,
    /// Per-chunk upload URLs. Self-host returns relative paths under
    /// `/api/v1/blobs/<upload_id>/<idx>`, which the PUT operation serves.
    pub chunk_urls: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct FinalizeRequest {
    /// Upload id from `init`.
    pub upload_id: String,
    /// BLAKE3 of the concatenated chunk ciphertext, 64 lowercase hex.
    pub content_hash: String,
    /// Per-chunk BLAKE3 hashes, 64 lowercase hex each, in order. Its length is
    /// the chunk count: `init`'s advisory count is not trusted here.
    pub chunk_hashes: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct FinalizeResponse {
    /// Canonical blob id: `blb_` + the first 16 bytes of `content_hash` in
    /// lowercase hex. Content-addressed, so re-uploading identical ciphertext
    /// converges on the same id.
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
    pub upload_id: String,
    /// Zero-based index within the upload.
    pub chunk_idx: u32,
}

/// `{blob_id}` in a blob fetch path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct BlobIdPath {
    /// The blob being read.
    pub blob_id: String,
}

/// Reserve an upload id and hand back one URL per chunk.
#[kynos::post("/api/v1/blobs/init", operation_id = "initBlobUpload")]
pub async fn init(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<InitRequest>,
) -> Result<Json<InitResponse>, ApiError> {
    if body.chunk_count == 0 || body.chunk_count > MAX_CHUNK_COUNT {
        return Err(ApiError::validation(format!(
            "chunk_count must be 1..={MAX_CHUNK_COUNT}"
        )));
    }
    if body.size_bytes > MAX_BLOB_BYTES {
        return Err(ApiError::validation(format!(
            "size_bytes exceeds the {MAX_BLOB_BYTES}-byte per-attachment limit"
        )));
    }
    if body.stream_id.trim().is_empty() {
        return Err(ApiError::validation("stream_id required"));
    }

    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).map_err(|_| ApiError::internal())?;
    let upload_id = format!("up_{}", hex::encode(raw));
    // Create the pending area eagerly so a chunk PUT is a write, not a
    // mkdir-then-write race between two concurrent chunk uploads.
    pending_store(&state, &caller, &upload_id)?;
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
) -> Result<NoContent, ApiError> {
    let upload = parse_upload_id(&path.upload_id)?;
    if path.chunk_idx >= MAX_CHUNK_COUNT {
        return Err(ApiError::validation(format!(
            "chunk index must be < {MAX_CHUNK_COUNT}"
        )));
    }
    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
        return Err(ApiError::validation(format!(
            "chunk must be 1..={MAX_CHUNK_BYTES} bytes"
        )));
    }
    let store = pending_store(&state, &caller, &path.upload_id)?;
    store
        .put_chunk(&upload, path.chunk_idx, &bytes)
        .map_err(|_| ApiError::internal())?;
    state.metrics.incr("sunrise_blob_chunk_total");
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
        .map(|h| parse_hash(h, "chunk_hashes"))
        .collect::<Result<_, _>>()?;

    let pending = pending_store(&state, &caller, &body.upload_id)?;
    // Re-hash what is actually on disk. The client's hashes are a claim; this
    // is the check. A chunk that never arrived, arrived truncated, or arrived
    // corrupted all fail here rather than becoming an unreadable attachment
    // that only surfaces months later on another device.
    let mut hasher = blake3::Hasher::new();
    let mut chunks = Vec::with_capacity(expected.len());
    let mut size_bytes: u64 = 0;
    for (idx, want) in expected.iter().enumerate() {
        let idx = u32::try_from(idx).map_err(|_| ApiError::internal())?;
        let bytes = pending
            .get_chunk(&upload, idx)
            .map_err(|_| ApiError::internal())?
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
    let committed = committed_store(&state, &caller)?;
    for (idx, bytes) in chunks.iter().enumerate() {
        let idx = u32::try_from(idx).map_err(|_| ApiError::internal())?;
        committed
            .put_chunk(&blob, idx, bytes)
            .map_err(|_| ApiError::internal())?;
    }
    let chunk_count = u32::try_from(chunks.len()).map_err(|_| ApiError::internal())?;
    write_manifest(&state, &caller, &blob, chunk_count, size_bytes)?;
    // Best effort: a leftover pending area costs disk, never correctness.
    let _ = pending.delete_all(&upload);

    state.metrics.incr("sunrise_blob_finalize_total");
    Ok(Json(FinalizeResponse {
        blob_id: format!("blb_{}", hex::encode(blob)),
        size_bytes,
        chunk_count,
    }))
}

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
) -> Result<BinaryStream<ChunkStream, OctetStream>, ApiError> {
    let blob = parse_blob_id(&path.blob_id)?;
    let (chunk_count, _) = read_manifest(&state, &caller, &blob)?.ok_or_else(not_found)?;
    let store = committed_store(&state, &caller)?;
    if !store
        .has_all(&blob, chunk_count)
        .map_err(|_| ApiError::internal())?
    {
        return Err(not_found());
    }
    state.metrics.incr("sunrise_blob_fetch_total");
    Ok(BinaryStream::new(chunk_stream(store, blob, chunk_count)))
}

/// The stream [`fetch`] returns: one chunk per poll, read on demand.
type ChunkStream = futures_util::stream::BoxStream<'static, Result<bytes::Bytes, std::io::Error>>;

/// Read `chunk_count` chunks of `blob` from `store`, one at a time.
///
/// A chunk that vanishes between the `has_all` check and its read ends the body
/// with an error rather than silently short-reading: the success status is
/// already committed by then, so truncating quietly would hand the client a
/// blob that fails its own content hash with nothing to say why.
fn chunk_stream(store: BlobStore, blob: [u8; 16], chunk_count: u32) -> ChunkStream {
    use futures_util::StreamExt as _;
    futures_util::stream::unfold(0u32, move |idx| {
        let store = store.clone();
        async move {
            if idx >= chunk_count {
                return None;
            }
            let item = match store.get_chunk(&blob, idx) {
                Ok(Some(bytes)) => Ok(bytes::Bytes::from(bytes)),
                Ok(None) => Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("chunk {idx} vanished mid-read"),
                )),
                Err(_) => Err(std::io::Error::other("chunk read failed")),
            };
            Some((item, idx + 1))
        }
    })
    .boxed()
}

// ---------------------------------------------------------------------------
// Storage layout
// ---------------------------------------------------------------------------

/// Per-account root. Keyed by a BLAKE3 of the account id so the path is
/// unconditionally filesystem-safe and reveals nothing to anyone reading the
/// disk — the same reasoning as `logging::account_h`, at full width because
/// this one has to be collision-free rather than merely correlatable.
fn account_root(state: &ServerState, caller: &Caller, area: &str) -> PathBuf {
    let digest = blake3::hash(caller.principal.account.account_id.as_bytes());
    state
        .blob_root
        .join(area)
        .join(hex::encode(&digest.as_bytes()[..16]))
}

fn pending_store(
    state: &ServerState,
    caller: &Caller,
    upload_id: &str,
) -> Result<BlobStore, ApiError> {
    // One directory per upload keeps a `delete_all` at finalize from touching
    // any other in-flight upload of the same account.
    let root = account_root(state, caller, "pending").join(upload_id);
    BlobStore::new(&root).map_err(|_| ApiError::internal())
}

fn committed_store(state: &ServerState, caller: &Caller) -> Result<BlobStore, ApiError> {
    BlobStore::new(&account_root(state, caller, "committed")).map_err(|_| ApiError::internal())
}

fn manifest_path(state: &ServerState, caller: &Caller, blob: &[u8; 16]) -> PathBuf {
    account_root(state, caller, "committed")
        .join("manifests")
        .join(hex::encode(blob))
}

/// `"<chunk_count> <size_bytes>"`. Written after every chunk, so its presence
/// is what makes a blob readable.
fn write_manifest(
    state: &ServerState,
    caller: &Caller,
    blob: &[u8; 16],
    chunk_count: u32,
    size_bytes: u64,
) -> Result<(), ApiError> {
    let path = manifest_path(state, caller, blob);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| ApiError::internal())?;
    }
    std::fs::write(&path, format!("{chunk_count} {size_bytes}")).map_err(|_| ApiError::internal())
}

fn read_manifest(
    state: &ServerState,
    caller: &Caller,
    blob: &[u8; 16],
) -> Result<Option<(u32, u64)>, ApiError> {
    let path = manifest_path(state, caller, blob);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ApiError::internal()),
    };
    let mut parts = raw.split_whitespace();
    let count: u32 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(ApiError::internal)?;
    let size: u64 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(ApiError::internal)?;
    Ok(Some((count, size)))
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

/// `up_` + 32 lowercase hex characters. Parsing rather than trusting is what
/// keeps a `../` out of a path built from a URL segment.
fn parse_upload_id(s: &str) -> Result<[u8; 16], ApiError> {
    let hexpart = s
        .strip_prefix("up_")
        .ok_or_else(|| ApiError::validation("upload_id must start with up_"))?;
    parse_hex16(hexpart, "upload_id")
}

/// `blb_` + 32 lowercase hex characters.
fn parse_blob_id(s: &str) -> Result<[u8; 16], ApiError> {
    let hexpart = s
        .strip_prefix("blb_")
        .ok_or_else(|| ApiError::validation("blob id must start with blb_"))?;
    parse_hex16(hexpart, "blob id")
}

fn parse_hex16(s: &str, field: &str) -> Result<[u8; 16], ApiError> {
    let mut out = [0u8; 16];
    if s.len() != 32 {
        return Err(ApiError::validation(format!(
            "{field} must carry 32 hex characters"
        )));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation(format!("{field} must be lowercase hex")))?;
    Ok(out)
}

fn parse_hash(s: &str, field: &'static str) -> Result<[u8; 32], ApiError> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return Err(ApiError::validation(format!(
            "{field} entries must be 64 hex characters"
        )));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation(format!("{field} must be lowercase hex")))?;
    Ok(out)
}

/// `400 BLOB_HASH_MISMATCH`, per `docs/06-server/api.md` §Blob errors: the
/// upload is well-formed but does not hash to what it claims.
fn hash_mismatch(message: impl Into<String>) -> ApiError {
    ApiError::validation_coded(codes::BLOB_HASH_MISMATCH, message)
}

/// One 404 for "no such blob" and for "not this account's blob" alike: a
/// distinguishable response would turn `GET /blobs/<hash>` into an oracle for
/// whether another account holds that ciphertext.
fn not_found() -> ApiError {
    ApiError::not_found(codes::BLOB_NOT_FOUND, "blob not found")
}

#[cfg(test)]
mod tests {
    use crate::api::error::codes;
    use crate::api::testing::{Client, SECOND_BEARER};
    use kynos::http::{Method, StatusCode};

    /// The ciphertext a test uploads. Opaque to the server by construction —
    /// what matters is only that the hashes agree.
    const CHUNKS: [&[u8]; 2] = [b"first-chunk-ciphertext", b"second-chunk-ciphertext"];

    fn hex_hash(bytes: &[u8]) -> String {
        hex::encode(blake3::hash(bytes).as_bytes())
    }

    async fn init(client: &Client, chunk_count: u32) -> String {
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(&serde_json::json!({
                    "stream_id": "str_test",
                    "chunk_count": chunk_count,
                    "size_bytes": 64,
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()["upload_id"]
            .as_str()
            .expect("an upload id")
            .to_owned()
    }

    async fn put_chunk(client: &Client, upload_id: &str, idx: u32, body: &[u8]) -> StatusCode {
        client
            .send_bytes(
                Method::PUT,
                &format!("/api/v1/blobs/{upload_id}/{idx}"),
                "application/octet-stream",
                body,
                &[],
            )
            .await
            .status
    }

    /// Init, PUT every chunk in order, and finalize — the whole happy path,
    /// returning the committed blob id.
    async fn upload(client: &Client, chunks: &[&[u8]]) -> String {
        let count = u32::try_from(chunks.len()).expect("a sane chunk count");
        let upload_id = init(client, count).await;
        for (idx, chunk) in chunks.iter().enumerate() {
            let idx = u32::try_from(idx).expect("a sane chunk index");
            assert_eq!(
                put_chunk(client, &upload_id, idx, chunk).await,
                StatusCode::NO_CONTENT
            );
        }
        let whole: Vec<u8> = chunks.concat();
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex_hash(&whole),
                    "chunk_hashes": chunks.iter().map(|c| hex_hash(c)).collect::<Vec<_>>(),
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()["blob_id"]
            .as_str()
            .expect("a blob id")
            .to_owned()
    }

    /// The whole two-phase commit, end to end.
    #[tokio::test]
    async fn chunks_finalize_and_fetch_round_trip() {
        let (client, _guard) = Client::with_blob_root();
        let upload_id = init(&client, 2).await;

        for (idx, chunk) in CHUNKS.iter().enumerate() {
            let idx = u32::try_from(idx).expect("two chunks fit in a u32");
            assert_eq!(
                put_chunk(&client, &upload_id, idx, chunk).await,
                StatusCode::NO_CONTENT
            );
        }

        let whole: Vec<u8> = CHUNKS.concat();
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex_hash(&whole),
                    "chunk_hashes": CHUNKS.iter().map(|c| hex_hash(c)).collect::<Vec<_>>(),
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        let body = res.json();
        let blob_id = body["blob_id"].as_str().expect("a blob id").to_owned();
        assert_eq!(body["chunk_count"], 2);
        assert_eq!(body["size_bytes"], whole.len());

        // The read streams chunk by chunk; what comes back must still be the
        // exact ciphertext that went in.
        let fetched = client
            .send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
            .await;
        fetched.assert_status(StatusCode::OK);
        assert_eq!(fetched.bytes, whole, "the streamed body must reassemble");
    }

    /// The client's hashes are a claim; finalize is the check.
    #[tokio::test]
    async fn finalize_rejects_a_chunk_that_does_not_hash_to_its_claim() {
        let (client, _guard) = Client::with_blob_root();
        let upload_id = init(&client, 1).await;
        assert_eq!(
            put_chunk(&client, &upload_id, 0, CHUNKS[0]).await,
            StatusCode::NO_CONTENT,
            "the chunk has to be stored, or finalize refuses it for the wrong reason"
        );

        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex_hash(b"something else"),
                    "chunk_hashes": [hex_hash(b"something else")],
                })),
            )
            .await;

        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json()["code"], codes::BLOB_HASH_MISMATCH);
    }

    /// A chunk that never arrived is a 409, not a 400: the request is
    /// well-formed and the upload simply is not finishable yet.
    #[tokio::test]
    async fn finalize_rejects_a_chunk_that_never_arrived() {
        let (client, _guard) = Client::with_blob_root();
        let upload_id = init(&client, 2).await;
        assert_eq!(
            put_chunk(&client, &upload_id, 0, CHUNKS[0]).await,
            StatusCode::NO_CONTENT,
            "chunk 0 has to be stored, or the 409 is about the wrong chunk"
        );

        let whole: Vec<u8> = CHUNKS.concat();
        let res = client
            .send(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(&serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex_hash(&whole),
                    "chunk_hashes": CHUNKS.iter().map(|c| hex_hash(c)).collect::<Vec<_>>(),
                })),
            )
            .await;

        res.assert_status(StatusCode::CONFLICT);
        assert_eq!(res.json()["code"], codes::BLOB_CHUNK_MISSING);
    }

    /// A blob nobody committed is a 404 — the same 404 another account's blob
    /// produces, so the route is not an existence oracle.
    #[tokio::test]
    async fn fetch_of_an_uncommitted_blob_is_404() {
        let (client, _guard) = Client::with_blob_root();
        let res = client
            .send(
                Method::GET,
                &format!("/api/v1/blobs/blb_{}", "0".repeat(32)),
                None,
            )
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(res.json()["code"], codes::BLOB_NOT_FOUND);
    }

    /// A traversing upload id never reaches the store — and the exact status
    /// says *where* it stops, which the previous `is_client_error` could not.
    ///
    /// It is a `404` from the router, not the handler's `400`: `up_../../etc`
    /// spans several path segments, so it matches no route and
    /// `parse_upload_id` is never called. The refusal is real and the
    /// traversal is contained, but the parse is not what contains it, and the
    /// loose assertion read as though it were. The body carries no `code`
    /// member at all, which is the other half of the same fact — it is kynos's
    /// refusal rather than this surface's.
    #[tokio::test]
    async fn a_traversing_upload_id_is_rejected() {
        let (client, _guard) = Client::with_blob_root();
        let res = client
            .send_bytes(
                Method::PUT,
                "/api/v1/blobs/up_../../etc/0",
                "application/octet-stream",
                CHUNKS[0],
                &[],
            )
            .await;
        res.assert_status(StatusCode::NOT_FOUND);
    }

    /// And the parse itself, on an id that *does* occupy one segment, so this
    /// is the assertion the test above was mistaken for: a well-shaped segment
    /// that is not `up_` plus 32 lowercase hex is the handler's own `400`.
    #[tokio::test]
    async fn a_malformed_single_segment_upload_id_is_the_handlers_400() {
        let (client, _guard) = Client::with_blob_root();

        for id in ["up_zz", "up_..", "not_an_upload_id"] {
            let res = client
                .send_bytes(
                    Method::PUT,
                    &format!("/api/v1/blobs/{id}/0"),
                    "application/octet-stream",
                    CHUNKS[0],
                    &[],
                )
                .await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert_eq!(res.json()["code"], codes::VALIDATION_INVALID, "for {id}");
        }
    }

    /// Pinned, not endorsed: an **uppercase** hex upload id is accepted.
    ///
    /// `parse_upload_id` is documented as "`up_` + 32 lowercase hex
    /// characters" and its own refusal says "must be lowercase hex", but the
    /// decode underneath is case-insensitive, so the same sixteen bytes have
    /// two spellings that reach the same pending directory. Nothing here
    /// depends on the id being canonical, so this is a statement of current
    /// behaviour rather than an endorsement of it — and it is stated so that
    /// tightening the parse fails a test instead of passing silently.
    #[tokio::test]
    async fn an_uppercase_hex_upload_id_is_accepted_despite_the_message() {
        let (client, _guard) = Client::with_blob_root();
        assert_eq!(
            put_chunk(&client, &format!("up_{}", "A".repeat(32)), 0, CHUNKS[0]).await,
            StatusCode::NO_CONTENT
        );
    }

    /// **Content addressing, which nothing checked.**
    ///
    /// `FinalizeResponse` documents "re-uploading identical ciphertext
    /// converges on the same id", and no test ever finalized the same content
    /// twice or related `blob_id` to `content_hash`. The id is `blb_` plus the
    /// first sixteen bytes of the BLAKE3 of the *concatenation*, so how a
    /// client chose to split those bytes into chunks cannot move it — which is
    /// what makes dedup across one account's devices work at all.
    #[tokio::test]
    async fn the_same_bytes_converge_on_one_blob_id_however_they_are_split() {
        let (client, _guard) = Client::with_blob_root();
        let whole: Vec<u8> = CHUNKS.concat();

        let as_one = upload(&client, &[&whole]).await;
        let as_two = upload(&client, &CHUNKS).await;
        assert_eq!(
            as_one, as_two,
            "the chunk split is the client's business, not the address's"
        );

        // And the id is the content address rather than an opaque handle the
        // server minted: it is derivable from the bytes alone.
        assert_eq!(as_one, format!("blb_{}", &hex_hash(&whole)[..32]));
    }

    /// The other direction, which is what makes the convergence above a
    /// content address rather than a constant: any differing byte gives a
    /// different id, including one in the tail that a truncated hash would
    /// miss.
    #[tokio::test]
    async fn a_single_differing_byte_gives_a_different_blob_id() {
        let (client, _guard) = Client::with_blob_root();
        let mut flipped: Vec<u8> = CHUNKS.concat();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;

        let original = upload(&client, &CHUNKS).await;
        let altered = upload(&client, &[&flipped]).await;
        assert_ne!(original, altered);

        // Both are readable and each reads back its own bytes, so the two ids
        // name two committed blobs rather than one overwriting the other.
        let fetched = client
            .send(Method::GET, &format!("/api/v1/blobs/{original}"), None)
            .await;
        fetched.assert_status(StatusCode::OK);
        assert_eq!(fetched.bytes, CHUNKS.concat());
    }

    /// **Blob paths are namespaced per caller, and nothing constrained it.**
    ///
    /// `account_root` keys every pending and committed path by a BLAKE3 of the
    /// account id, because content addressing across accounts would be a
    /// cross-tenant read primitive: one account could name another's blob by
    /// its hash. `fetch_of_an_uncommitted_blob_is_404` claims its refusal is
    /// "the same 404 another account's blob produces" — but no blob test ever
    /// had two accounts, so nothing would have failed if `account_root`
    /// stopped keying on the caller.
    #[tokio::test]
    async fn one_account_cannot_fetch_anothers_blob_and_cannot_tell_it_apart() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        let blob_id = upload(&client, &CHUNKS).await;

        // The account that committed it reads it back.
        client
            .send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
            .await
            .assert_status(StatusCode::OK);

        // A second account naming the very same content address does not —
        // and gets byte for byte what a blob nobody ever committed produces,
        // so the route answers nothing about whose ciphertext exists.
        let theirs = client
            .send_as(
                Method::GET,
                &format!("/api/v1/blobs/{blob_id}"),
                Some(SECOND_BEARER),
                None,
            )
            .await;
        let nothing = client
            .send_as(
                Method::GET,
                &format!("/api/v1/blobs/blb_{}", "0".repeat(32)),
                Some(SECOND_BEARER),
                None,
            )
            .await;

        theirs.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(theirs.json()["code"], codes::BLOB_NOT_FOUND);
        assert_eq!(
            (theirs.status, theirs.bytes),
            (nothing.status, nothing.bytes),
            "the two refusals must be indistinguishable, or this is an oracle"
        );
    }

    /// `init` bounds the upload before any byte is written.
    #[tokio::test]
    async fn init_rejects_zero_chunks_and_an_oversized_blob() {
        let (client, _guard) = Client::with_blob_root();

        client
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(&serde_json::json!({
                    "stream_id": "str_test",
                    "chunk_count": 0,
                    "size_bytes": 1,
                })),
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);

        client
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(&serde_json::json!({
                    "stream_id": "str_test",
                    "chunk_count": 1,
                    "size_bytes": super::MAX_BLOB_BYTES + 1,
                })),
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// An empty chunk hashes to something, so it would finalize cleanly while
    /// contributing nothing. Refused at the write instead.
    #[tokio::test]
    async fn an_empty_chunk_is_rejected() {
        let (client, _guard) = Client::with_blob_root();
        let upload_id = init(&client, 1).await;
        assert_eq!(
            put_chunk(&client, &upload_id, 0, b"").await,
            StatusCode::BAD_REQUEST
        );
    }

    /// Every blob operation is authenticated. This module ran unauthenticated
    /// once, so the check is a test rather than a reading.
    ///
    /// Against a verifier that actually checks. `ServerConfig::default()`
    /// installs `NullVerifier`, for which an absent header verifies the empty
    /// string and succeeds — deliberately, so that enabling authentication is
    /// purely a matter of configuring a verifier. Asserting a 401 there would
    /// be asserting the self-host escape hatch is broken.
    #[tokio::test]
    async fn every_blob_route_requires_a_bearer() {
        let (client, _guard) = Client::with_blob_root_and_verifier();
        for (method, path) in [
            (Method::POST, "/api/v1/blobs/init"),
            (Method::POST, "/api/v1/blobs/finalize"),
            (
                Method::PUT,
                "/api/v1/blobs/up_00000000000000000000000000000000/0",
            ),
            (
                Method::GET,
                "/api/v1/blobs/blb_00000000000000000000000000000000",
            ),
        ] {
            let res = client.send_as(method.clone(), path, None, None).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} must refuse an unauthenticated caller"
            );
        }
    }
}
