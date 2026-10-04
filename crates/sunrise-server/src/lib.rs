//! Sunrise sync relay server library surface.
//!
//! Implements the foundation of `docs/06-server/`. It ships:
//!
//! - REST endpoints under `/api/v1/`: account and device lifecycle, blob 2PC,
//!   meta, health.
//! - The sync surface under `/api/v1/sync/`, an SSE stream downstream and typed
//!   `POST`s upstream per
//!   [ADR-0023](../../../docs/11-adr/0023-sse-sync-transport.md):
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
pub mod drain;
pub mod healthcheck;
pub mod logging;
pub mod metrics;
pub mod push;
pub mod relay;
pub mod relay_log;
pub mod state;
pub mod store;
pub mod sync_session;

#[cfg(test)]
mod serve_tests;

pub use api::error::ApiError;
pub use auth::oidc::{OidcConfig, OidcVerifier};
pub use auth::{AuthError, NullVerifier, StaticVerifier, Subject, TokenVerifier, Verified};
pub use config::ServerConfig;
pub use logging::{account_h, id_h};
pub use metrics::Metrics;
pub use push::{
    ApnsProvider, Dispatcher, PushIntent, PushPlatform, PushProvider, PushTokenRegistration,
};
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
    api::router(&config, &state.metrics).build(state)
}

/// Serve the typed surface on `listener` until the process ends.
///
/// [`serve_until`] with a trigger that never fires, for the tests and
/// embedders that end the server by dropping its task.
///
/// # Errors
/// As [`serve_until`].
pub async fn serve(state: ServerState, listener: tokio::net::TcpListener) -> kynos::Result<()> {
    serve_until(state, listener, std::future::pending()).await
}

/// Serve the typed surface on `listener` until `shutdown` resolves, then drain.
///
/// kynos owns the accept loop and the drain. An earlier design in `api/mod.rs`
/// described a hand-written dispatcher that would hand `/sync` to axum and
/// everything else to kynos; ADR-0023 removed the need for it by making
/// `/sync` describable, so there is one stack and no dispatcher.
///
/// When `shutdown` resolves, in this order:
///
/// 1. [`ServerState::drain`] is set, so `GET /api/v1/health?deep=1` answers
///    `503` and every open SSE stream sends a retryable `closed` event and
///    ends (`api::sync::stream`).
/// 2. kynos stops accepting and waits up to
///    [`ServerConfig::shutdown_grace_secs`] for in-flight requests.
/// 3. The store's write-ahead log is checkpointed into the database file, so
///    the data directory a stopped relay leaves behind is one file.
///
/// The binary passes its `SIGTERM`/`SIGINT` listener as `shutdown`; a test
/// passes any future it controls.
///
/// # Errors
/// Returns kynos's error if the surface cannot be built, the listener fails
/// terminally, or in-flight requests outlast the drain deadline.
pub async fn serve_until(
    state: ServerState,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> kynos::Result<()> {
    let grace = state.config.shutdown_grace();
    let drain = state.drain.clone();
    let store = std::sync::Arc::clone(&state.store);
    let trigger = {
        let drain = drain.clone();
        async move {
            shutdown.await;
            tracing::info!(
                ev = "srv.stop.draining",
                n_streams = drain.open_streams() as u64,
                // The longest the drain may take, in the allowlist's one
                // duration field.
                delay_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
                "shutdown requested; draining"
            );
            drain.begin();
        }
    };
    let served = kynos::server::Server::new(build_service(state)?)
        .listener(listener)
        .graceful_shutdown(kynos::server::shutdown::Shutdown::on(trigger))
        .shutdown_timeout(grace)
        .serve()
        .await;
    // Only after a drain: a listener that failed outright still leaves the
    // store to the process exit, which closes it the same way.
    if drain.is_draining() {
        let flushed = checkpoint(&store);
        tracing::info!(
            ev = "srv.stop",
            result = match &served {
                Ok(()) => "drained",
                Err(kynos::Error::Server(kynos::server::error::ServerError::ShutdownTimeout {
                    ..
                })) => "timed_out",
                Err(_) => "failed",
            },
            n_streams = drain.open_streams() as u64,
            status = if flushed.is_ok() {
                "store_flushed"
            } else {
                "store_flush_failed"
            },
            cause = flushed.as_ref().err().map(tracing::field::display),
            "drained and stopped"
        );
    }
    served
}

/// Fold the write-ahead log back into the database file.
///
/// Every acknowledged frame is already durable in the WAL, so this loses
/// nothing if it fails; what it buys is a data directory that is one file once
/// the relay stops, which is what `self-hosting.md` tells an operator to back
/// up. `TRUNCATE` rather than `PASSIVE` because no request is left to contend
/// with it.
fn checkpoint(store: &store::Store) -> rusqlite::Result<()> {
    store
        .conn
        .lock()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
}
