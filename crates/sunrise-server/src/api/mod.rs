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

pub mod health;
pub mod meta;

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
}

#[cfg(test)]
mod tests {
    /// The document is the contract, so its existence is a test rather than a
    /// build artefact nobody looks at. `openapi()` runs the structural checks,
    /// which is what makes "an API that cannot be described correctly fails at
    /// startup" true rather than aspirational.
    #[test]
    fn the_router_describes_itself() {
        let doc = super::document().expect("the router must describe");
        let json = doc.to_json().expect("the description must serialize");
        assert!(
            json.contains("/api/v1/health") && json.contains("/api/v1/meta"),
            "every ported operation must appear in the document: {json}"
        );
        assert!(
            json.contains("\"openapi\": \"3.2"),
            "the document must be emitted AS 3.2, not at whatever minimum \
             expresses today's API -- `itemSchema` is what ADR-0023's event \
             stream needs and 3.1 has no way to say it: {json}"
        );
    }
}
