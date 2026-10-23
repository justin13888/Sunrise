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

/// `sync --once` — bring the relay up, drain the outbox, exit.
///
/// Named for cron and CI in `docs/07-clients/overview.md`. It is a *bounded*
/// wait, not a loop: a job that hangs forever because the relay is down is
/// worse than one that fails, since nothing downstream ever runs.
pub(super) async fn sync(core: &Core, rest: &[String]) -> CmdResult {
    if !rest.is_empty() && rest[0] != "--once" {
        return Err(strings::sync::usage().into());
    }
    if std::env::var(sunrise_cli::livesync::ENV_SYNC_URL).is_err() {
        return Err(strings::sync::needs_url().into());
    }
    once(core).await
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
