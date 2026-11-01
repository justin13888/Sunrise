use super::*;
use sunrise_cbor::version::STORAGE_V;
use sunrise_error::ErrorCode;

fn key() -> VaultRootKey {
    VaultRootKey::from_bytes([0xab; 32])
}

/// A current vault at `dir/vault.db` holding one stream named `name`.
fn vault_with_stream(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join("vault.db");
    let db = Db::open(&path, &key()).unwrap();
    add_stream(&db, 1, name);
    path
}

fn add_stream(db: &Db, id: u8, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO streams
             (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
             VALUES (?1, X'00', 0, ?2, 0, 0)",
            rusqlite::params![vec![id], name],
        )
        .unwrap();
}

/// The stream names in the vault file at `path`, read without migrating it
/// or touching the copies beside it.
fn stream_names(path: &Path) -> Vec<String> {
    let db = Db::open_unmigrated(path, &key()).unwrap();
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM streams ORDER BY name")
        .unwrap();
    let names = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    names
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_rebuild_copy_sits_beside_the_vault() {
    assert_eq!(
        Db::rebuild_backup_path(Path::new("/v/acct/vault.db")),
        PathBuf::from("/v/acct/vault.db.pre-rebuild.bak")
    );
}

/// "No backup, no rebuild": the copy is a whole vault under the same key,
/// and the next ordinary open, the one that proves the rebuilt vault opens,
/// deletes it.
#[test]
fn a_rebuild_copy_is_a_whole_vault_kept_until_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_with_stream(dir.path(), "Before the rebuild");
    let mut db = Db::open(&path, &key()).unwrap();
    let copy = db.back_up_for_rebuild().unwrap().expect("a file is copied");
    assert_eq!(copy, Db::rebuild_backup_path(&path));
    add_stream(&db, 2, "After the copy");
    drop(db);

    assert_eq!(stream_names(&copy), ["Before the rebuild"]);
    drop(Db::open(&path, &key()).unwrap());
    assert!(!copy.exists(), "the next open deletes the copy");
}

#[test]
fn an_in_memory_vault_has_nothing_to_copy() {
    let mut db = Db::open_memory(&key()).unwrap();
    assert_eq!(db.back_up_for_rebuild().unwrap(), None);
}

#[test]
fn only_the_open_that_creates_a_vault_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    assert!(Db::open(&path, &key()).unwrap().created_by_this_open());
    assert!(!Db::open(&path, &key()).unwrap().created_by_this_open());
}

/// An open that migrated has its copy from before the migration, which is
/// from before the rebuild that follows it too: no second copy is written.
#[test]
fn a_rebuild_after_a_migration_runs_under_the_migrations_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let from_v = u32::from(STORAGE_V) - 1;
    drop(Db::create_at_storage_v(&path, &key(), from_v).unwrap());
    let mut db = Db::open(&path, &key()).unwrap();
    assert_eq!(
        db.back_up_for_rebuild().unwrap(),
        Some(Db::backup_path(&path, from_v))
    );
    assert!(!Db::rebuild_backup_path(&path).exists());
    // A later rebuild in the session copies what the session wrote.
    assert_eq!(
        db.back_up_for_rebuild().unwrap(),
        Some(Db::rebuild_backup_path(&path))
    );
}

#[test]
fn a_rebuild_copy_that_cannot_be_written_is_its_own_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_with_stream(dir.path(), "Kept");
    // A directory where the temporary copy goes, so it cannot be written.
    let tmp = sibling(&Db::rebuild_backup_path(&path), ".tmp");
    fs::create_dir(&tmp).unwrap();
    fs::write(tmp.join("occupant"), b"x").unwrap();
    let mut db = Db::open(&path, &key()).unwrap();
    let err = db.back_up_for_rebuild().unwrap_err();
    assert!(matches!(err, DbError::RebuildBackup { .. }), "{err:?}");
    assert_eq!(err.as_error_code(), ErrorCode::StorageBackupFailed);
}

