//! `GET /api/v1/meta` — server version, supported protocol versions,
//! capability bitfield.

use crate::state::ServerState;
use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
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
    /// OIDC issuer discovery URL, so an unauthenticated client can bootstrap
    /// the login flow without static configuration. `None` in self-host mode.
    pub oidc_issuer: Option<String>,
    /// OIDC client id tokens must be audienced to.
    pub oidc_client_id: Option<String>,
    /// Device-binding mode, per `docs/06-server/api.md`. `header_sig_v1` is the
    /// only mode v1 defines; the field exists so a future client can detect a
    /// server that has moved on without having to guess from a 403.
    pub device_binding_mode: &'static str,
    /// Whether the server *requires* device binding on authenticated requests.
    /// A client that sees `false` may still send a signature, and it will still
    /// be verified.
    pub device_binding_required: bool,
}

/// The only device-binding mode v1 speaks.
pub const DEVICE_BINDING_MODE: &str = "header_sig_v1";

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
        doc_schema_floor: u32::from(DOC_SCHEMA_FLOOR),
        capabilities: REQUIRED_SERVER_BITS.0,
        oidc_issuer: state.config.oidc_issuer.clone(),
        oidc_client_id: state.config.oidc_client_id.clone(),
        device_binding_mode: DEVICE_BINDING_MODE,
        device_binding_required: state.config.require_device_sig,
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

    /// `/meta` is unauthenticated on purpose — a client has to be able to read
    /// it *before* it has a token, which is the point of publishing the issuer
    /// here at all.
    #[tokio::test]
    async fn meta_advertises_the_bootstrap_facts_without_a_token() {
        let cfg = ServerConfig {
            oidc_issuer: Some("https://idp.example".into()),
            oidc_client_id: Some("sunrise-relay".into()),
            require_device_sig: true,
            ..Default::default()
        };
        let app = build_router(ServerState::new(cfg));
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
        assert_eq!(v["oidc_issuer"], "https://idp.example");
        assert_eq!(v["oidc_client_id"], "sunrise-relay");
        assert_eq!(v["device_binding_mode"], "header_sig_v1");
        assert_eq!(v["device_binding_required"], true);
    }
}
