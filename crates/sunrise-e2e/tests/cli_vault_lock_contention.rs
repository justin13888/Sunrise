//! Two `sunrise` processes on one vault (issue #756).
//!
//! `sunrise sync --watch` holds the vault for as long as it runs. A second
//! command against the same vault meanwhile has to fail and say who holds
//! it, with the way out, rather than wait or corrupt anything; once the
//! holder is gone the same command goes through. The vault lock's own tests
//! cover the lock in one process and `sunrise-cli`'s cover repeated runs one
//! after another; only two live processes reach the message a user reads.

use std::io::{BufRead as _, BufReader};
use std::process::Child;
use std::sync::mpsc;

use sunrise_cli::i18n::strings;
use sunrise_core::VaultLockError;
use sunrise_e2e::cli::{first_id, sign_in, Device, WAIT};
use sunrise_e2e::spawn_relay_unlimited;

/// The holder, killed when the test ends however it ends, so a failed
/// assertion leaves no `sunrise` running.
struct Holder(Child);

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_on_a_vault_held_by_sync_watch_is_told_who_holds_it() {
    let (addr, relay) = spawn_relay_unlimited().await;
    let dir = tempfile::tempdir().unwrap();
    let a = Device::new(&dir.path().join("a"), &format!("http://{addr}"));
    sign_in(&a.vault);
    a.ok(&["bootstrap", "a@example.com"]).await;

    let mut holder = Holder(a.spawn(&["sync", "--watch"]));
    // Every line it prints, read to the end: a holder whose stdout closed
    // would fail its next print and exit, releasing the vault early.
    let stdout = holder.0.stdout.take().expect("stdout is piped");
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // Live means it has opened the vault, and holds it until it exits.
    let live = strings::sync::live();
    let lines = tokio::task::spawn_blocking(move || loop {
        match lines.recv_timeout(WAIT) {
            Ok(line) if line.contains(&live) => return lines,
            Ok(_) => {}
            Err(e) => panic!("`sync --watch` never went live: {e}"),
        }
    })
    .await
    .expect("join the reader");

    let out = a.run(&["capture", "Call the plumber"]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a held vault opened: {stderr}");
    // The core's own words, naming the holder's pid; the time it started is
    // the holder's to know.
    let held = VaultLockError::AlreadyHeld {
        holder_pid: holder.0.id(),
        holder_started_at: String::new(),
    }
    .to_string();
    assert!(stderr.contains(&held), "{held:?} not in: {stderr}");
    assert!(
        stderr.contains(&strings::open::try_again()),
        "the way out is not offered: {stderr}"
    );

    // Gone, even without the chance to shut down: the lock goes with the
    // process, so the next command opens the vault.
    drop(holder);
    drop(lines);
    let task = first_id(&a.ok(&["capture", "Call the plumber"]).await);
    assert_eq!(a.task(&task).await["title"], "Call the plumber");

    relay.abort();
}
