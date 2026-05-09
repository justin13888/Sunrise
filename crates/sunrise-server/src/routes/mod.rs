//! REST endpoints under `/api/v1/`. Each sub-module pins one resource group.

use crate::state::ServerState;
use axum::Router;

pub mod accounts;
pub mod blobs;
pub mod devices;
pub mod health;
pub mod meta;

/// Build the v1 router. Routes are namespaced; full set per
/// `spec/06-server/api.md`. v1 ships health/meta/accounts/blobs/devices;
/// per-account quota and full OIDC-gated CRUD ride on Phase 17.
#[must_use]
pub fn api_v1() -> Router<ServerState> {
    Router::new()
        .merge(health::router())
        .merge(meta::router())
        .merge(accounts::router())
        .merge(blobs::router())
        .merge(devices::router())
}
