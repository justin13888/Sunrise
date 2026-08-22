//! `sunrise-server` self-host single-binary entrypoint.
//!
//! ```text
//! ./sunrise-server -c sunrise.toml
//! ```
//!
//! v1 self-host scope: REST endpoints + (deferred) WebSocket relay.
//! Configuration via TOML; defaults bind 127.0.0.1:8443.
//!
//! Logging is installed first, before anything that could want to log. A
//! server's output is something an ingest pipeline parses, so it is NDJSON on
//! stderr; `SUNRISE_LOG` tunes verbosity and `SUNRISE_LOG_FORMAT=pretty`
//! switches to a human line in dev builds. See
//! `docs/10-cross-cutting/logging.md` §10.

use std::sync::Arc;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_log::ProtoVersions;
use sunrise_server::{build_router, config::ServerConfig, state::ServerState, OidcVerifier};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // First statement in the process: everything below this line can log, and
    // nothing above it needs to.
    sunrise_log::init_stderr()?;

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
    if let Err(e) = state.config.validate(single_tenant) {
        tracing::error!(
            ev = "srv.start.refused",
            err_code = "CONFIG_INVALID",
            err_kind = "permanent",
            retryable = false,
            cause = %e,
            "refusing to start"
        );
        return Err(e.into());
    }

    let app = build_router(state);

    // The protocol versions ride the startup line rather than every record —
    // see `sunrise_log::proto` for why that trade was made. Both binaries
    // report them through the same struct so the two startup records have the
    // same shape.
    let proto = ProtoVersions::new(WIRE_PROTO_V, DOC_SCHEMA_V, CRYPTO_SUITE_V);
    tracing::info!(
        ev = "srv.start",
        bind = %bind,
        mode = if single_tenant { "single_tenant" } else { "multi_tenant" },
        app_v = env!("CARGO_PKG_VERSION"),
        wire_v = u64::from(proto.wire),
        doc_v = u64::from(proto.doc),
        crypto_v = u64::from(proto.crypto),
        "sunrise-server listening"
    );
    if single_tenant {
        tracing::warn!(
            ev = "srv.start.single_tenant",
            mode = "single_tenant",
            "every connection maps to one account; loopback only, configure an OIDC issuer for multi-user use"
        );
    }

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;

    tracing::info!(ev = "srv.stop", "listener closed");
    Ok(())
}
