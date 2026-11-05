#![allow(clippy::doc_markdown)]

//! The copies a rebuild runs under, the repair of a vault the integrity
//! check refuses, and restoring a copy (issue #441,
//! `docs/04-storage/migrations.md` §Migration rigor rules 2 to 4), through
//! the public `Core` API against real vault files.

use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use sunrise_core::{
    Clock, Command, Core, CoreConfig, CoreError, Query, QueryResult, SystemRng, Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::TaskDraft;
use sunrise_storage::{BackupKind, Db, DbError};

#[derive(Debug)]
struct FakeClock {
    ms: Mutex<u64>,
}
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.ms.lock()
    }
}

const ROOT: [u8; 32] = [3u8; 32];

fn cfg(dir: &Path) -> CoreConfig {
    CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+repair",
        Arc::new(FakeClock {
            ms: Mutex::new(1_700_000_000_000),
        }),
        Arc::new(SystemRng),
    )
}

fn unlock() -> Unlock {
    Unlock::DevicePaired {
        root: VaultRootKey::from_bytes(ROOT),
        paired: None,
    }
}

async fn open(dir: &Path) -> Result<Core, CoreError> {
    Core::open(cfg(dir), unlock()).await
}

async fn add_task(core: &Core, title: &str) {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .unwrap();
}

async fn inbox_titles(core: &Core) -> Vec<String> {
    let QueryResult::StreamTasks(tasks) = core.query(Query::Inbox).await.unwrap() else {
        panic!("expected tasks");
    };
    let mut titles: Vec<String> = tasks.into_iter().map(|t| t.title).collect();
    titles.sort();
    titles
}

/// Break `task_blockers`, a projection table, the way a damaged page breaks
/// a table: a row `quick_check` reports, here a `NULL` its `NOT NULL`
/// column refuses. Written through a schema with the constraint taken off
/// and then put back.
fn damage_a_projection_table(dir: &Path) {
    let db = Db::open(&dir.join("vault.db"), &VaultRootKey::from_bytes(ROOT)).unwrap();
    let sql: String = db
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name = 'task_blockers'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let loose = sql.replace("blocker_id  BLOB NOT NULL", "blocker_id  BLOB");
    assert_ne!(loose, sql, "the column's declaration moved: {sql}");
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

/// A rebuild asked for by hand runs under a copy of the vault from before
/// it, which the next open deletes.
#[tokio::test]
async fn a_rebuild_leaves_a_copy_of_the_vault_until_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await.unwrap();
    add_task(&core, "copied").await;
    core.rebuild_projection().unwrap();
    let copies = Core::vault_backups(dir.path());
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0].kind, BackupKind::PreRebuild);
    core.close().await.unwrap();

    let core = open(dir.path()).await.unwrap();
    assert!(Core::vault_backups(dir.path()).is_empty());
    assert_eq!(inbox_titles(&core).await, ["copied"]);
}

/// The open refuses a damaged projection with its own error; the repair
/// rebuilds it from the op log, and the vault then opens as any other.
#[tokio::test]
async fn a_damaged_projection_is_refused_and_then_repaired_from_the_op_log() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await.unwrap();
    add_task(&core, "kept by the op log").await;
    add_task(&core, "and this one").await;
    core.close().await.unwrap();
    damage_a_projection_table(dir.path());

    let err = open(dir.path()).await.unwrap_err();
    assert!(
        matches!(err, CoreError::Db(DbError::IntegrityCheckFailed { .. })),
        "{err:?}"
    );

    let (core, report) = Core::open_repairing(cfg(dir.path()), unlock())
        .await
        .unwrap();
    assert!(report.ops_replayed >= 2, "{report:?}");
    assert_eq!(
        inbox_titles(&core).await,
        ["and this one", "kept by the op log"]
    );
    let copies = Core::vault_backups(dir.path());
    assert_eq!(copies.len(), 1, "the repair keeps its copy: {copies:?}");
    core.close().await.unwrap();

    let core = open(dir.path()).await.unwrap();
    assert_eq!(inbox_titles(&core).await.len(), 2);
    assert!(Core::vault_backups(dir.path()).is_empty());
}

/// A copy is restored only while nothing has the vault open; the vault it
/// replaces is moved aside, not deleted.
#[tokio::test]
async fn a_copy_is_restored_only_into_a_closed_vault() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await.unwrap();
    add_task(&core, "in the copy").await;
    core.rebuild_projection().unwrap();
    add_task(&core, "after the copy").await;
    let copy = Core::vault_backups(dir.path()).remove(0);

    let err = Core::restore_vault_backup(&cfg(dir.path()), &copy.path).unwrap_err();
    assert!(matches!(err, CoreError::VaultLock(_)), "{err:?}");
    core.close().await.unwrap();

    let aside = Core::restore_vault_backup(&cfg(dir.path()), &copy.path)
        .unwrap()
        .expect("the vault it replaced");
    assert!(aside.exists());
    let core = open(dir.path()).await.unwrap();
    assert_eq!(inbox_titles(&core).await, ["in the copy"]);
}

#[tokio::test]
async fn a_vault_is_set_aside_whole_and_only_when_closed() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("vault");
    let core = open(&dir).await.unwrap();
    add_task(&core, "set aside").await;
    assert!(matches!(
        Core::set_vault_aside(&cfg(&dir)),
        Err(CoreError::VaultLock(_))
    ));
    core.close().await.unwrap();

    let aside = Core::set_vault_aside(&cfg(&dir)).unwrap();
    assert!(!dir.exists());
    let core = open(&aside).await.unwrap();
    assert_eq!(inbox_titles(&core).await, ["set aside"]);
}

/// The rebuild an open runs when the fold changed runs under a copy too:
/// here forced by forgetting the fold the projection was built at.
#[tokio::test]
async fn the_rebuild_at_open_runs_under_a_copy() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await.unwrap();
    add_task(&core, "refolded at open").await;
    core.close().await.unwrap();
    assert!(
        Core::vault_backups(dir.path()).is_empty(),
        "a new vault's first rebuild has nothing to copy"
    );
    let db = Db::open(
        &dir.path().join("vault.db"),
        &VaultRootKey::from_bytes(ROOT),
    )
    .unwrap();
    db.conn()
        .execute("DELETE FROM projection_rebuild", [])
        .unwrap();
    drop(db);

    let core = open(dir.path()).await.unwrap();
    let copies = Core::vault_backups(dir.path());
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0].kind, BackupKind::PreRebuild);
    assert_eq!(inbox_titles(&core).await, ["refolded at open"]);
}
