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

use std::process::ExitCode;
use std::sync::Arc;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_log::ProtoVersions;
use sunrise_server::{build_router, state::ServerState, OidcVerifier};

/// `EX_CONFIG` from `sysexits.h`: the server was asked to run a configuration
/// it cannot honour. Distinct from a crash so a supervisor does not restart
/// into the same refusal forever.
const EX_CONFIG: u8 = 78;

/// Anything else that stops the server before or during serving.
const EX_FAILURE: u8 = 1;

/// Returns an [`ExitCode`] rather than a `Result` so the config refusals can
/// exit 78. A `Result`-returning `main` reports every error as 1, which would
/// tell a supervisor to restart into the same refusal forever.
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

/// The real entrypoint. `Err(code)` has already been logged.
async fn run() -> Result<(), u8> {
    // First statement in the process: everything below this line can log, and
    // nothing above it needs to.
    // The one place in the workspace where `print_stderr` is the right call:
    // the failure being reported *is* the logger not existing, so there is no
    // other channel. Exiting silently would leave an operator with a bare
    // status code and nothing to read.
    #[allow(clippy::print_stderr)]
    if let Err(e) = sunrise_log::init_stderr() {
        eprintln!("cannot initialize logging: {e}");
        return Err(EX_FAILURE);
    }

    // Argument and config-file handling. A config that cannot be resolved is
    // fatal with EX_CONFIG (78) rather than a generic failure, because an
    // operator's supervisor distinguishes "misconfigured, do not restart me"
    // from "crashed, restart me".
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match sunrise_server::config::load(&args) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(
                ev = "srv.start.refused",
                err_code = "CONFIG_INVALID",
                err_kind = "permanent",
                retryable = false,
                cause = %e,
                "refusing to start"
            );
            return Err(EX_CONFIG);
        }
    };
    let bind = cfg.bind.clone();

    // `ServerState::try_new` installs the self-host `NullVerifier`, which maps
    // every caller to one synthetic account. When the config names an OIDC
    // issuer, the real JWKS-backed verifier replaces it here — that swap is
    // the only difference between self-host and managed at this layer.
    let mut state = match ServerState::try_new(cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                ev = "srv.start.refused",
                err_code = "CONFIG_INVALID",
                err_kind = "permanent",
                retryable = false,
                cause = %e,
                "refusing to start"
            );
            return Err(EX_CONFIG);
        }
    };
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
        return Err(EX_CONFIG);
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

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(ev = "srv.start.failed", bind = %bind, cause = %e, "cannot bind");
            return Err(EX_FAILURE);
        }
    };
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(ev = "srv.stop.failed", cause = %e, "server stopped");
        return Err(EX_FAILURE);
    }

    tracing::info!(ev = "srv.stop", "listener closed");
    Ok(())
}
