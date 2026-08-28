//! Sunrise sync relay server library surface.
//!
//! Implements the foundation of `docs/06-server/`. v1 ships:
//!
//! - REST endpoints under `/api/v1/`: account scaffolding, meta, health.
//! - Self-host single-binary mode (`./sunrise-server -c sunrise.toml`).
//! - WebSocket endpoint at `/sync` (handshake stub; OpBatch fanout
//!   ships in Phase 17 once the engine in `sunrise-core` is wired).
//! - OIDC token validation: the [`auth::TokenVerifier`] seam plus
//!   [`auth::oidc::OidcVerifier`], a JWKS-backed implementation.
//! - Account + device persistence in SQLite ([`store`]).
//!
//! The library is testable in isolation: [`build_router`] returns an
//! `axum::Router` that integration tests can drive via tower::ServiceExt.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::double_must_use,
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::match_same_arms,
    clippy::needless_pass_by_value,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::unused_async,
    clippy::missing_panics_doc
)]

pub mod auth;
pub mod config;
pub mod error;
pub mod logging;
pub mod metrics;
pub mod push;
pub mod relay;
pub mod relay_log;
pub mod routes;
pub mod state;
pub mod store;
pub mod ws;

pub use auth::oidc::{OidcConfig, OidcVerifier};
pub use auth::{AuthError, NullVerifier, StaticVerifier, Subject, TokenVerifier, Verified};
pub use config::ServerConfig;
pub use error::ApiError;
pub use logging::{account_h, id_h};
pub use metrics::Metrics;
pub use push::{LoggingProvider, PushIntent, PushPlatform, PushProvider, PushRegistration};
pub use relay::RelayHub;
pub use state::{Clock, ServerState, SystemClock};
pub use store::{Account, Device, Store, StoreError};

use axum::Router;

/// Build the public Axum router with all v1 routes wired.
///
/// `tower-http` was a declared dependency with no import anywhere, so the
/// relay ran with no CORS policy, no request-size limit, and no trace layer.
/// All three are mounted here now:
///
/// - **CORS** is an exact-match allowlist from `ServerConfig::allowed_origins`,
///   empty by default. Origins are never reflected back, and `*` is rejected at
///   config validation — a wildcard origin paired with credentials is what made
///   the v0 API reachable from any page.
/// - **Body limit** caps request bodies so an unauthenticated POST cannot pin
///   memory. The relay's own frames ride the WebSocket and are bounded
///   separately by the wire protocol's frame cap.
/// - **Trace** gives request spans. It must never log the `?access_token=`
///   query parameter that browsers use in place of an `Authorization` header,
///   so it is [`logging::trace_layer`] rather than `TraceLayer::new_for_http`
///   with stock callbacks — the stock `MakeSpan` records the full URI.
#[must_use]
pub fn build_router(state: ServerState) -> Router {
    let origins: Vec<axum::http::HeaderValue> = state
        .config
        .allowed_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    let cors = if origins.is_empty() {
        // No browser client is configured: deny every cross-origin request
        // rather than defaulting to permissive.
        tower_http::cors::CorsLayer::new()
    } else {
        tower_http::cors::CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
            ])
    };
    let max_body = state.config.max_body_bytes;

    Router::new()
        .nest("/api/v1", routes::api_v1())
        .merge(ws::router())
        .merge(metrics::router())
        .layer(cors)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(max_body))
        .layer(logging::trace_layer())
        .with_state(state)
}
