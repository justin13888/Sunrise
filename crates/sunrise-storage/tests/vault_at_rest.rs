//! What a caller of `sunrise-storage` depends on and cannot check for itself.
//!
//! The unit tests in `src/db.rs` reach `Db::ensure_schema`, `Db::apply_pragmas`
//! and `MIGRATIONS[n].sql` directly, and most of them run against
//! `Db::open_memory`. Everything here is a dependent crate's view: one
//! `Db::open` against a real path, and then either the public API or the raw
//! bytes on disk. Nothing in this file names a private item, and nothing here
//! could be written any other way — the properties are about the *file*, and
//! an in-memory database does not have one.
//!
//! Four promises are pinned, all of them made by `docs/04-storage/`:
//!
//! 1. A wrong vault root **fails**, and does not present as an empty vault
//!    (`overview.md` §Why encrypted-at-rest, `local-database.md`).
//! 2. The file is not a readable `SQLite` database, and vault content is not in
//!    its bytes (`overview.md`: "`SQLCipher` provides whole-database
//!    encryption").
//! 3. Re-opening a current vault applies no migration — the caller-visible
//!    form of `migrations.md` §Local DB migrations' "re-running is a no-op".
//! 4. `STORAGE_V` behaves at its two boundaries: one past this build is
//!    refused as too new, one below the baseline is refused as pre-baseline
//!    (`migrations.md`, ADR-0018).

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_error::ErrorCode;
use sunrise_storage::migrations::BASELINE_STORAGE_V;
use sunrise_storage::{Db, DbError, STORAGE_V};

/// A string a caller would recognize in a hex dump. Written into a `TEXT`
/// column, so if anything is stored in the clear it is stored as these bytes.
const CANARY: &str = "sunrise-plaintext-canary-4f19c7d2";

fn root(seed: u8) -> VaultRootKey {
    VaultRootKey::from_bytes([seed; 32])
}

/// Insert one `streams` row through the public connection handle.
fn insert_stream(db: &Db, id: u8, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO streams
             (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
             VALUES (?, X'00', 0, ?, 0, 0)",
            rusqlite::params![vec![id; 16], name],
        )
        .expect("insert a stream");
}

fn stream_names(db: &Db) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM streams ORDER BY name")
        .expect("prepare");
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    rows
}

fn stamped_storage_v(db: &Db) -> u32 {
    db.conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
        .expect("read schema_meta")
}

/// Every object `SQLite` knows about, rendered deterministically.
///
/// Compared across re-opens: it changes if a single `ALTER TABLE` or
/// `CREATE INDEX` runs a second time, which is what "applying a migration
/// twice" would look like from here if it did not simply fail.
fn schema_fingerprint(db: &Db) -> String {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master
             ORDER BY type, name",
        )
        .expect("prepare");
    let rows: Vec<String> = stmt
        .query_map([], |r| {
            Ok(format!(
                "{}\t{}\t{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            ))
        })
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    rows.join("\n")
}

/// A current vault whose version stamp has then been rewritten to `storage_v`,
/// closed and ready to be re-opened.
///
/// The only way to build a vault at some other `STORAGE_V` without reaching
/// inside the crate: `Db::conn` is public and `schema_meta` is part of the
/// schema `docs/04-storage/migrations.md` documents, so this is a manoeuvre a
/// dependent crate can perform. Each call gets its own directory, because a
/// vault stamped outside the accepted range cannot be re-opened to be stamped
/// back.
fn vault_stamped_at(dir: &Path, key: &VaultRootKey, storage_v: u32) -> std::path::PathBuf {
    let path = dir.join("vault.db");
    let db = Db::open(&path, key).expect("create the vault");
    db.conn()
        .execute(
            "UPDATE schema_meta SET storage_v = ?",
            rusqlite::params![storage_v],
        )
        .expect("restamp");
    drop(db);
    path
}

/// Every byte the vault occupies: the database and any WAL / shm sidecar.
fn vault_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).expect("read vault dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = fs::read(entry.path()).expect("read vault file");
        out.push((name, bytes));
    }
    assert!(!out.is_empty(), "the vault directory must hold a file");
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The property the whole at-rest story rests on: a wrong key is an **error**,
/// not an empty vault.
///
/// The distinction is the point. `Db::open` creates and migrates a database
/// that has no `schema_meta`, so a keying layer that silently handed back a
/// blank page instead of failing would produce a fresh, empty, perfectly valid
/// vault — and a caller that treats "opened" as "unlocked" would then show the
/// user an account with nothing in it and start writing over the real one. So
/// this asserts three things in sequence: the wrong key fails, the failed
/// attempt left the file alone, and the right key still finds every row.
#[test]
fn a_wrong_vault_root_is_refused_and_leaves_the_real_rows_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vault.db");

    {
        let db = Db::open(&path, &root(0xab)).expect("create the vault");
        insert_stream(&db, 1, "Work");
        insert_stream(&db, 2, "Home");
    }

    // A key that differs from the real one in a single bit.
    let mut near_miss = [0xab_u8; 32];
    near_miss[31] ^= 0x01;
    let err = Db::open(&path, &VaultRootKey::from_bytes(near_miss))
        .expect_err("a wrong vault root must not open the vault");
    assert!(
        matches!(err, DbError::Sqlite(_)),
        "expected the keying layer to refuse, got {err:?}"
    );

    // ...and it must not have "opened" onto a blank vault either, which is the
    // failure mode this test exists for: had the open succeeded, it would have
    // migrated a fresh schema and reported zero streams.
    let reopened = Db::open(&path, &root(0xab)).expect("the right key still opens it");
    assert_eq!(
        stream_names(&reopened),
        vec!["Home".to_string(), "Work".to_string()],
        "the failed attempt must not have replaced or emptied the vault"
    );
    assert_eq!(stamped_storage_v(&reopened), u32::from(STORAGE_V));
}

