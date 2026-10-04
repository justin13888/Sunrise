//! `sunrise-server` self-host single-binary entrypoint.
//!
//! ```text
//! ./sunrise-server -c sunrise.toml
//! ```
//!
//! Self-host scope: the typed REST surface plus ADR-0023's SSE sync.
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
use sunrise_server::{state::ServerState, OidcVerifier};

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "healthcheck" => healthcheck(rest).await,
        Some((cmd, rest)) if cmd == "admin" => admin(rest),
        _ => run(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

/// `sunrise-server healthcheck [--deep] [-c <path>]`: probe the configured
/// listener and exit 0 if it answered `200`, 1 otherwise.
///
/// Resolves the config exactly as the server does — `-c`, then
/// `$SUNRISE_CONFIG`, then the implicit paths — so it probes the address the
/// server bound. 1 for every failure, a config refusal included, because
/// Docker reads 0 as healthy, 1 as unhealthy, and reserves the rest.
///
/// The one line it writes goes to stderr as plain text rather than through the
/// NDJSON logger: its reader is `docker inspect`'s health log, not an ingest
/// pipeline, and a healthy probe says nothing.
#[allow(clippy::print_stderr)]
async fn healthcheck(args: &[String]) -> Result<(), u8> {
    let deep = args.iter().any(|a| a == "--deep");
    let rest: Vec<String> = args.iter().filter(|a| *a != "--deep").cloned().collect();
    let cfg = match sunrise_server::config::load(&rest) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("healthcheck: {e}");
            return Err(EX_FAILURE);
        }
    };
    sunrise_server::healthcheck::probe(&cfg.bind, deep)
        .await
        .map_err(|e| {
            eprintln!("healthcheck: {e}");
            EX_FAILURE
        })
}

/// `sunrise-server admin [-c <path>] [--json] <command>`: the operator CLI,
/// on the data dir directly. `sunrise_server::admin::cli` documents the
/// commands and why there is no admin socket.
///
/// Its output is the command's answer, for a person or a script, so it goes to
/// stdout as text or JSON rather than through the NDJSON logger.
fn admin(args: &[String]) -> Result<(), u8> {
    let code = sunrise_server::admin::cli::run(
        args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    if code == 0 {
        Ok(())
    } else {
        Err(code)
    }
}

/// Run the maintenance pass every `[storage] maintenance_interval_secs`, the
/// first one at startup.
///
/// On a blocking thread, because every step is database or filesystem I/O, and
/// one pass at a time, because the next tick waits for this one to finish.
fn spawn_maintenance(state: ServerState) {
    let every = std::time::Duration::from_secs(state.config.maintenance_interval_secs);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let state = state.clone();
            let pass = tokio::task::spawn_blocking(move || {
                sunrise_server::admin::maintenance::run(&state, state.clock.now_ms(), false)
            })
            .await;
            match pass {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    ev = "srv.maintenance.failed",
                    reason = "pass",
                    cause = %e,
                    "maintenance pass failed; retrying at the next interval"
                ),
                Err(e) => tracing::warn!(
                    ev = "srv.maintenance.failed",
                    reason = "pass",
                    cause = %e,
                    "maintenance pass did not complete; retrying at the next interval"
                ),
            }
        }
    });
}

/// Resolves on the first `SIGTERM` or `SIGINT`, then forces on the second.
///
/// Installed before the listener binds, so a signal that arrives during
/// startup is a shutdown rather than the default abrupt exit. The first signal
/// drains; the second sends on `force`, which abandons the drain.
fn shutdown_signals(
    force: tokio::sync::oneshot::Sender<()>,
) -> std::io::Result<impl std::future::Future<Output = ()> + Send + 'static> {
    let (first_tx, first_rx) = tokio::sync::oneshot::channel::<()>();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::spawn(async move {
            for tx in [first_tx, force] {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
                let _ = tx.send(());
            }
        });
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        for tx in [first_tx, force] {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(());
        }
    });
    Ok(async move {
        // A listener task that died without sending is not a request to stop.
        if first_rx.await.is_err() {
            std::future::pending::<()>().await;
        }
    })
}

/// The real entrypoint. `Err(code)` has already been logged.
async fn run(args: Vec<String>) -> Result<(), u8> {
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
    // Once, here: an operator wondering why phones never wake reads it at the
    // top of the log rather than inferring it from silence.
    if state.push.provider().is_none() {
        tracing::info!(
            ev = "srv.push.disabled",
            "no [push] provider is configured; devices without an open stream sync on their own schedule"
        );
    }

    spawn_maintenance(state.clone());
    serve_until_signalled(state, &bind).await
}

/// Bind `bind`, serve until `SIGTERM`/`SIGINT`, and drain.
///
/// `srv.stop` is logged by `serve_until` once the drain ends, with what it
/// drained; only failures are reported here.
async fn serve_until_signalled(state: ServerState, bind: &str) -> Result<(), u8> {
    let (force_tx, mut force_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown = match shutdown_signals(force_tx) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                ev = "srv.start.failed",
                bind = %bind,
                cause = %e,
                "cannot install the shutdown signal handlers"
            );
            return Err(EX_FAILURE);
        }
    };
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(ev = "srv.start.failed", bind = %bind, cause = %e, "cannot bind");
            return Err(EX_FAILURE);
        }
    };
    let served = tokio::select! {
        served = sunrise_server::serve_until(state, listener, shutdown) => served,
        // A second signal: the operator wants it gone now. Dropping the server
        // future aborts its accept loops, and returning ends the runtime and
        // every connection with it.
        Ok(()) = &mut force_rx => {
            tracing::error!(
                ev = "srv.stop.failed",
                err_kind = "user",
                cause = "a second shutdown signal abandoned the drain",
                "server stopped"
            );
            return Err(EX_FAILURE);
        }
    };
    if let Err(e) = served {
        tracing::error!(
            ev = "srv.stop.failed",
            err_kind = "permanent",
            cause = %e,
            "server stopped"
        );
        return Err(EX_FAILURE);
    }
    Ok(())
}
