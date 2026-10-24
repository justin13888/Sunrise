//! `sunrise search` end to end (issue #345): the real binary against a real
//! vault, as `cli.rs` runs it.

use std::path::Path;
use std::process::{Command, Output};

/// A keystore of this test's own, under the vault directory, as `cli.rs`
/// keeps one.
fn keystore(vault: &Path) -> std::path::PathBuf {
    vault.join("keystore")
}

fn run(vault: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sunrise"))
        .args(args)
        .env("SUNRISE_VAULT", vault)
        .env("SUNRISE_KEYSTORE", keystore(vault))
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_VAULT_ROOT")
        .output()
        .expect("run sunrise")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// `search` answers every kind, grouped, with operators, CJK and
/// substrings, and `--json` is the grouped document with each task whole.
#[test]
fn search_groups_every_kind_and_reads_operators() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["streams", "create", "Passports"]);
    run(dir.path(), &["capture", "Renew passport #passports !1"]);
    run(dir.path(), &["capture", "東京出張の準備"]);
    let out = stdout(&run(dir.path(), &["search", "pass"]));
    let tasks = out.find("Tasks (1)").expect("a task group");
    let streams = out.find("Streams (1)").expect("a stream group");
    assert!(tasks < streams, "groups in the fixed order: {out:?}");
    assert!(out.contains("Renew passport"), "{out:?}");
    assert!(stdout(&run(dir.path(), &["search", "東京"])).contains("東京出張の準備"));
    let only = stdout(&run(dir.path(), &["search", "--kind", "stream", "pass"]));
    assert!(!only.contains("Tasks"), "{only:?}");
    let filtered = stdout(&run(dir.path(), &["search", "#passports", "!1"]));
    assert!(filtered.contains("Renew passport"), "{filtered:?}");

    let doc: serde_json::Value =
        serde_json::from_str(stdout(&run(dir.path(), &["search", "--json", "passport"])).trim())
            .unwrap();
    assert_eq!(doc["canonical"], "passport");
    let group = &doc["groups"][0];
    assert_eq!(group["kind"], "task");
    assert_eq!(group["total"], 1);
    let hit = &group["hits"][0];
    assert_eq!(hit["title"]["text"], "Renew passport");
    assert_eq!(hit["title"]["ranges"], serde_json::json!([[6, 14]]));
    assert_eq!(hit["task"]["title"], "Renew passport");
    assert_eq!(hit["id"], hit["task"]["id"]);

    let out = run(dir.path(), &["search", "#nowhere"]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nothing is named “nowhere”"), "{err:?}");
}
