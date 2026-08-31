//! The typed REST surface, and the OpenAPI 3.2 document that describes it.
//!
//! Per [ADR-0021](../../../../docs/11-adr/0021-kynos-openapi-server.md). The
//! surface this replaces had no machine-readable contract of any kind: fourteen
//! routes registered by hand across five modules, nine drifted CDDL blocks in
//! `docs/06-server/api.md`, and no client anywhere in the workspace — the
//! account and device bootstrap was specified, served, and never invoked.
//!
//! The rule that makes this different from bolting a generator onto the old
//! router is that a handler which cannot be described does not compile. The
//! document is generated from the operations, not maintained beside them.
//!
//! ## Why this module exists beside `routes/`
//!
//! Ported routes live here; the rest stay in `routes/` on axum until they move.
//! Both are served from one listener by the dispatcher in [`crate::serve`],
//! which owns its own accept loop and hands everything that is not `/sync` to
//! the kynos service. That is the embedding kynos's `Service::call` documents,
//! and it is deliberately *not* `into_tower_unchecked`: that conversion flags
//! every operation in the document as `OpaqueReason::UntypedLayer`, which would
//! spend this ADR's entire benefit for the whole length of the migration.
//!
//! The two-stack state ends with [ADR-0023](../../../../docs/11-adr/0023-sse-sync-transport.md),
//! when `/sync` stops being a WebSocket and axum leaves the workspace.

use crate::state::ServerState;

pub mod accounts;
pub mod auth;
pub mod devices;
pub mod error;
pub mod health;
pub mod meta;
pub mod signed;
#[cfg(test)]
pub(crate) mod testing;

/// The API description, targeted at OpenAPI 3.2.
///
/// `openapi_as` targets rather than downgrades: if the surface grows something
/// 3.2 cannot express, this fails and names what blocks it, instead of quietly
/// emitting an older version that describes the API less well.
///
/// # Errors
/// Returns kynos's error when the router cannot be described at 3.2.
pub fn document() -> kynos::Result<kynos::openapi::Document> {
    router().openapi_as(kynos::openapi::SpecVersion::V3_2)
}

/// Build the typed router for every ported operation.
///
/// # Errors
/// Returns kynos's build error when the router cannot be described — a route
/// whose operations conflict, or a handler whose types do not resolve.
pub fn router() -> kynos::Router<ServerState> {
    kynos::Router::<ServerState>::new()
        .mount(kynos::routes![health::health])
        .mount(kynos::routes![meta::meta])
        .mount(kynos::routes![accounts::create, accounts::me])
        .mount(kynos::routes![
            devices::list,
            devices::register,
            devices::revoke,
            devices::push_tokens
        ])
}

#[cfg(test)]
mod tests {
    /// The document is the contract, so its shape is a test rather than a
    /// build artefact nobody reads.
    ///
    /// `openapi_as` runs the structural checks, which is what makes "an API
    /// that cannot be described correctly fails at startup" true rather than
    /// aspirational.
    #[test]
    fn the_router_describes_itself() {
        let doc = super::document().expect("the router must describe at 3.2");
        let json = doc.to_json().expect("the description must serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(
            v["openapi"]
                .as_str()
                .unwrap_or_default()
                .split('.')
                .take(2)
                .collect::<Vec<_>>(),
            vec!["3", "2"],
            "the document must be emitted AS 3.2 rather than at whatever minimum \
             expresses today's surface: `itemSchema` is what ADR-0023's event \
             stream needs, and 3.1 has no way to say it"
        );

        for path in [
            "/api/v1/health",
            "/api/v1/meta",
            "/api/v1/accounts",
            "/api/v1/accounts/me",
            "/api/v1/devices",
            "/api/v1/devices/{device_id}",
            "/api/v1/devices/push-tokens",
        ] {
            assert!(
                v["paths"].get(path).is_some(),
                "{path} must appear in the document"
            );
        }
    }

    /// An authenticated operation must *say* it is authenticated.
    ///
    /// Taking `Auth<AccountToken>` adds the scheme to `security`, registers it
    /// under `components.securitySchemes`, and adds 401 and 403 — there is no
    /// way to do one without the others, and this asserts the whole bundle
    /// rather than trusting it.
    #[test]
    fn an_authenticated_operation_describes_its_security() {
        let doc = super::document().unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc.to_json().unwrap()).unwrap();
        let op = &v["paths"]["/api/v1/accounts/me"]["get"];

        assert_eq!(op["security"][0]["AccountToken"], serde_json::json!([]));
        assert!(v["components"]["securitySchemes"]["AccountToken"].is_object());
        for status in ["200", "401", "403"] {
            assert!(
                op["responses"].get(status).is_some(),
                "{status} must be described on an authenticated operation"
            );
        }
    }

    /// The device-binding headers must be described under the names they
    /// actually travel as.
    ///
    /// `#[derive(HeaderParams)]` falls back to the field identifier verbatim,
    /// so the first version of `DeviceSig` read a header literally called
    /// `x_sunrise_device` and told clients that was its name. Nothing else
    /// caught it: a server and a test that both use the wrong name agree with
    /// each other. Reading the emitted document is what caught it, so the
    /// document is what pins it.
    #[test]
    fn the_device_binding_headers_are_described_by_their_wire_names() {
        let doc = super::document().unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc.to_json().unwrap()).unwrap();
        let params = v["paths"]["/api/v1/accounts/me"]["get"]["parameters"]
            .as_array()
            .expect("the operation declares header parameters");
        let named: Vec<&str> = params
            .iter()
            .map(|p| p["name"].as_str().unwrap_or_default())
            .collect();

        assert_eq!(
            named,
            vec!["X-Sunrise-Device", "X-Sunrise-Device-Sig", "Date"],
            "header parameters must be described by their wire names"
        );
        assert!(
            params.iter().all(|p| p["in"] == "header"),
            "they are header parameters, not query ones"
        );
    }
}