/// The file is not a `SQLite` database to anyone without the key, and the
/// content written through the public API is not in its bytes.
///
/// The plain control database is not decoration: it is what makes this test
/// able to fail. It is written by the same `rusqlite` this crate uses, holds
/// the same canary in the same column of a table by the same name, and the
/// assertions on it are the exact negations of the assertions on the vault. If
/// the search were broken, or the canary never actually reached storage, the
/// control half fails first and says so.
#[test]
fn the_vault_file_is_not_plain_sqlite_and_does_not_hold_its_content_in_the_clear() {
    const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

    let dir = tempfile::tempdir().expect("tempdir");
    let vault_dir = dir.path().join("vault");
    fs::create_dir(&vault_dir).expect("mkdir");
    let path = vault_dir.join("vault.db");
    {
        let db = Db::open(&path, &root(0x5a)).expect("create the vault");
        insert_stream(&db, 7, CANARY);
    }

    // The control: the same row, the same library, no key.
    let plain_path = dir.path().join("plain.db");
    {
        let conn = Connection::open(&plain_path).expect("open a plain db");
        conn.execute_batch("CREATE TABLE streams (name TEXT NOT NULL);")
            .expect("create");
        conn.execute(
            "INSERT INTO streams (name) VALUES (?)",
            rusqlite::params![CANARY],
        )
        .expect("insert");
    }
    let plain = fs::read(&plain_path).expect("read the plain db");
    assert!(
        plain.starts_with(SQLITE_MAGIC),
        "the control must be a plain SQLite file, or this test proves nothing"
    );
    assert!(
        contains(&plain, CANARY.as_bytes()),
        "the control must hold the canary in the clear, or the search is broken"
    );

    for (name, bytes) in vault_bytes(&vault_dir) {
        assert!(
            !bytes.starts_with(SQLITE_MAGIC),
            "`{name}` starts with the SQLite header; the vault is not encrypted"
        );
        assert!(
            !contains(&bytes, CANARY.as_bytes()),
            "`{name}` holds vault content in the clear"
        );
    }

    // And the strongest form of the same statement: the file is not openable
    // as a database at all without the key, so an attacker with the file has
    // no query surface to work from.
    let unkeyed = Connection::open(&path).expect("sqlite opens any path lazily");
    let probe: Result<i64, _> = unkeyed.query_row("SELECT count(*) FROM streams", [], |r| r.get(0));
    assert!(
        probe.is_err(),
        "an unkeyed connection must not be able to read the vault"
    );
}

/// `migrations.md` promises re-running a migration is a no-op. It is not the
/// SQL that provides that — none of 0014-0018 is re-runnable, they are bare
/// `ALTER TABLE ADD COLUMN` and `CREATE TABLE` — it is the runner, which
/// applies only migrations above the stamped version. This pins the promise in
/// the form a caller sees it: opening the same vault again and again changes
/// nothing.
///
/// The fingerprint comparison is what makes this more than "it did not crash".
/// A second application of 0015 would add a second `extra` column, a second
/// 0017 would leave `stream_keys_old` behind — both visible here, neither
/// visible to a test that only re-reads `storage_v`.
#[test]
fn re_opening_a_current_vault_applies_no_migration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vault.db");

    let (first_fingerprint, first_v) = {
        let db = Db::open(&path, &root(0x11)).expect("create the vault");
        insert_stream(&db, 3, "Reading");
        (schema_fingerprint(&db), stamped_storage_v(&db))
    };
    assert_eq!(first_v, u32::from(STORAGE_V));

    for open in 2..=4 {
        let db = Db::open(&path, &root(0x11)).unwrap_or_else(|e| panic!("open {open}: {e}"));
        assert_eq!(
            schema_fingerprint(&db),
            first_fingerprint,
            "open {open} changed the schema"
        );
        assert_eq!(
            stamped_storage_v(&db),
            first_v,
            "open {open} moved STORAGE_V"
        );
        assert_eq!(
            stream_names(&db),
            vec!["Reading".to_string()],
            "open {open} lost a row"
        );
    }
}

