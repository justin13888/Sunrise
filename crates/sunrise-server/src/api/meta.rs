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
#[kynos::get("/api/v1/meta", operation_id = "meta")]
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

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::state::ServerState;
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::Arc;
    use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
    use sunrise_wire_protocol::capability::REQUIRED_SERVER_BITS;

    /// **What `/meta` returns, which only a substring check ever touched.**
    ///
    /// `sunrise-e2e`'s health round trip asserted `body.contains("wire_proto")
    /// || body.contains("crypto_suite")` on a raw response string — satisfied
    /// by the field names alone, either one sufficing, and the values never
    /// read. Every member below could have carried the wrong number.
    #[tokio::test]
    async fn meta_reports_the_versions_this_build_speaks() {
        let config = ServerConfig::default();
        let expected_app_v = config.server_app_v.clone();
        let client = Client::new(config);

        let res = client.send(Method::GET, "/api/v1/meta", None).await;
        res.assert_status(StatusCode::OK);
        let meta = res.json();

        assert_eq!(meta["server_app_v"], serde_json::json!(expected_app_v));
        assert_eq!(
            meta["wire_proto_supported"],
            serde_json::json!([u32::from(WIRE_PROTO_V)])
        );
        assert_eq!(
            meta["crypto_suite_supported"],
            serde_json::json!([u32::from(CRYPTO_SUITE_V)])
        );
        assert_eq!(
            meta["doc_schema_floor"],
            serde_json::json!(u32::from(DOC_SCHEMA_FLOOR))
        );
        assert_eq!(
            meta["capabilities"],
            serde_json::json!(REQUIRED_SERVER_BITS.0)
        );
        assert_eq!(
            meta["device_binding_mode"],
            serde_json::json!(super::DEVICE_BINDING_MODE),
            "the string a client reads must be the scheme the server verifies"
        );
    }

    /// `device_binding_required` is the fact `api::signed` publishes so a
    /// client can detect the policy instead of inferring it from a refusal,
    /// and `an_absent_binding_where_one_is_required_names_the_signature`
    /// reasons from it having been advertised. Both directions, because a
    /// constant would have satisfied the one that was never checked.
    #[tokio::test]
    async fn meta_reports_the_device_binding_policy_in_force() {
        for required in [false, true] {
            let client = Client::new(ServerConfig {
                require_device_sig: required,
                ..ServerConfig::default()
            });
            let res = client.send(Method::GET, "/api/v1/meta", None).await;
            res.assert_status(StatusCode::OK);
            assert_eq!(
                res.json()["device_binding_required"],
                serde_json::json!(required)
            );
        }
    }

    /// The bootstrap facts, and why the route is open.
    ///
    /// Publishing the issuer here is what lets a client with no token start its
    /// login flow, so requiring one first would be circular — asserted against
    /// a verifier that actually checks, since `NullVerifier` accepts an absent
    /// bearer and cannot tell an open route from a closed one.
    #[tokio::test]
    async fn meta_publishes_the_issuer_to_a_caller_with_no_token() {
        let state = ServerState::new(ServerConfig {
            oidc_issuer: Some("https://auth.example.com".to_owned()),
            oidc_client_id: Some("sunrise".to_owned()),
            ..ServerConfig::default()
        })
        .with_verifier(Arc::new(
            StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
        ));
        let client = Client::from_state(state);

        let res = client
            .send_as(Method::GET, "/api/v1/meta", None, None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(
            res.json()["oidc_issuer"],
            serde_json::json!("https://auth.example.com")
        );
        assert_eq!(res.json()["oidc_client_id"], serde_json::json!("sunrise"));

        // Self-host has no IdP, and says so rather than omitting the member.
        let client = Client::new(ServerConfig::default());
        let res = client.send(Method::GET, "/api/v1/meta", None).await;
        assert_eq!(res.json()["oidc_issuer"], serde_json::Value::Null);
        assert_eq!(res.json()["oidc_client_id"], serde_json::Value::Null);
    }
}
