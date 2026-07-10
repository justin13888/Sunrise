//! Sunrise sync relay server library surface.
//!
//! Implements the foundation of `docs/06-server/`. v1 ships:
//!
//! - REST endpoints under `/api/v1/`: account scaffolding, meta, health.
//! - Self-host single-binary mode (`./sunrise-server -c sunrise.toml`).
//! - WebSocket endpoint at `/sync` (handshake stub; OpBatch fanout
//!   ships in Phase 17 once the engine in `sunrise-core` is wired).
//! - OIDC token validation interface (verifier trait; production binds
//!   to a JWKS HTTP fetch).
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
pub mod metrics;
pub mod push;
pub mod relay;
pub mod routes;
pub mod state;
pub mod ws;

pub use auth::{AuthError, NullVerifier, StaticVerifier, Subject, TokenVerifier};
pub use config::ServerConfig;
pub use metrics::Metrics;
pub use push::{LoggingProvider, PushIntent, PushPlatform, PushProvider, PushRegistration};
pub use relay::RelayHub;
pub use state::{Clock, ServerState, SystemClock};

use axum::Router;

/// Build the public Axum router with all v1 routes wired.
#[must_use]
pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .nest("/api/v1", routes::api_v1())
        .merge(ws::router())
        .merge(metrics::router())
        .with_state(state)
}
