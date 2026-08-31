//! `GET /api/v1/health` — simple liveness probe.

use crate::state::ServerState;
use axum::{routing::get, Json, Router};
use serde::Serialize;

/// Health response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// `"ok"` while the server is healthy.
    pub status: &'static str,
}

/// Mount the health route.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new().route("/health", get(handler))
}

async fn handler() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, config::ServerConfig};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_returns_ok() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);
    }
}
