//! `sunrise-server` self-host single-binary entrypoint.
//!
//! ```text
//! ./sunrise-server -c sunrise.toml
//! ```
//!
//! v1 self-host scope: REST endpoints + (deferred) WebSocket relay.
//! Configuration via TOML; defaults bind 127.0.0.1:8443.

// Binary may print startup banner on stderr — sanctioned for the
// self-host CLI entry. Production deployments use the structured
// logging surface for everything else (Phase 17 wires it up).
#![allow(clippy::print_stderr)]

use std::sync::Arc;
use sunrise_server::{build_router, config::ServerConfig, state::ServerState, OidcVerifier};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = ServerConfig::default();
    let bind = cfg.bind.clone();

    // `ServerState::try_new` installs the self-host `NullVerifier`, which maps
    // every caller to one synthetic account. When the config names an OIDC
    // issuer, the real JWKS-backed verifier replaces it here — that swap is
    // the only difference between self-host and managed at this layer.
    let mut state = ServerState::try_new(cfg)?;
    if let Some(oidc) = OidcVerifier::from_server_config(&state.config, state.clock.clone()) {
        state = state.with_verifier(Arc::new(oidc));
    }
    // Validating against the *installed* verifier is the point: binding a
    // single-tenant verifier to a non-loopback address publishes one shared
    // namespace to the network, so we refuse to start rather than serving and
    // leaking.
    let single_tenant = state.is_single_tenant();
    state.config.validate(single_tenant)?;

    let app = build_router(state);

    eprintln!("sunrise-server listening on {bind}");
    if single_tenant {
        eprintln!(
            "sunrise-server: SELF-HOST MODE — every connection maps to one account. \
             Loopback only; configure an OIDC issuer for multi-user use."
        );
    }
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
