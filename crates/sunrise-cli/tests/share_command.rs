//! `sunrise share` and `sunrise accept` end to end, offline: the real binary
//! against a real vault, as `cli.rs` runs it. What reaches the relay is
//! `sunrise-relay-client`'s and the server's to test; what is tested here is
//! everything decided before a request is made.

use std::path::Path;
use std::process::{Command, Output};

fn keystore(vault: &Path) -> std::path::PathBuf {
    vault.join("keystore")
}

fn run(vault: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sunrise"))
        .args(args)
        .env("SUNRISE_VAULT", vault)
        .env("SUNRISE_KEYSTORE", keystore(vault))
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_SYNC_TOKEN")
        .env_remove("SUNRISE_SYNC_DEVICE_ID")
        .env_remove("SUNRISE_VAULT_ROOT")
        .output()
        .expect("run sunrise")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The share key is this account's, the same on every run, and reads back.
#[test]
fn share_key_prints_this_accounts_key() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["share", "key"]);
    assert!(out.status.success(), "{out:?}");
    let key = stdout(&out).trim().to_owned();
    assert!(
        sunrise_cli::share::decode_share_key(&key).is_some(),
        "{key:?}"
    );
    assert_eq!(stdout(&run(dir.path(), &["share", "key"])).trim(), key);
    assert!(stderr(&out).contains("identity"), "{}", stderr(&out));

    let other = tempfile::tempdir().unwrap();
    assert_ne!(stdout(&run(other.path(), &["share", "key"])).trim(), key);
}

/// With no relay to register the grant on, `share` refuses before it writes:
/// the outbox holds what it held.
#[test]
fn share_without_a_relay_writes_nothing() {
    let owner = tempfile::tempdir().unwrap();
    let recipient = tempfile::tempdir().unwrap();
    assert!(run(owner.path(), &["streams", "create", "Garden"])
        .status
        .success());
    let key = stdout(&run(recipient.path(), &["share", "key"]));
    let before = stdout(&run(owner.path(), &["sync", "status"]));

    let out = run(
        owner.path(),
        &["share", "Garden", key.trim(), "--days", "7"],
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr(&out).contains("SUNRISE_SYNC_URL"),
        "{}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty(), "no link: {}", stdout(&out));
    assert_eq!(stdout(&run(owner.path(), &["sync", "status"])), before);
}

/// A key that is not one, and a length that is not whole days, are refused
/// in the person's words.
#[test]
fn share_refuses_a_bad_key_and_a_bad_length() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path(), &["streams", "create", "Garden"])
        .status
        .success());
    let out = run(dir.path(), &["share", "Garden", "not-a-key"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("share key"), "{}", stderr(&out));

    let key = stdout(&run(dir.path(), &["share", "key"]));
    let out = run(dir.path(), &["share", "Garden", key.trim(), "--days", "0"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--days"), "{}", stderr(&out));

    let out = run(dir.path(), &["share"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("usage"), "{}", stderr(&out));
}

/// `accept` takes a share link and nothing else.
#[test]
fn accept_takes_only_a_share_link() {
    let dir = tempfile::tempdir().unwrap();
    for link in [
        "sunrise://morning",
        "sunrise://share/AAAA",
        "https://example.com",
    ] {
        let out = run(dir.path(), &["accept", link]);
        assert!(!out.status.success(), "{link}: {out:?}");
        assert!(
            stderr(&out).contains("not a sunrise://share link"),
            "{link}: {}",
            stderr(&out)
        );
    }
}