/// The upper boundary: this build opens `STORAGE_V` and refuses `STORAGE_V + 1`.
///
/// Both halves matter. A vault written by a newer build holds tables and
/// columns this binary has never heard of, and the honest answer is to refuse
/// it — running against it would materialize into a schema that is missing
/// columns the other build writes, and the user would lose whatever lives in
/// them. The error carries both versions because that is the only thing a UI
/// can tell the user with ("this vault needs a newer Sunrise").
#[test]
fn storage_v_boundary_this_build_opens_and_one_past_it_is_refused() {
    let key = root(0x33);

    // Exactly this build's version: accepted.
    let accepted = tempfile::tempdir().expect("tempdir");
    let path = vault_stamped_at(accepted.path(), &key, u32::from(STORAGE_V));
    let db = Db::open(&path, &key).expect("a vault at STORAGE_V must open");
    assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V));
    drop(db);

    // One past it: refused, with both numbers.
    let refused = tempfile::tempdir().expect("tempdir");
    let path = vault_stamped_at(refused.path(), &key, u32::from(STORAGE_V) + 1);
    let err = Db::open(&path, &key).expect_err("a newer vault must be refused");
    match &err {
        DbError::StorageVTooNew { db_v, binary_v } => {
            assert_eq!(*db_v, u32::from(STORAGE_V) + 1);
            assert_eq!(*binary_v, u32::from(STORAGE_V));
        }
        other => panic!("expected StorageVTooNew, got {other:?}"),
    }
    assert_eq!(err.as_error_code(), ErrorCode::StorageVTooNew);
}

/// The lower boundary: the baseline itself is inside the range, and one below
/// it is refused as pre-baseline rather than as merely old.
///
/// ADR-0018 deleted the migrations that would have carried a `storage_v < 13`
/// vault forward, so there is nothing to run and the refusal is terminal. It
/// gets its own variant for exactly that reason — a caller that treated it as
/// `StorageVTooOld` would offer a "retry the upgrade" button that can never
/// work. The wire code is deliberately the shared one, and that is pinned too:
/// the distinction is for the local caller, not for the protocol.
#[test]
fn storage_v_boundary_one_below_the_baseline_is_refused_as_pre_baseline() {
    let key = root(0x44);

    let below = tempfile::tempdir().expect("tempdir");
    let path = vault_stamped_at(below.path(), &key, BASELINE_STORAGE_V - 1);
    let err = Db::open(&path, &key).expect_err("a pre-baseline vault must be refused");
    match &err {
        DbError::StorageVPreBaseline { db_v, baseline_v } => {
            assert_eq!(*db_v, BASELINE_STORAGE_V - 1);
            assert_eq!(*baseline_v, BASELINE_STORAGE_V);
        }
        other => panic!("expected StorageVPreBaseline, got {other:?}"),
    }
    assert_eq!(err.as_error_code(), ErrorCode::StorageVTooOld);

    // The baseline itself is not pre-baseline: the comparison is strict, and
    // 13 is inside the accepted range. Asserted so it cannot drift to `<=`
    // without something noticing.
    //
    // Only the refusal is asserted, not the outcome. This fixture is a current
    // schema with its stamp lied down to 13, not a real 13-era vault, so
    // whatever the runner then makes of it says nothing about the genuine
    // upgrade path — and a genuine 13-era vault cannot be built from outside
    // the crate, because keying one is `Db`'s private business. What the gate
    // must not do is refuse it before the runner is reached at all.
    let at = tempfile::tempdir().expect("tempdir");
    let path = vault_stamped_at(at.path(), &key, BASELINE_STORAGE_V);
    let outcome = Db::open(&path, &key);
    assert!(
        !matches!(outcome, Err(DbError::StorageVPreBaseline { .. })),
        "the baseline version is inside the range, not below it"
    );
}

/// `Db::with_tx` documents that returning `Err` rolls back. Nothing pinned it,
/// and the whole op-application loop is built on it: a batch that fails partway
/// must leave the vault exactly as it was, or a replica materializes half an op
/// batch and never notices.
#[test]
fn with_tx_rolls_back_when_the_closure_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vault.db");
    let mut db = Db::open(&path, &root(0x55)).expect("create the vault");
    insert_stream(&db, 1, "kept");

    let outcome: Result<(), DbError> = db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO streams
             (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
             VALUES (X'02', X'00', 0, 'rolled back', 0, 0)",
            [],
        )?;
        // Anything at all going wrong after a write has landed in the tx.
        tx.execute("INSERT INTO no_such_table (x) VALUES (1)", [])?;
        Ok(())
    });
    assert!(outcome.is_err(), "the closure failed, so with_tx must");
    assert_eq!(
        stream_names(&db),
        vec!["kept".to_string()],
        "the write before the failure must not have survived"
    );

    // ...and the happy path still commits, so the rollback is not simply
    // discarding everything.
    db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO streams
             (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
             VALUES (X'03', X'00', 0, 'committed', 0, 0)",
            [],
        )
        .map(|_| ())
    })
    .expect("a successful closure commits");
    assert_eq!(
        stream_names(&db),
        vec!["committed".to_string(), "kept".to_string()]
    );
}
