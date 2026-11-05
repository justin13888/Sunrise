use super::cipher::{await_cipher_ready, ensure_cipher_ready, probe_cipher};
use super::{Db, DbError};
use crate::migrations::{BASELINE_STORAGE_V, MIGRATIONS};
use rusqlite::Connection;
use sunrise_cbor::version::STORAGE_V;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_error::ErrorCode;

fn vault_key() -> VaultRootKey {
    VaultRootKey::from_bytes([0xab; 32])
}

/// What SQLCipher returns for a key applied before it has initialised.
fn uninitialised_cipher() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some("An error occurred with PRAGMA key or rekey.".into()),
    )
}

#[test]
fn the_cipher_gate_waits_out_an_initialisation_in_progress() {
    let mut probes = 0;
    let mut waits = 0;
    let ready = await_cipher_ready(
        || {
            probes += 1;
            if probes < 4 {
                Err(uninitialised_cipher())
            } else {
                Ok(())
            }
        },
        10,
        || waits += 1,
    );
    assert_eq!(ready, Ok(()));
    assert_eq!((probes, waits), (4, 3));
}

#[test]
fn a_cipher_that_never_initialises_is_reported_as_unavailable_with_its_error() {
    let mut probes = 0;
    let detail = await_cipher_ready(
        || {
            probes += 1;
            Err(uninitialised_cipher())
        },
        3,
        || {},
    )
    .unwrap_err();
    assert_eq!(probes, 3, "the gate gives up after its attempts");
    assert!(detail.contains("failed 3 times"), "{detail}");
    assert!(detail.contains("PRAGMA key or rekey"), "{detail}");

    let e = DbError::CipherUnavailable { detail };
    assert!(
        e.to_string()
            .starts_with("SQLCipher could not be initialised"),
        "{e}"
    );
    assert_eq!(e.as_error_code(), ErrorCode::FatalInternal);
}

#[test]
fn this_process_cipher_is_ready() {
    ensure_cipher_ready().unwrap();
    probe_cipher().unwrap();
}

#[test]
fn open_in_memory_initializes_schema() {
    let db = Db::open_memory(&vault_key()).unwrap();
    let v: u32 = db
        .conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |row| row.get(0))
        .unwrap();
    assert_eq!(v, u32::from(STORAGE_V));
}

#[test]
fn open_persistent_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    {
        let _ = Db::open(&path, &vault_key()).unwrap();
    }
    // Reopen with the same key.
    let _ = Db::open(&path, &vault_key()).unwrap();
}

#[test]
fn wrong_key_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    {
        let _ = Db::open(&path, &vault_key()).unwrap();
    }
    let bogus = VaultRootKey::from_bytes([0u8; 32]);
    let res = Db::open(&path, &bogus);
    assert!(res.is_err(), "wrong vault root must not open the DB");
}

/// A fresh vault applies the baseline and everything appended after it,
/// landing at the binary's `STORAGE_V` rather than at the baseline.
#[test]
fn fresh_vault_applies_the_baseline_and_everything_after_it() {
    let db = Db::open_memory(&vault_key()).unwrap();
    let v: u32 = db
        .conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, u32::from(STORAGE_V));
    assert!(v >= BASELINE_STORAGE_V);

    // Spot-check that the collapse did not lose a table from the middle of
    // the old sequence: one from 0001, one from 0005, one from 0010, one
    // from 0011.
    for table in [
        "ops",
        "streams",
        "tasks",
        "local_identity",
        "sync_cursors",
        "contexts",
        "task_blockers",
        "focus_sessions",
        "review_snapshots",
    ] {
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master
                     WHERE type = 'table' AND name = ?",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "baseline must create `{table}`");
    }
}

/// ...and it does NOT recreate the schema the reset dropped.
#[test]
fn baseline_omits_the_dead_schema() {
    let db = Db::open_memory(&vault_key()).unwrap();
    let journal: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'merge_journal'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        journal, 0,
        "merge_journal is a per-field journal for an entity-level merge model"
    );

    // `routines.extra` is deliberately NOT in this list any more. The
    // baseline dropped it because it had no reader — 0004 recorded it as
    // "left NULL" and it still was — and `0015_entity_extra_columns.sql`
    // reinstates it with one, which is the exact condition its removal was
    // predicated on. A column that is written and read is not dead schema.
    for (table, column) in [
        ("streams", "doc_blob"),
        ("streams", "doc_blob_v"),
        ("routines", "rrule"),
    ] {
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM pragma_table_info(?) WHERE name = ?",
                rusqlite::params![table, column],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "`{table}.{column}` was dropped by the reset");
    }
}

