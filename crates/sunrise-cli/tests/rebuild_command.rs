//! `sunrise rebuild`, run as the real binary against a real vault (issue
//! #441): the projection is rebuilt from the vault's own op log, in a
//! separate process, and a later process reads the same tasks back.

use std::path::Path;
use std::process::{Command, Output};

fn run(vault: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sunrise"))
        .args(args)
        .env("SUNRISE_VAULT", vault)
        // A keystore of this test's own, so nothing reaches the developer's.
        .env("SUNRISE_KEYSTORE", vault.join("keystore"))
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_VAULT_ROOT")
        .output()
        .expect("run sunrise")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn rebuild_rebuilds_from_the_log_and_every_task_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let id = stdout(&run(dir.path(), &["capture", "Renew passport !1"]))
        .split_whitespace()
        .next()
        .expect("capture prints the new id")
        .to_owned();
    run(dir.path(), &["capture", "Water the plants"]);
    assert!(run(dir.path(), &["done", &id]).status.success());
    let before = stdout(&run(dir.path(), &["inbox"]));
    let streams_before = stdout(&run(dir.path(), &["streams"]));

    let out = run(dir.path(), &["rebuild"]);
    assert!(out.status.success(), "rebuild failed: {out:?}");
    let report = stdout(&out);
    assert!(
        report.starts_with("rebuilt the projection from 3 ops"),
        "two creates and a completion: {report:?}"
    );

    assert_eq!(stdout(&run(dir.path(), &["inbox"])), before);
    assert_eq!(stdout(&run(dir.path(), &["streams"])), streams_before);
    let search = stdout(&run(dir.path(), &["search", "plants"]));
    assert!(
        search.contains("Water the plants"),
        "the search index is rebuilt too: {search:?}"
    );
}

#[test]
fn rebuild_takes_no_arguments_and_is_in_the_usage_block() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!run(dir.path(), &["rebuild", "--now"]).status.success());
    assert!(stdout(&run(dir.path(), &["help"])).contains("sunrise rebuild"));
}
