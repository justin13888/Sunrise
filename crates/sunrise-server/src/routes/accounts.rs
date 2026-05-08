//! `POST /api/v1/accounts` and `GET /api/v1/accounts/me`.
//!
//! v1 implementation: shape-only — accepts the request body, returns a
//! deterministic response, doesn't yet persist. Persistence + OIDC auth
//! land in Phase 17.

use crate::state::ServerState;
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use sunrise_onboarding::{AccountCreateRequest, AccountInfo};

/// Mount account routes.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/accounts", post(create))
        .route("/accounts/me", get(me))
}

async fn create(
    State(_state): State<ServerState>,
    Json(req): Json<AccountCreateRequest>,
) -> Result<(StatusCode, Json<AccountInfo>), (StatusCode, &'static str)> {
    if req.email.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "email required"));
    }
    // v1: derive a stable identity_id from the supplied identity_signing_pub
    // (echoed back). Production will run the BLAKE3.derive_key chain
    // server-side from the validated request.
    Ok((
        StatusCode::CREATED,
        Json(AccountInfo {
            identity_id: req.identity_signing_pub[..16.min(req.identity_signing_pub.len())]
                .to_string(),
            email: req.email.trim().to_ascii_lowercase(),
            tier: "free".to_string(),
            device_count: 1,
            created_at_ms: 0,
        }),
    ))
}

async fn me(State(_state): State<ServerState>) -> (StatusCode, Json<AccountInfo>) {
    // Without OIDC auth wiring, return a sentinel record.
    (
        StatusCode::OK,
        Json(AccountInfo {
            identity_id: "unauthenticated".into(),
            email: String::new(),
            tier: "free".into(),
            device_count: 0,
            created_at_ms: 0,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, config::ServerConfig};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn account_create_round_trip() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "email": "user@example.com",
            "identity_signing_pub": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "identity_dh_pub": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "recovery_blob": "Cg",
            "terms_at_ms": 1_700_000_000_000_u64,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/accounts")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["email"], "user@example.com");
        assert_eq!(v["tier"], "free");
    }

    #[tokio::test]
    async fn empty_email_rejected() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "email": "  ",
            "identity_signing_pub": "x",
            "identity_dh_pub": "y",
            "recovery_blob": "z",
            "terms_at_ms": 1_u64,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/accounts")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
