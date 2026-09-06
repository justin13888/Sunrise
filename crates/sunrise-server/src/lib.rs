//! Sunrise sync relay server library surface.
//!
//! Implements the foundation of `docs/06-server/`. v1 ships:
//!
//! - REST endpoints under `/api/v1/`: account and device lifecycle, blob 2PC,
//!   meta, health.
//! - The sync surface under `/api/v1/sync/`, an SSE stream downstream and typed
//!   `POST`s upstream per
//!   [ADR-0023](../../../../docs/11-adr/0023-sse-sync-transport.md):
//!   `POST /sync/session` negotiates, `POST /sync/subscribe` declares the
//!   streams, `GET /sync/events` fans out with `Last-Event-ID` resumption,
//!   `POST /sync/ops` takes a batch and answers with an `Ack`, and
//!   `POST /sync/session/refresh` renews the credential. There is no WebSocket:
//!   it and `/sync`'s handshake frames went with ADR-0023, and fan-out,
//!   cursor-filtered replay and the typed cursor gap all ship. See
//!   [`api::sync`].
//! - Self-host single-binary mode (`./sunrise-server -c sunrise.toml`).
//! - OIDC token validation: the [`auth::TokenVerifier`] seam plus
//!   [`auth::oidc::OidcVerifier`], a JWKS-backed implementation.
//! - Account + device persistence in SQLite ([`store`]).
//!
//! The library is testable in isolation: [`build_service`] returns the built
//! `kynos` [`Service`] that integration tests drive in process.

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

pub mod api;
pub mod auth;
pub mod config;
pub mod logging;
pub mod metrics;
pub mod push;
pub mod relay;
pub mod relay_log;
pub mod state;
pub mod store;
pub mod sync_session;

pub use api::error::ApiError;
pub use auth::oidc::{OidcConfig, OidcVerifier};
pub use auth::{AuthError, NullVerifier, StaticVerifier, Subject, TokenVerifier, Verified};
pub use config::ServerConfig;
pub use logging::{account_h, id_h};
pub use metrics::Metrics;
pub use push::{LoggingProvider, PushIntent, PushPlatform, PushProvider, PushRegistration};
pub use relay::RelayHub;
pub use state::{Clock, ServerState, SystemClock};
pub use store::{Account, Device, Store, StoreError};
pub use sync_session::{Session, SessionStore};

use kynos::router::service::Service;

/// Build the typed surface over `state`, ready to serve.
///
/// The one place the router and the context meet. `build` is where kynos runs
/// its structural checks, so an API that cannot be described correctly fails
/// here — at startup — rather than at documentation time.
///
/// # Errors
/// Returns kynos's error naming every violation found.
pub fn build_service(state: ServerState) -> kynos::Result<Service<ServerState>> {
    let config = ServerConfig::clone(&state.config);
    api::router(&config).build(state)
}

/// Serve the typed surface on `listener` until the process ends.
///
/// kynos owns the accept loop. An earlier design in `api/mod.rs` described a
/// hand-written dispatcher that would hand `/sync` to axum and everything else
/// to kynos; ADR-0023 removed the need for it by making `/sync` describable, so
/// there is one stack and no dispatcher.
///
/// # Errors
/// Returns kynos's error if the surface cannot be built or the listener fails
/// terminally.
pub async fn serve(state: ServerState, listener: tokio::net::TcpListener) -> kynos::Result<()> {
    kynos::server::Server::new(build_service(state)?)
        .listener(listener)
        .serve()
        .await
}