/// 0015 gives every column-projected entity an `extra` blob, so forward-
/// compat unknowns survive materialization rather than only `tasks`,
/// `blocks` and `attachments` doing so.
///
/// The projected tables are read from the entity registry
/// (`sunrise_id::for_each_entity!`), so a record registered with a table
/// no migration creates fails here. The two exclusions are registry
/// declarations too, because both are decisions rather than omissions:
/// `focus_interruptions` holds the one entity with no `unknown` map (its
/// whole value is its key), and `review_snapshots` already round-trips
/// through its whole-record CBOR `body` blob, so a column there would be a
/// second home for the same data.
#[test]
fn every_column_projected_entity_has_an_extra_blob() {
    use sunrise_id::registry::{Merge, Unknowns, ENTITIES};

    let db = Db::open_memory(&vault_key()).unwrap();
    let has_column = |table: &str, column: &str| -> bool {
        db.conn()
            .query_row(
                "SELECT count(*) FROM pragma_table_info(?) WHERE name = ?",
                rusqlite::params![table, column],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    };

    let mut projected = 0;
    for entity in &ENTITIES {
        for record in entity.records {
            let Some(storage) = record.storage else {
                continue;
            };
            projected += 1;
            let table = storage.table;
            assert!(
                has_column(table, storage.key),
                "`{table}.{}` must hold {}'s id",
                storage.key,
                record.name
            );
            assert_eq!(
                has_column(table, "extra"),
                storage.unknowns == Unknowns::Extra,
                "`{table}.extra` disagrees with the registry; see 0015's header"
            );
            if storage.unknowns == Unknowns::Body {
                assert!(has_column(table, "body"), "`{table}.body` must exist");
            }
        }
        // The table the materializer reads an LWW stamp from must hold one.
        if let (Merge::Lww, Some(storage)) = (entity.merge, entity.storage()) {
            for column in ["lww_hlc_ms", "lww_hlc_logical", "lww_seq", "lww_device"] {
                assert!(
                    has_column(storage.table, column),
                    "`{}.{column}` must exist for {:?}'s merge",
                    storage.table,
                    entity.kind
                );
            }
        }
    }
    assert_eq!(projected, 12, "the registry projects twelve tables");
}

/// 0014 adds `streams.sort_order`, defaulting to the "never ordered"
/// sentinel rather than to a key.
#[test]
fn stream_sort_order_column_exists_and_defaults_to_unset() {
    let db = Db::open_memory(&vault_key()).unwrap();
    db.conn()
        .execute(
            "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
                 VALUES (X'01', X'00', 0, 'Work', 0, 0)",
            [],
        )
        .unwrap();
    let got: String = db
        .conn()
        .query_row("SELECT sort_order FROM streams", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        got, "",
        "an un-keyed row must be distinguishable from a first-position one"
    );
}

/// 0014's backfill hands every pre-existing row a key, in the order those
/// rows were already being displayed — so a vault's sidebar does not
/// rearrange itself on upgrade.
///
/// Exercised by replaying 0013 and then 0014 by hand, because a fresh
/// vault has no rows for the backfill to touch.
#[test]
fn migration_0014_backfills_existing_streams_in_display_order() {
    let mut conn = Connection::open_in_memory().unwrap();
    Db::apply_pragmas(&conn).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute_batch(MIGRATIONS[0].sql).unwrap();
    // Deliberately inserted out of order, and with a case that only a
    // NOCASE collation gets right.
    for (id, name) in [(1u8, "zebra"), (2, "Apple"), (3, "mango")] {
        tx.execute(
            "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
                 VALUES (?, X'00', 0, ?, 0, 0)",
            rusqlite::params![vec![id], name],
        )
        .unwrap();
    }
    tx.execute_batch(MIGRATIONS[1].sql).unwrap();

    let mut stmt = tx
        .prepare("SELECT name, sort_order FROM streams ORDER BY sort_order")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(stmt);
    assert_eq!(
        rows,
        vec![
            ("Apple".to_string(), "AAAN".to_string()),
            ("mango".to_string(), "AABN".to_string()),
            ("zebra".to_string(), "AACN".to_string()),
        ],
        "sorting by the new key must reproduce the old name ordering"
    );
}

/// A 0016 vault upgrades to 0017: the identity, `deferred_ops`,
/// and `device_revocations` arrive, and `stream_keys` is
/// re-keyed on `(stream_id, epoch, key_id)`.
///
/// The pre-0017 rows are dropped on purpose and this asserts it: every one
/// of them wrapped a key *derived* from the vault root, so
/// `Keychain::open`'s legacy adoption recomputes the identical key on the
/// next open. Carrying them forward would mean inventing a `key_id` for a
/// row during a migration that has no access to the vault root, and so no
/// way to unwrap the key the id is computed from.
#[test]
fn migration_0017_rekeys_stream_keys_and_adds_the_hierarchy_tables() {
    let mut conn = Connection::open_in_memory().unwrap();
    Db::apply_pragmas(&conn).unwrap();
    let tx = conn.transaction().unwrap();
    // Everything up to and including 0016. Selected by id, not by
    // position: `MIGRATIONS[..len - 1]` meant "everything but 0017" only
    // while 0017 happened to be last, and silently became "everything but
    // 0018" -- which re-ran 0017 against a schema it had already migrated
    // -- the first time a migration was appended.
    for m in MIGRATIONS.iter().filter(|m| m.id <= 16) {
        tx.execute_batch(m.sql).unwrap();
    }
    tx.execute(
        "INSERT INTO stream_keys (stream_id, epoch, wrapped, created_at_ms)
             VALUES (?, 1, X'00', 0)",
        rusqlite::params![vec![7u8; 16]],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO devices
             (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
             VALUES (?, X'00', 'old', 'test', 0, NULL)",
        rusqlite::params![vec![9u8; 16]],
    )
    .unwrap();

    tx.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.id == 17)
            .expect("0017 is registered")
            .sql,
    )
    .unwrap();

    for table in [
        "identity",
        "stream_keys",
        "deferred_ops",
        "device_revocations",
    ] {
        let n: i64 = tx
            .query_row(
                "SELECT count(*) FROM sqlite_master
                     WHERE type = 'table' AND name = ?",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "0017 must create `{table}`");
    }
    let leftover: i64 = tx
        .query_row(
            "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'stream_keys_old'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        leftover, 0,
        "the renamed table must not survive the upgrade"
    );

    // The re-keyed table is empty and takes a `key_id` + `source`.
    let rows: i64 = tx
        .query_row("SELECT count(*) FROM stream_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "derived pre-0017 keys are re-derived on open, not migrated"
    );
    tx.execute(
        "INSERT INTO stream_keys (stream_id, epoch, key_id, wrapped, source, created_at_ms)
             VALUES (?, 1, X'0102030405060708', X'00', 'legacy', 0)",
        rusqlite::params![vec![7u8; 16]],
    )
    .unwrap();
    // Same (stream_id, epoch), different key_id: both concurrent mints live.
    tx.execute(
        "INSERT INTO stream_keys (stream_id, epoch, key_id, wrapped, source, created_at_ms)
             VALUES (?, 1, X'0807060504030201', X'00', 'envelope', 0)",
        rusqlite::params![vec![7u8; 16]],
    )
    .unwrap();
    let rows: i64 = tx
        .query_row("SELECT count(*) FROM stream_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows, 2,
        "(stream_id, epoch, key_id) must admit two keys per epoch"
    );

    // The devices row survived and grew its identity columns. Revocation
    // is no longer among them: it lives in `device_revocations`, so that a
    // `device_revoke` naming a device this vault has never seen cannot
    // mint a phantom entry in the user's device list.
    let (nickname, identity_id): (String, Option<Vec<u8>>) = tx
        .query_row("SELECT nickname, identity_id FROM devices", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(nickname, "old");
    assert!(identity_id.is_none());
    let revocations: i64 = tx
        .query_row("SELECT count(*) FROM device_revocations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(revocations, 0, "an unrevoked device carries no register");
}

/// 0019 clears `identity.id_d_priv_wrapped` on a device that was *paired*
/// at STORAGE_V 17, and leaves it alone on the account's creator.
///
/// This is issue #87. 0018 declined to touch the column because the schema
/// recorded nothing that told the two apart; `stream_keys.source` does,
/// because only a founding vault mints the vault-meta stream's first epoch
/// itself. Both halves are asserted, because either alone is worthless: a
/// migration that clears every row trades a revocation gap for an account
/// whose `ID_D_priv` no longer exists anywhere.
#[test]
fn migration_0019_clears_a_paired_devices_identity_key_and_keeps_the_creators() {
    // `source` is the discriminator, so the two vaults differ in exactly
    // that one column and in nothing else.
    let build = |meta_key_source: &str| -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        let tx = conn.transaction().unwrap();
        for m in MIGRATIONS.iter().filter(|m| m.id <= 18) {
            tx.execute_batch(m.sql).unwrap();
        }
        tx.execute(
            "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, X'00', X'00', 0)",
            rusqlite::params![vec![3u8; 16]],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO identity
                 (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped,
                  id_d_priv_wrapped, created_at_ms)
                 VALUES (1, ?, ?, ?, X'aa', X'bb', 0)",
            rusqlite::params![vec![4u8; 16], vec![5u8; 32], vec![6u8; 32]],
        )
        .unwrap();
        // Epoch 1 of the vault-meta stream: sixteen zero bytes.
        tx.execute(
            "INSERT INTO stream_keys
                 (stream_id, epoch, key_id, wrapped, source, created_at_ms)
                 VALUES (?, 1, X'0102030405060708', X'00', ?, 0)",
            rusqlite::params![vec![0u8; 16], meta_key_source],
        )
        .unwrap();
        tx.execute_batch(
            MIGRATIONS
                .iter()
                .find(|m| m.id == 19)
                .expect("0019 is registered")
                .sql,
        )
        .unwrap();
        tx.commit().unwrap();
        conn
    };
    let read = |conn: &Connection| -> (Vec<u8>, Option<Vec<u8>>) {
        conn.query_row(
            "SELECT id_d_priv_wrapped, minted_by_device_id FROM identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };

    let (wrapped, minted_by) = read(&build("pairing"));
    assert!(
        wrapped.is_empty(),
        "a device whose vault-meta key came from a pairing payload never \
             minted the identity, and must not keep its unwrapping key"
    );
    assert!(minted_by.is_none(), "and it is recorded as not the minter");

    let (wrapped, minted_by) = read(&build("envelope"));
    assert!(
        wrapped.is_empty(),
        "nor may one whose vault-meta key arrived in a `key_envelope` op"
    );
    assert!(minted_by.is_none());

    for source in ["local", "legacy"] {
        let (wrapped, minted_by) = read(&build(source));
        assert_eq!(
            wrapped,
            vec![0xbbu8],
            "the creator's copy is the only one in existence and must survive \
                 ({source})"
        );
        assert_eq!(
            minted_by,
            Some(vec![3u8; 16]),
            "and the row now names it, so nothing has to infer it again"
        );
    }
}

/// A vault from before the reset is REFUSED, with its own error — not
/// silently upgraded, and not confused with a too-new one.
#[test]
fn refuses_a_pre_baseline_vault() {
    let mut conn = Connection::open_in_memory().unwrap();
    Db::apply_pragmas(&conn).unwrap();
    // A v12 vault: the last shape that existed before the baseline. Only
    // `schema_meta` is needed — the refusal happens on the stamped version,
    // before any DDL is considered.
    conn.execute_batch(
        "CREATE TABLE schema_meta (storage_v INTEGER NOT NULL,
                                       applied_at_ms INTEGER NOT NULL);
             INSERT INTO schema_meta (storage_v, applied_at_ms) VALUES (12, 0);",
    )
    .unwrap();

    let err = Db::ensure_schema(&mut conn, None, false).unwrap_err();
    assert!(
        matches!(
            err,
            DbError::StorageVPreBaseline {
                db_v: 12,
                baseline_v: 13
            }
        ),
        "expected a pre-baseline refusal, got {err:?}"
    );
    assert_eq!(err.as_error_code(), ErrorCode::StorageVTooOld);
}

/// Every pre-baseline version is refused, not just the last one.
#[test]
fn refuses_every_pre_baseline_version() {
    for db_v in 1..BASELINE_STORAGE_V {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_meta (storage_v INTEGER NOT NULL,
                                           applied_at_ms INTEGER NOT NULL);
                 INSERT INTO schema_meta (storage_v, applied_at_ms) VALUES (0, 0);",
        )
        .unwrap();
        conn.execute(
            "UPDATE schema_meta SET storage_v = ?",
            rusqlite::params![db_v],
        )
        .unwrap();
        assert!(
            matches!(
                Db::ensure_schema(&mut conn, None, false),
                Err(DbError::StorageVPreBaseline { .. })
            ),
            "storage_v = {db_v} must be refused"
        );
    }
}
#[test]
fn rejects_db_from_newer_binary() {
    let mut conn = Connection::open_in_memory().unwrap();
    Db::apply_pragmas(&conn).unwrap();
    Db::ensure_schema(&mut conn, None, false).unwrap();
    // Stamp a version strictly newer than this binary supports.
    let too_new = u32::from(STORAGE_V) + 1;
    conn.execute(
        "UPDATE schema_meta SET storage_v = ?",
        rusqlite::params![too_new],
    )
    .unwrap();
    let err = Db::ensure_schema(&mut conn, None, false).unwrap_err();
    assert!(matches!(err, DbError::StorageVTooNew { .. }));
}

/// Both full-text indexes exist, with the tokenizers 0040 names: the
/// trigram one finds a substring, which the bundled SQLite must support.
#[test]
fn both_fts5_indexes_are_present_and_the_trigram_one_matches_substrings() {
    let db = Db::open_memory(&vault_key()).unwrap();
    for table in ["search_words", "search_tri"] {
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = ?",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "`{table}`");
    }
    db.conn()
        .execute(
            "INSERT INTO search_tri (rowid, title, body) VALUES (1, 'passport', '')",
            [],
        )
        .unwrap();
    let hit: i64 = db
        .conn()
        .query_row(
            "SELECT rowid FROM search_tri WHERE search_tri MATCH '\"sspo\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hit, 1);
}

/// Migration 0040 drops the task-only `search_idx` and adds both new
/// indexes empty, with no `search_meta` row, which is what makes the next
/// open write them from the entity rows.
#[test]
fn migration_0040_replaces_the_task_index_with_two_empty_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let db = Db::create_at_storage_v(&path, &vault_key(), 39).unwrap();
    db.conn()
        .execute(
            "INSERT INTO search_idx (kind, id, stream_id, title, body, contexts)
                 VALUES ('task', X'01', X'02', 'Renew the passport', '', '')",
            [],
        )
        .unwrap();
    drop(db);

    let db = Db::open(&path, &vault_key()).unwrap();
    let gone: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'search_idx'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(gone, 0, "`search_idx` is dropped");
    for table in ["search_docs", "search_words", "search_tri", "search_meta"] {
        let n: i64 = db
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("`{table}` after 0040: {e}"));
        assert_eq!(n, 0, "`{table}` starts empty");
    }
}

/// Migration 0039 adds the version DAG's tables empty, with no
/// `projection_rebuild` row, which is what makes the next open rebuild
/// the projection, and leaves the merge state it will be rebuilt from,
/// `merge_entities.row_*` included, as it was.
#[test]
fn migration_0039_adds_the_version_tables_empty_and_keeps_the_merge_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let db = Db::create_at_storage_v(&path, &vault_key(), 38).unwrap();
    db.conn()
        .execute(
            "INSERT INTO merge_entities
                 (entity_id, kind, created, head_hlc_ms, head_hlc_logical, head_device,
                  head_seq, head_stream, row_hlc_ms, row_hlc_logical, row_device, row_seq)
                 VALUES (X'01', 'task', 1, 5, 0, X'02', 3, X'03', 5, 0, X'02', 3)",
            [],
        )
        .unwrap();
    drop(db);

    let db = Db::open(&path, &vault_key()).unwrap();
    for table in [
        "entity_versions",
        "entity_frontier",
        "register_losers",
        "pending_groups",
        "projection_rebuild",
    ] {
        let n: i64 = db
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("`{table}` after 0039: {e}"));
        assert_eq!(n, 0, "`{table}` starts empty");
    }
    let row_seq: i64 = db
        .conn()
        .query_row("SELECT row_seq FROM merge_entities", [], |r| r.get(0))
        .unwrap();
    assert_eq!(row_seq, 3, "the merge state is left for the rebuild");
}
