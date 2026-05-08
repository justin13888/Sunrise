//! `GET /api/v1/meta` — server version, supported protocol versions,
//! capability bitfield.

use crate::state::ServerState;
use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_wire_protocol::capability::REQUIRED_SERVER_BITS;

/// Server meta response.
#[derive(Debug, Serialize)]
pub struct MetaResponse {
    /// Server's `<semver>+<platform>` string.
    pub server_app_v: String,
    /// Wire-protocol versions the server speaks.
    pub wire_proto_supported: Vec<u32>,
    /// Crypto-suite versions the server speaks.
    pub crypto_suite_supported: Vec<u32>,
    /// Server's `doc_schema_floor` (lowest accepted).
    pub doc_schema_floor: u32,
    /// Server's capability bitfield.
    pub capabilities: u64,
}

/// Mount the meta route.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new().route("/meta", get(handler))
}

async fn handler(State(state): State<ServerState>) -> Json<MetaResponse> {
    Json(MetaResponse {
        server_app_v: state.config.server_app_v.clone(),
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        doc_schema_floor: u32::from(DOC_SCHEMA_V),
        capabilities: REQUIRED_SERVER_BITS.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, config::ServerConfig};
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn meta_returns_versions() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/meta")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["wire_proto_supported"], serde_json::json!([1]));
        assert_eq!(v["crypto_suite_supported"], serde_json::json!([1]));
        assert_eq!(v["doc_schema_floor"], 1);
    }
}
