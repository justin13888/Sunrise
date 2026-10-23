//! `sync` and `rebuild`: the vault's plumbing.

use sunrise_cli::i18n::strings;
use sunrise_core::Core;

use super::CmdResult;

/// `rebuild` — rebuild the vault's projection from its local op log
/// (issue #441).
///
/// Offline, like every verb but `sync` and `attachment`: it reads the log
/// this vault already holds and sends nothing. The whole rebuild is one
/// transaction, so a failure leaves the vault as it was.
pub(super) fn rebuild(core: &Core, rest: &[String]) -> CmdResult {
    if !rest.is_empty() {
        return Err(strings::rebuild::usage().into());
    }
    let report = core.rebuild_projection()?;
    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    println!("{}", strings::rebuild::done(count(report.ops_replayed)));
    if report.ops_skipped > 0 {
        println!("{}", strings::rebuild::skipped(count(report.ops_skipped)));
    }
    Ok(())
}

/// How long `sync --once` waits for the outbox to empty before giving up.
const DEADLINE_MS: u64 = 30_000;

/// `sync [--once] | sync --watch [--rounds <n>]`.
///
/// `--once` (the default) is named for cron and CI in
/// `docs/07-clients/overview.md`. It is a *bounded* wait, not a loop: a job
/// that hangs forever because the relay is down is worse than one that fails,
/// since nothing downstream ever runs. `--watch` is the loop, for a terminal
/// or a service manager; see [`sunrise_cli::watch`].
pub(super) async fn sync(ctx: &super::Ctx<'_>, rest: &[String]) -> CmdResult {
    let core = ctx.core;
    if rest.first().is_some_and(|a| a == "status") {
        return status(ctx).await;
    }
    let watch = rest.first().is_some_and(|a| a == "--watch");
    let rounds = super::flag_value(rest, "--rounds");
    let shape_ok = match rest {
        [] => true,
        [once] => once == "--once" || once == "--watch",
        [w, r, _] => w == "--watch" && r == "--rounds",
        _ => false,
    };
    if !shape_ok {
        return Err(strings::sync::usage().into());
    }
    let Ok(url) = std::env::var(sunrise_cli::livesync::ENV_SYNC_URL) else {
        return Err(strings::sync::needs_url().into());
    };
    sunrise_cli::livesync::relay_origin(&url)?;
    if !watch {
        return once(core).await;
    }
    let rounds = match rounds {
        Some(n) => Some(
            n.parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(strings::sync::usage)?,
        ),
        None => None,
    };
    watch_loop(core, rounds).await
}

/// The badge for this vault's sync state, as the client layer reads it
/// ([`sunrise_client_core::sync_status::sync_badge`]).
async fn badge(core: &Core) -> CmdResult<sunrise_client_core::sync_status::SyncBadge> {
    match core.query(sunrise_core::Query::SyncStatus).await? {
        sunrise_core::QueryResult::SyncStatus(s) => Ok(
            sunrise_client_core::sync_status::sync_badge(s.state, s.outbox_pending),
        ),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// The badge kind as a stable code.
const fn badge_code(kind: sunrise_client_core::sync_status::SyncBadgeKind) -> &'static str {
    use sunrise_client_core::sync_status::SyncBadgeKind as K;
    match kind {
        K::Checking => "checking",
        K::Synced => "synced",
        K::Sending => "sending",
        K::CatchingUp => "catching_up",
        K::Offline => "offline",
        K::ChangesMissing => "changes_missing",
        K::Stopped => "stopped",
    }
}

/// `sync status` — this vault's sync badge and outbox depth, offline or not.
async fn status(ctx: &super::Ctx<'_>) -> CmdResult {
    let b = badge(ctx.core).await?;
    if ctx.json {
        sunrise_cli::json::print(&serde_json::json!({
            "badge": badge_code(b.kind),
            "pending": b.pending,
            "known_incomplete": b.known_incomplete,
        }));
    } else {
        println!(
            "{}",
            strings::sync::status(badge_code(b.kind), i64::from(b.pending))
        );
    }
    Ok(())
}

/// Resolves on Ctrl-C, and on `SIGTERM` where there is one, so a service
/// manager's stop is as clean as a person's.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// `sync --watch`: rounds of the bounded wait until stopped.
async fn watch_loop(core: &Core, rounds: Option<u32>) -> CmdResult {
    use sunrise_cli::watch::{run, Backoff, End, Event, Round};
    let end = run(
        Backoff::default(),
        || async {
            match sunrise_client_core::sync::wait_until_caught_up(core, DEADLINE_MS, &mut |_| {})
                .await
            {
                Ok(Ok(())) => Round::Live,
                // The badge says what kind of not-live it is: offline
                // retries on its own, stopped and changes-missing do not.
                Ok(Err(_)) => Round::Failed(match badge(core).await {
                    Ok(b) => strings::sync::not_live(badge_code(b.kind), i64::from(b.pending)),
                    Err(e) => e.to_string(),
                }),
                Err(e) => Round::Failed(e.to_string()),
            }
        },
        tokio::time::sleep,
        stop_signal(),
        rounds,
        &mut |e| match e {
            Event::Live => println!("{}", strings::sync::live()),
            Event::Failed { reason, retry_in } => eprintln!(
                "{}",
                strings::sync::retrying(
                    reason,
                    i64::try_from(retry_in.as_secs()).unwrap_or(i64::MAX)
                )
            ),
        },
    )
    .await;
    if end == End::Stopped {
        eprintln!("{}", strings::sync::stopped());
    }
    Ok(())
}

/// One bounded drain, printing each change in the outbox depth.
async fn once(core: &Core) -> CmdResult {
    // Timed against the core's injected clock rather than `Instant`, which
    // the workspace lint bans so that time is never read from two sources.
    let mut last = u32::MAX;
    let waited = sunrise_client_core::sync::wait_until_caught_up(core, DEADLINE_MS, &mut |s| {
        let caught_up = s.outbox_pending == 0 && s.state == sunrise_sync::SyncState::Live;
        if !caught_up && s.outbox_pending != last {
            last = s.outbox_pending;
            println!(
                "{}",
                strings::sync::pending(i64::from(s.outbox_pending), &format!("{:?}", s.state))
            );
        }
    })
    .await?;
    match waited {
        Ok(()) => {
            println!("{}", strings::sync::live());
            Ok(())
        }
        Err(_) => Err(strings::sync::timed_out(
            i64::try_from(DEADLINE_MS / 1000).unwrap_or(i64::MAX),
        )
        .into()),
    }
}
