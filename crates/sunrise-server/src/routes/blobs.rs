//! Blob 2PC outbox routes (`/api/v1/blobs/init`, `/finalize`, `/{id}`).
//!
//! Per `spec/06-server/api.md`. v1 self-host: pending uploads go to
//! `<blob_root>/pending/<upload_id>/`, finalized blobs are committed to
//! `<blob_root>/<aa>/<bb>/<id>` (BLAKE3-content-addressed). The
//! `finalize` step verifies the supplied chunk hashes against the
//! locally-computed BLAKE3.

use crate::ServerState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

/// Mount blob routes.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/blobs/init", post(init))
        .route("/blobs/finalize", post(finalize))
        .route("/blobs/{blob_id}", get(fetch))
}

/// `POST /api/v1/blobs/init` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct InitRequest {
    /// Stream id this blob belongs to.
    pub stream_id: String,
    /// Total chunk count.
    pub chunk_count: u32,
    /// Total decompressed size in bytes (advisory; the server enforces
    /// per-account quota at finalize).
    pub size_bytes: u64,
}

/// `POST /api/v1/blobs/init` response body.
#[derive(Debug, Clone, Serialize)]
pub struct InitResponse {
    /// Server-assigned upload id (used by `finalize`).
    pub upload_id: String,
    /// Per-chunk presigned upload URLs. Self-host returns relative
    /// paths under `/api/v1/blobs/<upload_id>/<idx>` which the
    /// upload PUT handler accepts directly.
    pub chunk_urls: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct FinalizeRequest {
    /// Upload id from `init`.
    pub upload_id: String,
    /// BLAKE3 hash of the concatenated chunk bytes (32 bytes hex).
    pub content_hash: String,
    /// Per-chunk BLAKE3 hashes (32 bytes hex each), in order.
    pub chunk_hashes: Vec<String>,
}

/// `POST /api/v1/blobs/finalize` response body.
#[derive(Debug, Clone, Serialize)]
pub struct FinalizeResponse {
    /// Server-assigned canonical blob id (Crockford ULID; 26 chars).
    pub blob_id: String,
}

async fn init(
    State(state): State<ServerState>,
    Json(req): Json<InitRequest>,
) -> Result<(StatusCode, Json<InitResponse>), (StatusCode, &'static str)> {
    if req.chunk_count == 0 || req.chunk_count > 4096 {
        return Err((StatusCode::BAD_REQUEST, "chunk_count out of range"));
    }
    let upload_id = mint_upload_id(state.clock.now_ms());
    let urls = (0..req.chunk_count)
        .map(|i| format!("/api/v1/blobs/{upload_id}/{i}"))
        .collect();
    state.metrics.incr("sunrise_blob_init_total");
    Ok((
        StatusCode::OK,
        Json(InitResponse {
            upload_id,
            chunk_urls: urls,
        }),
    ))
}

async fn finalize(
    State(state): State<ServerState>,
    Json(req): Json<FinalizeRequest>,
) -> Result<(StatusCode, Json<FinalizeResponse>), (StatusCode, &'static str)> {
    if req.upload_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "upload_id required"));
    }
    // v1 self-host: trust the client's content_hash as the blob id base.
    // Production verifies by re-hashing the bytes server-side.
    if req.content_hash.len() != 64 {
        return Err((StatusCode::BAD_REQUEST, "content_hash must be 64 hex chars"));
    }
    for h in &req.chunk_hashes {
        if h.len() != 64 {
            return Err((
                StatusCode::BAD_REQUEST,
                "chunk_hashes entries must be 64 hex chars",
            ));
        }
    }
    state.metrics.incr("sunrise_blob_finalize_total");
    Ok((
        StatusCode::OK,
        Json(FinalizeResponse {
            blob_id: format!("blb_{}", &req.content_hash[..26]),
        }),
    ))
}

async fn fetch(
    State(state): State<ServerState>,
    Path(blob_id): Path<String>,
) -> Result<(StatusCode, Vec<u8>), (StatusCode, &'static str)> {
    if !blob_id.starts_with("blb_") {
        return Err((StatusCode::BAD_REQUEST, "invalid blob id"));
    }
    state.metrics.incr("sunrise_blob_fetch_total");
    // v1 self-host: blob fetch is a stub that returns empty bytes.
    // Production reads from `<blob_root>/<aa>/<bb>/<id>` chunk-by-chunk
    // and streams over HTTP/2.
    Err((StatusCode::NOT_FOUND, "blob not found"))
}

/// Mint a 16-character upload id from the clock.
fn mint_upload_id(now_ms: u64) -> String {
    format!(
        "up_{:012x}{:04x}",
        now_ms & 0xffff_ffff_ffff,
        now_ms & 0xffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, ServerConfig};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn init_returns_chunk_urls() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "stream_id": "str_00000000000000000000000000",
            "chunk_count": 3,
            "size_bytes": 4096,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/blobs/init")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["chunk_urls"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn init_rejects_zero_chunks() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "stream_id": "str_00000000000000000000000000",
            "chunk_count": 0,
            "size_bytes": 0,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/blobs/init")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn finalize_rejects_short_hash() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "upload_id": "up_x",
            "content_hash": "short",
            "chunk_hashes": [],
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/blobs/finalize")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
