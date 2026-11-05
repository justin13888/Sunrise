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
    assert!(!run(dir.path(), &["rebuild", "--repair", "--now"])
        .status
        .success());
    let help = stdout(&run(dir.path(), &["help"]));
    assert!(help.contains("sunrise rebuild --repair"), "{help}");
    assert!(help.contains("sunrise backups"), "{help}");
}

/// The root every process below opens the vault with, so the test can key
/// the same file.
const ROOT: [u8; 32] = [0x5a; 32];

fn run_rooted(vault: &Path, args: &[&str]) -> Output {
    let hex = "5a".repeat(ROOT.len());
    Command::new(env!("CARGO_BIN_EXE_sunrise"))
        .args(args)
        .env("SUNRISE_VAULT", vault)
        .env("SUNRISE_KEYSTORE", vault.join("keystore"))
        .env("SUNRISE_VAULT_ROOT", hex)
        .env_remove("SUNRISE_SYNC_URL")
        .output()
        .expect("run sunrise")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Break `task_blockers`, a projection table, with a row its `NOT NULL`
/// refuses, which the integrity check reports: written through a schema with
/// the constraint taken off, then put back.
fn damage(vault: &Path) {
    let db = sunrise_storage::Db::open(
        &vault.join("vault.db"),
        &sunrise_crypto::keys::VaultRootKey::from_bytes(ROOT),
    )
    .unwrap();
    let sql: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name = 'task_blockers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let loose = sql.replace("blocker_id  BLOB NOT NULL", "blocker_id  BLOB");
    assert_ne!(loose, sql);
    let set_schema = |to: &str| {
        db.conn()
            .execute_batch("PRAGMA writable_schema = ON")
            .unwrap();
        db.conn()
            .execute(
                "UPDATE sqlite_schema SET sql = ?1 WHERE name = 'task_blockers'",
                [to],
            )
            .unwrap();
        db.conn()
            .execute_batch("PRAGMA writable_schema = RESET")
            .unwrap();
    };
    set_schema(&loose);
    db.conn()
        .execute(
            "INSERT INTO task_blockers (task_id, blocker_id) VALUES (X'01', NULL)",
            [],
        )
        .unwrap();
    set_schema(&sql);
}

/// A damaged vault is refused with what to do about it; `rebuild --repair`
/// mends it from its own op log under a copy of the damaged file, which
/// `backups` lists and `backups restore` puts back.
#[test]
fn a_damaged_vault_is_explained_repaired_and_its_copy_restored() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run_rooted(dir.path(), &["capture", "Survives the damage"])
        .status
        .success());
    damage(dir.path());

    let refused = run_rooted(dir.path(), &["inbox"]);
    assert!(!refused.status.success());
    let why = stderr(&refused);
    assert!(why.contains("integrity check failed"), "{why}");
    assert!(why.contains("sunrise rebuild --repair"), "{why}");

    let repaired = run_rooted(dir.path(), &["rebuild", "--repair"]);
    assert!(repaired.status.success(), "{repaired:?}");
    assert!(
        stdout(&repaired).starts_with("repaired the vault from"),
        "{repaired:?}"
    );

    // Listed before an ordinary open, which deletes the copy.
    let listed = stdout(&run_rooted(dir.path(), &["backups"]));
    let copy = listed
        .split_whitespace()
        .next()
        .expect("the repair's copy")
        .to_owned();
    assert!(copy.ends_with("vault.db.pre-rebuild.bak"), "{listed}");
    assert!(listed.contains("taken before a rebuild or a repair"));

    // Putting the copy back puts the damage back: it is the file as it was.
    let restored = run_rooted(dir.path(), &["backups", "restore", &copy]);
    assert!(restored.status.success(), "{restored:?}");
    assert!(
        stdout(&restored).contains("vault.db.replaced-1"),
        "{restored:?}"
    );
    assert!(!run_rooted(dir.path(), &["inbox"]).status.success());

    assert!(run_rooted(dir.path(), &["rebuild", "--repair"])
        .status
        .success());
    let inbox = stdout(&run_rooted(dir.path(), &["inbox"]));
    assert!(inbox.contains("Survives the damage"), "{inbox}");
    assert_eq!(stdout(&run_rooted(dir.path(), &["backups"])), "");
}

#[test]
fn there_is_nothing_to_repair_in_an_empty_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["rebuild", "--repair"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("there is no vault in"), "{out:?}");
    assert!(!dir.path().join("keystore").exists(), "no root was minted");
}
