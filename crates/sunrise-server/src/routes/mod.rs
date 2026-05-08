//! REST endpoints under `/api/v1/`. Each sub-module pins one resource group.

use crate::state::ServerState;
use axum::Router;

pub mod accounts;
pub mod health;
pub mod meta;

/// Build the v1 router. Routes are namespaced; full set per
/// `spec/06-server/api.md`. v1 ships health/meta/accounts; the rest
/// land in Phase 17 once the engine + storage pipeline are wired.
#[must_use]
pub fn api_v1() -> Router<ServerState> {
    Router::new()
        .merge(health::router())
        .merge(meta::router())
        .merge(accounts::router())
}
