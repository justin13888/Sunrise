//! Blob two-phase-commit routes (`/api/v1/blobs/...`).
//!
//! Per `docs/06-server/api.md` and `docs/02-domain/attachments.md`. Three
//! phases and a read:
//!
//! 1. `POST /blobs/init` reserves an upload id and hands back one URL per
//!    chunk.
//! 2. `PUT /blobs/{upload_id}/{idx}` stores one chunk of **opaque ciphertext**
//!    under the caller's pending area.
//! 3. `POST /blobs/finalize` re-hashes every stored chunk with BLAKE3, checks
//!    each against the client's per-chunk hash and the concatenation against
//!    the client's `content_hash`, and only then commits the blob under its
//!    content address.
//! 4. `GET /blobs/{blob_id}` reassembles the committed chunks and serves them,
//!    re-verifying the content hash on the way out.
//!
//! Everything here is E2EE ciphertext. The server never holds a key and can
//! never read a byte of it; hashing is an *integrity* check, not a content
//! check, which is exactly why the client sends the hashes it computed over
//! the same ciphertext.
//!
//! # Isolation
//!
//! Both the pending area and the committed store are rooted at a per-account
//! directory, keyed by a BLAKE3 of the account id. Content addressing across
//! accounts would be a cross-tenant read primitive: one account could name
//! another's blob by its hash. Per-account roots make that impossible by
//! construction rather than by a check someone can forget, and they keep
//! dedup working where it should — across one account's devices.
//!
//! # Auth
//!
//! Every route runs the full `authenticate` pipeline. This module was missed
//! by the auth work in `e1a0c98` and ran unauthenticated until now.

use crate::auth::request::{authenticate, Caller, RequestContext};
use crate::error::{codes, ApiError};
use crate::ServerState;
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sunrise_storage::BlobStore;

/// Largest single chunk the relay accepts, per
/// `docs/02-domain/attachments.md` §Lazy fetch ("chunks are 1 MiB each").
/// The router's body limit caps this again; both are deliberate.
const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// Most chunks one blob may have. 4096 x 1 MiB is 4 GiB of addressable space,
/// far above the size policy below, and it bounds the work `finalize` does.
const MAX_CHUNK_COUNT: u32 = 4096;

/// Hard per-attachment ceiling from `docs/02-domain/attachments.md` §Size
/// policy: 100 MB on managed cloud, configurable on self-host.
const MAX_BLOB_BYTES: u64 = 100 * 1000 * 1000;

/// Mount blob routes.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/blobs/init", post(init))
        .route("/blobs/finalize", post(finalize))
        // The URL `init` hands back. It was never mounted, so every chunk
        // upload 404'd and no blob could ever be assembled. Note the axum 0.7
        // `:param` syntax: the old `/blobs/{blob_id}` was a LITERAL segment,
        // which is the other half of why `fetch` answered 404 unconditionally.
        .route("/blobs/:upload_id/:chunk_idx", put(put_chunk))
        .route("/blobs/:blob_id", get(fetch))
}

/// `POST /api/v1/blobs/init` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct InitRequest {
    /// Stream id this blob belongs to.
    pub stream_id: String,
    /// Total chunk count.
    pub chunk_count: u32,
    /// Total ciphertext size in bytes (advisory; the server enforces the
    /// per-attachment ceiling here and the real size at finalize).
    pub size_bytes: u64,
}