#[test]
fn the_copies_are_listed_newest_first_and_nothing_else_is() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault.db");
    for name in [
        "vault.db",
        "vault.db.pre-v30.bak",
        "vault.db.pre-v38.bak",
        "vault.db.pre-rebuild.bak",
        "vault.db.pre-rebuild.bak-wal",
        "vault.db.pre-v39.bak.tmp",
        "vault.db.pre-vNOTES.bak",
        "vault.db.replaced-1",
        "other.db.pre-v31.bak",
    ] {
        fs::write(dir.path().join(name), b"x").unwrap();
    }
    let listed: Vec<(String, BackupKind)> = Db::backups(&vault)
        .into_iter()
        .map(|b| {
            (
                b.path.file_name().unwrap().to_string_lossy().into_owned(),
                b.kind,
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (
                "vault.db.pre-rebuild.bak".to_owned(),
                BackupKind::PreRebuild
            ),
            (
                "vault.db.pre-v38.bak".to_owned(),
                BackupKind::PreMigration { from_v: 38 }
            ),
            (
                "vault.db.pre-v30.bak".to_owned(),
                BackupKind::PreMigration { from_v: 30 }
            ),
        ]
    );
    assert!(Db::backups(&dir.path().join("missing").join("vault.db")).is_empty());
}

#[test]
fn every_kind_of_copy_and_its_leftovers_count_as_a_copy() {
    for name in [
        "vault.db.pre-v7.bak",
        "vault.db.pre-v7.bak.tmp",
        "vault.db.pre-rebuild.bak",
        "vault.db.pre-rebuild.bak.tmp",
        "vault.db.pre-rebuild.bak-wal",
        "vault.db.pre-rebuild.bak-wal.tmp",
    ] {
        assert!(is_backup_file("vault.db", name), "{name}");
    }
    for name in [
        "vault.db",
        "vault.db-wal",
        "vault.db.pre-v.bak",
        "vault.db.pre-vx.bak",
        "vault.db.replaced-1",
        "other.db.pre-rebuild.bak",
    ] {
        assert!(!is_backup_file("vault.db", name), "{name}");
    }
}

/// The restored copy is the vault; what it replaced is moved aside with its
/// log, never deleted.
#[test]
fn restoring_a_copy_puts_it_in_place_and_keeps_what_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_with_stream(dir.path(), "In the copy");
    let mut db = Db::open(&path, &key()).unwrap();
    let copy = db.back_up_for_rebuild().unwrap().unwrap();
    add_stream(&db, 2, "Only in the vault");
    drop(db);

    let aside = Db::restore_backup(&path, &copy).unwrap().expect("moved");
    assert_eq!(aside, dir.path().join("vault.db.replaced-1"));
    assert!(!copy.exists(), "the copy is now the vault");
    assert_eq!(stream_names(&path), ["In the copy"]);
    assert_eq!(stream_names(&aside), ["In the copy", "Only in the vault"]);
    // The restored vault opens as any other.
    drop(Db::open(&path, &key()).unwrap());

    // A second restore picks the next free name.
    let mut db = Db::open(&path, &key()).unwrap();
    let copy = db.back_up_for_rebuild().unwrap().unwrap();
    drop(db);
    assert_eq!(
        Db::restore_backup(&path, &copy).unwrap(),
        Some(dir.path().join("vault.db.replaced-2"))
    );
}

#[test]
fn only_a_copy_of_this_vault_is_restored() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_with_stream(dir.path(), "Untouched");
    let stranger = dir.path().join("elsewhere.db");
    fs::write(&stranger, b"not a copy").unwrap();
    let err = Db::restore_backup(&path, &stranger).unwrap_err();
    assert!(matches!(err, DbError::Restore { .. }), "{err:?}");
    assert_eq!(stream_names(&path), ["Untouched"]);
    assert!(stranger.exists());
}

#[test]
fn a_vault_directory_is_moved_aside_whole() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("vault");
    fs::create_dir(&dir).unwrap();
    vault_with_stream(&dir, "Kept aside");
    let aside = Db::set_vault_dir_aside(&dir).unwrap();
    assert_eq!(aside, root.path().join("vault.set-aside-1"));
    assert!(!dir.exists());
    assert_eq!(stream_names(&aside.join("vault.db")), ["Kept aside"]);
}

/// Put a row in a table of the vault at `path` that breaks the table's
/// declared `NOT NULL`, which `quick_check` reports: damage a rebuild of the
/// table clears.
fn damage(path: &Path) {
    let db = Db::open_unmigrated(path, &key()).unwrap();
    db.conn()
        .execute_batch(
            "CREATE TABLE damaged (a INTEGER);
             INSERT INTO damaged VALUES (1), (NULL);
             PRAGMA writable_schema = ON;
             UPDATE sqlite_schema SET sql = 'CREATE TABLE damaged (a INTEGER NOT NULL)'
               WHERE name = 'damaged';
             PRAGMA writable_schema = RESET;",
        )
        .unwrap();
}

