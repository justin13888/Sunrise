//! `GET /api/v1/meta` — versions, capability bitfield, and the bootstrap facts.

use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use serde::{Deserialize, Serialize};
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
use sunrise_wire_protocol::capability::REQUIRED_SERVER_BITS;

/// The device-binding mode this build speaks.
///
/// Re-exported from the crate that implements it, so the string a client reads
/// out of `/meta` cannot drift from the scheme the server actually verifies.
pub use sunrise_http_sig::BINDING_MODE as DEVICE_BINDING_MODE;

/// Server meta response.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
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
    /// Device-binding mode. The field exists so a client can detect a server
    /// that has moved on without having to infer it from a 403.
    pub device_binding_mode: String,
    /// Whether the server *requires* device binding on authenticated requests.
    /// A client that sees `false` may still sign, and the signature is still
    /// verified.
    pub device_binding_required: bool,
}

/// Versions, capabilities, and the facts a client needs before it has a token.
///
/// Unauthenticated on purpose: publishing the issuer here is what lets a fresh
/// client bootstrap its login flow, so requiring a token first would be
/// circular.
#[kynos::get("/api/v1/meta")]
pub async fn meta(Inject(state): Inject<ServerState>) -> Json<MetaResponse> {
    Json(MetaResponse {
        server_app_v: state.config.server_app_v.clone(),
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        doc_schema_floor: u32::from(DOC_SCHEMA_FLOOR),
        capabilities: REQUIRED_SERVER_BITS.0,
        oidc_issuer: state.config.oidc_issuer.clone(),
        oidc_client_id: state.config.oidc_client_id.clone(),
        device_binding_mode: DEVICE_BINDING_MODE.to_owned(),
        device_binding_required: state.config.require_device_sig,
    })
}