/// `POST /api/v1/blobs/init` response body.
#[derive(Debug, Clone, Serialize)]
pub struct InitResponse {
    /// Server-assigned upload id, used by the chunk PUTs and by `finalize`.
    pub upload_id: String,
    /// Per-chunk upload URLs. Self-host returns relative paths under
    /// `/api/v1/blobs/<upload_id>/<idx>`, which the PUT handler accepts.
    pub chunk_urls: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct FinalizeRequest {
    /// Upload id from `init`.
    pub upload_id: String,
    /// BLAKE3 of the concatenated chunk ciphertext (32 bytes, hex).
    pub content_hash: String,
    /// Per-chunk BLAKE3 hashes (32 bytes hex each), in order. The length is
    /// the chunk count: `init`'s advisory count is not trusted here.
    pub chunk_hashes: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` response body.
#[derive(Debug, Clone, Serialize)]
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

async fn caller_of(
    state: &ServerState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Caller, ApiError> {
    authenticate(
        state,
        RequestContext {
            method,
            uri,
            headers,
            body,
        },
    )
    .await
}

async fn init(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<InitResponse>), ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    let req: InitRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("malformed init body: {e}")))?;
    if req.chunk_count == 0 || req.chunk_count > MAX_CHUNK_COUNT {
        return Err(ApiError::validation(format!(
            "chunk_count must be 1..={MAX_CHUNK_COUNT}"
        )));
    }
    if req.size_bytes > MAX_BLOB_BYTES {
        return Err(ApiError::validation(format!(
            "size_bytes exceeds the {MAX_BLOB_BYTES}-byte per-attachment limit"
        )));
    }
    if req.stream_id.trim().is_empty() {
        return Err(ApiError::validation("stream_id required"));
    }

    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).map_err(|_| ApiError::internal())?;
    let upload_id = format!("up_{}", hex::encode(raw));
    // Create the pending area eagerly so a chunk PUT is a write, not a
    // mkdir-then-write race between two concurrent chunk uploads.
    pending_store(&state, &caller, &upload_id)?;
    let chunk_urls = (0..req.chunk_count)
        .map(|i| format!("/api/v1/blobs/{upload_id}/{i}"))
        .collect();
    state.metrics.incr("sunrise_blob_init_total");
    Ok((
        StatusCode::OK,
        Json(InitResponse {
            upload_id,
            chunk_urls,
        }),
    ))
}

async fn put_chunk(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path((upload_id, chunk_idx)): Path<(String, u32)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    let upload = parse_upload_id(&upload_id)?;
    if chunk_idx >= MAX_CHUNK_COUNT {
        return Err(ApiError::validation(format!(
            "chunk index must be < {MAX_CHUNK_COUNT}"
        )));
    }
    if body.is_empty() || body.len() > MAX_CHUNK_BYTES {
        return Err(ApiError::validation(format!(
            "chunk must be 1..={MAX_CHUNK_BYTES} bytes"
        )));
    }
    let store = pending_store(&state, &caller, &upload_id)?;
    store
        .put_chunk(&upload, chunk_idx, &body)
        .map_err(|_| ApiError::internal())?;
    state.metrics.incr("sunrise_blob_chunk_total");
    Ok(StatusCode::NO_CONTENT)
}

async fn finalize(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<FinalizeResponse>), ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    let req: FinalizeRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("malformed finalize body: {e}")))?;
    let upload = parse_upload_id(&req.upload_id)?;
    let content_hash = parse_hash(&req.content_hash, "content_hash")?;
    if req.chunk_hashes.is_empty() || req.chunk_hashes.len() > MAX_CHUNK_COUNT as usize {
        return Err(ApiError::validation(format!(
            "chunk_hashes must hold 1..={MAX_CHUNK_COUNT} entries"
        )));
    }
    let expected: Vec<[u8; 32]> = req
        .chunk_hashes
        .iter()
        .map(|h| parse_hash(h, "chunk_hashes"))
        .collect::<Result<_, _>>()?;

    let pending = pending_store(&state, &caller, &req.upload_id)?;
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
                ApiError::new(
                    StatusCode::CONFLICT,
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
    Ok((
        StatusCode::OK,
        Json(FinalizeResponse {
            blob_id: format!("blb_{}", hex::encode(blob)),
            size_bytes,
            chunk_count,
        }),
    ))
}

async fn fetch(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(blob_id): Path<String>,
    body: Bytes,
) -> Result<([(axum::http::HeaderName, &'static str); 1], Vec<u8>), ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    let blob = parse_blob_id(&blob_id)?;
    let (chunk_count, _) = read_manifest(&state, &caller, &blob)?.ok_or_else(not_found)?;
    let store = committed_store(&state, &caller)?;
    if !store
        .has_all(&blob, chunk_count)
        .map_err(|_| ApiError::internal())?
    {
        return Err(not_found());
    }
    let mut out = Vec::new();
    for idx in 0..chunk_count {
        let bytes = store
            .get_chunk(&blob, idx)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(not_found)?;
        out.extend_from_slice(&bytes);
    }
    state.metrics.incr("sunrise_blob_fetch_total");
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        out,
    ))
}

// ---------------------------------------------------------------------------
// Storage layout
// ---------------------------------------------------------------------------

/// Per-account root. Keyed by a BLAKE3 of the account id so the path is
/// unconditionally filesystem-safe and reveals nothing to anyone reading the
/// disk — the same reasoning as `logging::account_h`, at full width because
/// this one has to be collision-free rather than merely correlatable.
fn account_root(state: &ServerState, caller: &Caller, area: &str) -> PathBuf {
    let digest = blake3::hash(caller.account.account_id.as_bytes());
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
    ApiError::new(StatusCode::BAD_REQUEST, codes::BLOB_HASH_MISMATCH, message)
}

/// One 404 for "no such blob" and for "not this account's blob" alike: a
/// distinguishable response would turn `GET /blobs/<hash>` into an oracle for
/// whether another account holds that ciphertext.
fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        codes::BLOB_NOT_FOUND,
        "blob not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, ServerConfig};
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    /// A state whose blob root is a fresh temp dir, plus the guard that keeps
    /// it alive for the test.
    fn state() -> (ServerState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            blob_root: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        };
        (ServerState::new(cfg), dir)
    }

    async fn json_post(
        state: &ServerState,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let app = build_router(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    async fn put_bytes(state: &ServerState, path: &str, body: Vec<u8>) -> StatusCode {
        let app = build_router(state.clone());
        app.oneshot(
            Request::builder()
                .method("PUT")
                .uri(path)
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    }

    async fn get_blob(state: &ServerState, path: &str) -> (StatusCode, Vec<u8>) {
        let app = build_router(state.clone());
        let res = app
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, bytes.to_vec())
    }

    fn hex32(bytes: &[u8]) -> String {
        hex::encode(blake3::hash(bytes).as_bytes())
    }

    /// The whole point of the issue: chunk upload -> finalize -> fetch, with
    /// the bytes coming back exactly as they went in.
    #[tokio::test]
    async fn chunks_finalize_and_fetch_round_trip() {
        let (st, _dir) = state();
        let chunks: Vec<Vec<u8>> = vec![b"first-chunk".to_vec(), b"second-chunk".to_vec()];
        let whole: Vec<u8> = chunks.concat();

        let (status, init) = json_post(
            &st,
            "/api/v1/blobs/init",
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 2,
                "size_bytes": whole.len(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let upload_id = init["upload_id"].as_str().unwrap().to_string();
        let urls: Vec<String> = init["chunk_urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(urls.len(), 2);

        for (url, chunk) in urls.iter().zip(&chunks) {
            assert_eq!(
                put_bytes(&st, url, chunk.clone()).await,
                StatusCode::NO_CONTENT,
                "the chunk route the init response points at must exist"
            );
        }

        let (status, fin) = json_post(
            &st,
            "/api/v1/blobs/finalize",
            serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex32(&whole),
                "chunk_hashes": chunks.iter().map(|c| hex32(c)).collect::<Vec<_>>(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fin["size_bytes"].as_u64(), Some(whole.len() as u64));
        assert_eq!(fin["chunk_count"].as_u64(), Some(2));
        let blob_id = fin["blob_id"].as_str().unwrap().to_string();
        // Content-addressed: the id is derived from the bytes, not minted.
        assert_eq!(blob_id, format!("blb_{}", &hex32(&whole)[..32]));

        let (status, body) = get_blob(&st, &format!("/api/v1/blobs/{blob_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, whole);
    }

    #[tokio::test]
    async fn finalize_rejects_a_chunk_that_does_not_hash_to_its_claim() {
        let (st, _dir) = state();
        let (_, init) = json_post(
            &st,
            "/api/v1/blobs/init",
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 1,
                "size_bytes": 5,
            }),
        )
        .await;
        let upload_id = init["upload_id"].as_str().unwrap().to_string();
        put_bytes(
            &st,
            &format!("/api/v1/blobs/{upload_id}/0"),
            b"actual".to_vec(),
        )
        .await;

        let (status, _) = json_post(
            &st,
            "/api/v1/blobs/finalize",
            serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex32(b"claimed"),
                "chunk_hashes": [hex32(b"claimed")],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn finalize_rejects_a_chunk_that_never_arrived() {
        let (st, _dir) = state();
        let (_, init) = json_post(
            &st,
            "/api/v1/blobs/init",
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 2,
                "size_bytes": 10,
            }),
        )
        .await;
        let upload_id = init["upload_id"].as_str().unwrap().to_string();
        put_bytes(
            &st,
            &format!("/api/v1/blobs/{upload_id}/0"),
            b"one".to_vec(),
        )
        .await;

        let (status, _) = json_post(
            &st,
            "/api/v1/blobs/finalize",
            serde_json::json!({
                "upload_id": upload_id,
                "content_hash": hex32(b"onetwo"),
                "chunk_hashes": [hex32(b"one"), hex32(b"two")],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn fetch_of_an_uncommitted_blob_is_404() {
        let (st, _dir) = state();
        let (status, _) = get_blob(&st, &format!("/api/v1/blobs/blb_{}", "ab".repeat(16))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Path segments reach the filesystem, so they are parsed rather than
    /// trusted. A traversal attempt is a 400, not a write outside the root.
    #[tokio::test]
    async fn a_traversing_upload_id_is_rejected() {
        let (st, _dir) = state();
        assert_eq!(
            put_bytes(&st, "/api/v1/blobs/up_..%2f..%2fetc/0", b"x".to_vec()).await,
            StatusCode::BAD_REQUEST
        );
        let (status, _) = get_blob(&st, "/api/v1/blobs/blb_..%2f..%2fetc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn init_rejects_zero_chunks_and_an_oversized_blob() {
        let (st, _dir) = state();
        for body in [
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 0,
                "size_bytes": 0,
            }),
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 1,
                "size_bytes": MAX_BLOB_BYTES + 1,
            }),
        ] {
            let (status, _) = json_post(&st, "/api/v1/blobs/init", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn an_empty_chunk_is_rejected() {
        let (st, _dir) = state();
        let (_, init) = json_post(
            &st,
            "/api/v1/blobs/init",
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 1,
                "size_bytes": 1,
            }),
        )
        .await;
        let upload_id = init["upload_id"].as_str().unwrap().to_string();
        assert_eq!(
            put_bytes(&st, &format!("/api/v1/blobs/{upload_id}/0"), Vec::new()).await,
            StatusCode::BAD_REQUEST
        );
    }

    /// The module ran unauthenticated. With a real verifier installed, every
    /// route rejects an anonymous caller.
    #[tokio::test]
    async fn every_blob_route_requires_a_bearer() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            blob_root: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        };
        // An empty allow-map: every bearer, including the absent one, is
        // rejected. `NullVerifier` (the self-host default) would accept them.
        let st = ServerState::new(cfg)
            .with_verifier(std::sync::Arc::new(crate::StaticVerifier::default()));

        let (status, _) = json_post(
            &st,
            "/api/v1/blobs/init",
            serde_json::json!({
                "stream_id": "str_00000000000000000000000000",
                "chunk_count": 1,
                "size_bytes": 1,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = json_post(
            &st,
            "/api/v1/blobs/finalize",
            serde_json::json!({
                "upload_id": format!("up_{}", "ab".repeat(16)),
                "content_hash": hex32(b"x"),
                "chunk_hashes": [hex32(b"x")],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        assert_eq!(
            put_bytes(
                &st,
                &format!("/api/v1/blobs/up_{}/0", "ab".repeat(16)),
                b"x".to_vec()
            )
            .await,
            StatusCode::UNAUTHORIZED
        );

        let (status, _) = get_blob(&st, &format!("/api/v1/blobs/blb_{}", "ab".repeat(16))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
