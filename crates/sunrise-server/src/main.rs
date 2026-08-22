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

use sunrise_server::{build_router, config::ServerConfig, state::ServerState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = ServerConfig::default();
    let bind = cfg.bind.clone();

    // `ServerState::new` installs the self-host `NullVerifier`, which maps
    // every caller to one synthetic account. Validating against that fact is
    // the point: binding a single-tenant verifier to a non-loopback address
    // publishes one shared namespace to the network, so we refuse to start
    // rather than serving and leaking.
    let state = ServerState::new(cfg);
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