/// A vault the open refuses is opened for repair under a byte-for-byte copy;
/// its check still fails until what is damaged is cleared.
#[test]
fn a_damaged_vault_opens_for_repair_under_a_copy_of_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_with_stream(dir.path(), "Survives the repair");
    damage(&path);
    let err = Db::open(&path, &key()).unwrap_err();
    assert!(
        matches!(err, DbError::IntegrityCheckFailed { .. }),
        "{err:?}"
    );
    assert_eq!(err.as_error_code(), ErrorCode::StorageIntegrityCheckFailed);
    let before = fs::read(&path).unwrap();

    let mut db = Db::open_for_repair(&path, &key()).unwrap();
    let copy = Db::rebuild_backup_path(&path);
    assert_eq!(fs::read(&copy).unwrap(), before, "copied as it was");
    assert_eq!(db.back_up_for_rebuild().unwrap(), Some(copy.clone()));
    assert!(matches!(
        db.check_integrity(),
        Err(DbError::IntegrityCheckFailed { .. })
    ));
    // What a rebuild does to a projection table: clear it whole. (A
    // `WHERE a IS NULL` would match nothing, the column being declared
    // `NOT NULL`.)
    db.conn().execute("DELETE FROM damaged", []).unwrap();
    db.check_integrity().unwrap();
    drop(db);
    assert!(copy.exists(), "a repair keeps its copy");
    drop(Db::open(&path, &key()).unwrap());
    assert_eq!(stream_names(&path), ["Survives the repair"]);
}

/// A repair whose vault is behind is checked before it migrates: a failed
/// check must not start a migration (rule 3).
#[test]
fn a_damaged_vault_behind_this_build_is_not_migrated_by_a_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let from_v = u32::from(STORAGE_V) - 1;
    drop(Db::create_at_storage_v(&path, &key(), from_v).unwrap());
    damage(&path);
    let err = Db::open_for_repair(&path, &key()).unwrap_err();
    assert!(
        matches!(err, DbError::IntegrityCheckFailed { .. }),
        "{err:?}"
    );
    let raw = Db::open_unmigrated(&path, &key()).unwrap();
    let v: u32 = raw
        .conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, from_v);
}

#[test]
fn there_is_nothing_to_repair_where_there_is_no_vault() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let err = Db::open_for_repair(&path, &key()).unwrap_err();
    assert!(matches!(err, DbError::Sqlite(_)), "{err:?}");
    assert!(names_in(dir.path()).is_empty(), "nothing is created");
}

#[test]
fn each_open_failure_a_client_branches_on_has_its_own_code() {
    let io = || std::io::Error::other("disk full");
    assert_eq!(
        DbError::Backup {
            from_v: 3,
            source: io()
        }
        .as_error_code(),
        ErrorCode::StorageBackupFailed
    );
    assert_eq!(
        DbError::IntegrityCheckFailed {
            detail: String::new()
        }
        .as_error_code(),
        ErrorCode::StorageIntegrityCheckFailed
    );
    assert_eq!(
        DbError::Migration {
            id: 3,
            name: "x",
            source: rusqlite::Error::InvalidQuery
        }
        .as_error_code(),
        ErrorCode::StorageMigrationFailed
    );
    assert_eq!(
        DbError::Sqlite(rusqlite::Error::InvalidQuery).as_error_code(),
        ErrorCode::FatalInternal,
        "a wrong key is not damage"
    );
}

// --- pre-migration backup (§Migration rigor rule 2) -------------------

/// The version one migration behind this build, so every open below has
/// exactly one migration to run.
fn previous_v() -> u32 {
    u32::from(STORAGE_V) - 1
}

/// A real vault file at [`previous_v`], holding one stream, closed.
fn vault_one_behind(dir: &Path) -> PathBuf {
    let path = dir.join("vault.db");
    let db = Db::create_at_storage_v(&path, &key(), previous_v()).unwrap();
    db.conn()
        .execute(
            "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
                 VALUES (X'01', X'00', 0, 'Kept in the copy', 0, 0)",
            [],
        )
        .unwrap();
    path
}

