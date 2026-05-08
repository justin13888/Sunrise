//! Sunrise sync relay server library surface.
//!
//! Implements the foundation of `spec/06-server/`. v1 ships:
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
    clippy::double_must_use
)]

pub mod config;
pub mod routes;
pub mod state;

pub use config::ServerConfig;
pub use state::ServerState;

use axum::Router;

/// Build the public Axum router with all v1 routes wired.
#[must_use]
pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .nest("/api/v1", routes::api_v1())
        .with_state(state)
}
