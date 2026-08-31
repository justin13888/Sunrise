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
pub mod blobs;
pub mod devices;
pub mod error;
pub mod health;
pub mod meta;
pub mod metrics;
pub mod observe;
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
    // The defaults, because the *shape* of the description does not vary with
    // them: `BodySize` contributes 413 to every operation whatever the limit
    // is, and an undocumented `Cors` contributes nothing either way. What would
    // vary is the numbers, and no number appears in the document.
    router(&crate::ServerConfig::default()).openapi_as(kynos::openapi::SpecVersion::V3_2)
}

/// The router's type, interceptor stack included.
///
/// kynos carries the mounted interceptors in the router's type so that two of
/// them claiming the same status is a compile error rather than a surprise at
/// build time. The cost is that the stack has to be named here; the benefit is
/// that adding a second body limit would not compile.
pub type ApiRouter = kynos::Router<
    ServerState,
    kynos::middleware::catch_panic::Propagate,
    kynos::middleware::stack::Cons<
        kynos::middleware::cors::Cors,
        kynos::middleware::stack::Cons<kynos::middleware::limits::BodySize, ()>,
    >,
>;

/// Build the typed router for every ported operation.
///
/// # Errors
/// Returns kynos's build error when the router cannot be described — a route
/// whose operations conflict, or a handler whose types do not resolve.
pub fn router(config: &crate::ServerConfig) -> ApiRouter {
    kynos::Router::<ServerState>::new()
        // Named, because the description is about to become a published
        // artefact that `spargen` reads: kynos's default `Info` is
        // `"API" 0.0.0`, and a client generated from that is a client called
        // `API` claiming to speak version zero of it.
        .info(kynos::openapi::Info::new(
            "Sunrise relay",
            env!("CARGO_PKG_VERSION"),
        ))
        .mount(kynos::routes![health::health])
        .mount(kynos::routes![meta::meta])
        .mount(kynos::routes![accounts::create, accounts::me])
        .mount(kynos::routes![
            devices::list,
            devices::register,
            devices::revoke,
            devices::push_tokens
        ])
        .mount(kynos::routes![
            blobs::init,
            blobs::finalize,
            blobs::put_chunk,
            blobs::fetch
        ])
        .merge(operator_surface(config))
        // Configuring the limit and documenting that a limit exists are one
        // action here: `BodySize` contributes 413 to every operation it covers,
        // so an API cannot quietly reject payloads it claims to accept.
        .intercept(kynos::middleware::limits::BodySize::new(
            config.max_body_bytes as u64,
        ))
        .intercept(cors(config))
        .observe(observe::RequestLog)
}

/// `/metrics`, mounted only where the documented contract allows serving it.
///
/// An empty router on a non-loopback bind, so the operation is absent from the
/// description as well as from the router: the document does not advertise a
/// surface this deployment refuses to serve.
fn operator_surface(config: &crate::ServerConfig) -> kynos::Router<ServerState> {
    if crate::config::binds_loopback(&config.bind) {
        kynos::Router::<ServerState>::new().mount(kynos::routes![metrics::metrics])
    } else {
        tracing::warn!(
            ev = "srv.start.metrics_withheld",
            bind = %config.bind,
            "/metrics is not mounted: the listener is not loopback"
        );
        kynos::Router::<ServerState>::new()
    }
}

/// The CORS policy, as an exact-match allowlist.
///
/// Origins are never reflected back, and `*` is rejected at config validation:
/// a wildcard origin paired with credentials is what made the v0 API reachable
/// from any page. kynos refuses that combination at build time as well, so the
/// rule is now enforced twice by two components that cannot disagree about it.
fn cors(config: &crate::ServerConfig) -> kynos::middleware::cors::Cors {
    let mut policy = kynos::middleware::cors::Cors::new()
        .allow_methods([kynos::openapi::Method::Get, kynos::openapi::Method::Post])
        .allow_headers(["authorization", "content-type"]);
    if !config.allowed_origins.is_empty() {
        policy = policy.allow_origins(config.allowed_origins.clone());
    }
    policy
}

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::ServerConfig;
    use kynos::http::{Method, StatusCode};

    /// A body over the limit is refused, and the description says so.
    ///
    /// Both halves matter: `tower-http`'s `RequestBodyLimitLayer` enforced a cap
    /// no document mentioned, so the API rejected payloads it claimed to
    /// accept. `BodySize` contributes the 413 it enforces.
    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_and_described() {
        let client = Client::new(ServerConfig {
            max_body_bytes: 128,
            ..ServerConfig::default()
        });

        let res = client
            .send(
                Method::POST,
                "/api/v1/devices",
                Some(&serde_json::json!({
                    "device_pub_s": "x",
                    "nickname": "y".repeat(4096),
                    "platform": "linux",
                })),
            )
            .await;
        res.assert_status(StatusCode::PAYLOAD_TOO_LARGE);

        let doc = super::document().unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc.to_json().unwrap()).unwrap();
        assert!(
            v["paths"]["/api/v1/devices"]["post"]["responses"]
                .get("413")
                .is_some(),
            "the limit must be described where it is enforced"
        );
    }

    /// The default is to deny every cross-origin request rather than to reflect
    /// whatever asked. A wildcard origin paired with credentials is what made
    /// the v0 API reachable from any page.
    #[tokio::test]
    async fn no_configured_origin_means_no_cross_origin_access() {
        let client = Client::new(ServerConfig::default());
        let res = client
            .send_with(
                Method::GET,
                "/api/v1/health",
                None,
                None,
                &[("origin", "https://evil.example")],
            )
            .await;

        assert!(
            !String::from_utf8_lossy(&res.bytes).contains("evil.example"),
            "an origin must never be reflected into the body"
        );
        res.assert_status(StatusCode::OK);
    }

    /// The committed description is the one the handlers produce.
    ///
    /// `spargen` generates the client from a *file*, so that file is an input to
    /// something rather than a report about it: a stale one produces a client
    /// that disagrees with the server and compiles perfectly while doing so.
    /// This is the check that makes regenerating it non-optional.
    #[test]
    fn the_committed_description_is_current() {
        let committed = include_str!("../../../../schemas/openapi.v1.json");
        let generated = format!(
            "{}\n",
            super::document()
                .expect("the router must describe at 3.2")
                .to_json()
                .expect("the description must serialize")
        );
        assert_eq!(
            committed, generated,
            "schemas/openapi.v1.json is stale; regenerate it with \
             `just openapi` (cargo run -p sunrise-server --bin openapi)"
        );
    }

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