fn stamp(db: &Db) -> u32 {
    db.conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn the_backup_path_sits_beside_the_vault_and_names_the_version() {
    assert_eq!(
        Db::backup_path(Path::new("/v/acct/vault.db"), 27),
        PathBuf::from("/v/acct/vault.db.pre-v27.bak")
    );
}

/// The copy is a whole vault at the version it was taken from, under the
/// same SQLCipher key, and the live vault moves on without it.
#[test]
fn the_backup_is_encrypted_under_the_vaults_own_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_one_behind(dir.path());

    let db = Db::open(&path, &key()).unwrap();
    assert_eq!(stamp(&db), u32::from(STORAGE_V));
    drop(db);

    let backup = Db::backup_path(&path, previous_v());
    let bytes = fs::read(&backup).expect("the open must leave a copy");
    assert!(
        !bytes.starts_with(b"SQLite format 3\0"),
        "the copy must be encrypted like the vault it copies"
    );
    assert!(
        !bytes
            .windows(b"Kept in the copy".len())
            .any(|w| w == b"Kept in the copy"),
        "the copy holds content in the clear"
    );

    let copy = Db::open_unmigrated(&backup, &key()).unwrap();
    assert_eq!(stamp(&copy), previous_v(), "the copy is the old version");
    let name: String = copy
        .conn()
        .query_row("SELECT name FROM streams", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "Kept in the copy");
    drop(copy);

    let read_with_another_key = Db::open_unmigrated(&backup, &VaultRootKey::from_bytes([0; 32]))
        .ok()
        .and_then(|c| {
            c.conn()
                .query_row("SELECT storage_v FROM schema_meta", [], |r| {
                    r.get::<_, u32>(0)
                })
                .ok()
        });
    assert!(
        read_with_another_key.is_none(),
        "another key must not read the copy"
    );
}

/// Kept through the migrating open, deleted by the next one — the first
/// open to prove the migrated vault usable. A half-written copy a crash
/// left goes with it; a file that only resembles one does not.
#[test]
fn the_backup_is_kept_until_the_next_open_at_the_new_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_one_behind(dir.path());
    drop(Db::open(&path, &key()).unwrap());
    let backup = Db::backup_path(&path, previous_v());
    assert!(backup.exists(), "kept after the migrating open");

    let stale_tmp = dir.path().join("vault.db.pre-v3.bak.tmp");
    let unrelated = dir.path().join("vault.db.pre-vNOTES.bak");
    let other_vault = dir.path().join("other.db.pre-v3.bak");
    for f in [&stale_tmp, &unrelated, &other_vault] {
        fs::write(f, b"x").unwrap();
    }

    drop(Db::open(&path, &key()).unwrap());
    assert!(!backup.exists(), "removed by the next successful open");
    assert!(!stale_tmp.exists(), "a crash's half-written copy goes too");
    assert!(unrelated.exists(), "only `.pre-v<digits>.bak` is a copy");
    assert!(
        other_vault.exists(),
        "another vault's copy is not this one's"
    );
}

/// No backup, no migration: when the copy cannot be written the open
/// fails with a typed error and the vault is still at its old version.
#[test]
fn a_vault_that_cannot_be_backed_up_is_not_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = vault_one_behind(dir.path());
    // A directory where the temporary copy goes: removing it fails, and
    // so would writing it. Portable, and unaffected by running as root,
    // which a read-only directory is not.
    let mut tmp = Db::backup_path(&path, previous_v()).into_os_string();
    tmp.push(".tmp");
    fs::create_dir(&tmp).unwrap();
    fs::write(PathBuf::from(&tmp).join("occupant"), b"x").unwrap();

    let err = Db::open(&path, &key()).unwrap_err();
    assert!(
        matches!(err, DbError::Backup { from_v, .. } if from_v == previous_v()),
        "expected a backup failure, got {err:?}"
    );
    let raw = Db::open_unmigrated(&path, &key()).unwrap();
    assert_eq!(
        stamp(&raw),
        previous_v(),
        "the vault must be left untouched"
    );
}

/// A fresh vault and an in-memory one have nothing to copy.
#[test]
fn a_fresh_vault_is_not_backed_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    drop(Db::open(&path, &key()).unwrap());
    let names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".pre-v"))
        .collect();
    assert!(names.is_empty(), "unexpected copies: {names:?}");
}
