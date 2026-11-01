//! Every fixture, every later migration: the survival check, the differences
//! it allows, and the gates that keep the fixtures and the seed complete.

use std::collections::BTreeMap;
use std::fs;

use rusqlite::types::Value;
use rusqlite::Connection;
use sunrise_cbor::version::STORAGE_V;

use super::seed::{columns, seed_every_table, seed_rows};
use super::{
    fixture_dir, fixture_key, fixture_name, fixture_versions, is_curated, stamped_storage_v,
    version_before,
};
use crate::db::Db;
use crate::migrations::{BASELINE_STORAGE_V, MIGRATIONS};

/// A difference a migration makes on purpose, which the survival check below
/// therefore allows on a fixture older than that migration.
struct DeliberateChange {
    /// The migration that makes it.
    migration: u32,
    table: &'static str,
    /// The column it rewrites, or `None` when it drops the table's rows.
    column: Option<&'static str>,
}

/// Every deliberate difference in the chain. Each one is asserted value by
/// value by a test named after the fixture that reaches it; this list only
/// keeps the general check from calling it a loss.
const DELIBERATE_CHANGES: &[DeliberateChange] = &[
    // Pre-hierarchy keys were derived from the vault root, and
    // `Keychain::open` re-derives them. Pinned by
    // `a_v13_vaults_derived_stream_keys_are_dropped_and_the_local_identity_is_not`.
    DeliberateChange {
        migration: 17,
        table: "stream_keys",
        column: None,
    },
    // Carried into `device_revocations`, then cleared. Pinned by
    // `a_v13_vaults_revoked_device_becomes_a_device_revocations_row`.
    DeliberateChange {
        migration: 17,
        table: "devices",
        column: Some("revoked_at_ms"),
    },
    // The legacy Inbox moves off the vault-meta stream id. Pinned by
    // `a_v13_vault_keeps_its_tasks_and_0017_repoints_the_legacy_inbox`.
    DeliberateChange {
        migration: 17,
        table: "tasks",
        column: Some("stream_id"),
    },
    // Blanked on a vault that did not mint the identity. Pinned, both ways,
    // by `db.rs`'s
    // `migration_0019_clears_a_paired_devices_identity_key_and_keeps_the_creators`
    // and here by `a_v17_creator_vault_keeps_the_only_copy_of_its_identity_key`.
    DeliberateChange {
        migration: 19,
        table: "identity",
        column: Some("id_d_priv_wrapped"),
    },
    // The task-only index is dropped, and the next open writes both new
    // indexes from the entity rows. Pinned by `db.rs`'s
    // `migration_0040_replaces_the_task_index_with_two_empty_ones`.
    DeliberateChange {
        migration: 40,
        table: "search_idx",
        column: None,
    },
    // The Inbox sentinel becomes no stream, and its synthetic `streams` row
    // goes (ADR-0046 §5). Pinned by
    // `a_v43_vaults_inbox_rows_have_no_stream_after_0044` and, from 13, by
    // `a_v13_vault_keeps_its_tasks_and_0017_repoints_the_legacy_inbox`.
    DeliberateChange {
        migration: 44,
        table: "tasks",
        column: Some("stream_id"),
    },
    DeliberateChange {
        migration: 44,
        table: "blocks",
        column: Some("stream_id"),
    },
    DeliberateChange {
        migration: 44,
        table: "routines",
        column: Some("stream_id"),
    },
    DeliberateChange {
        migration: 44,
        table: "streams",
        column: None,
    },
];

/// A column a migration renames, which the survival check below reads under
/// its new name on a fixture older than that migration.
struct RenamedColumn {
    /// The migration that renames it.
    migration: u32,
    table: &'static str,
    from: &'static str,
    to: &'static str,
}

/// Every column rename in the chain. Each is asserted value by value by a
/// test named after the fixture that reaches it; this list only keeps the
/// general check from calling the old name a loss.
const RENAMED_COLUMNS: &[RenamedColumn] = &[
    // ADR-0047 §1: the plan and the hard deadline. Pinned by
    // `a_v44_vaults_task_times_are_the_plan_and_the_hard_deadline_after_0045`.
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "scheduled_at_ms",
        to: "planned_at_ms",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "scheduled_at_kind",
        to: "planned_at_kind",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "scheduled_at_tz",
        to: "planned_at_tz",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "scheduled_at_civil",
        to: "planned_at_civil",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "due_at_ms",
        to: "hard_due_at_ms",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "due_at_kind",
        to: "hard_due_at_kind",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "due_at_tz",
        to: "hard_due_at_tz",
    },
    RenamedColumn {
        migration: 45,
        table: "tasks",
        from: "due_at_civil",
        to: "hard_due_at_civil",
    },
];

/// The name `table.column` of a fixture at `from_v` has once every later
/// migration has run.
fn renamed(table: &str, column: &str, from_v: u32) -> String {
    let mut name = column.to_string();
    for r in RENAMED_COLUMNS
        .iter()
        .filter(|r| r.migration > from_v && r.table == table)
    {
        if name == r.from {
            name = r.to.to_string();
        }
    }
    name
}

/// Every row of every table, as `table -> (columns, rows)`.
///
/// FTS5's shadow tables are left out: they are the storage of `search_idx`,
/// `search_words` and `search_tri`, which is read through those tables
/// themselves, and `schema_meta` is the stamp the migration is supposed to
/// change.
type Snapshot = BTreeMap<String, (Vec<String>, Vec<Vec<Value>>)>;

fn snapshot(conn: &Connection) -> Snapshot {
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table'
                 AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'search_idx_%'
                 AND name NOT LIKE 'search_words_%' AND name NOT LIKE 'search_tri_%'
                 AND name <> 'schema_meta' ORDER BY name",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get(0))
            .expect("list tables")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read tables");
        rows
    };
    tables
        .into_iter()
        .map(|table| {
            let cols = columns(conn, &table).expect("a listed table has columns");
            let rows = select(conn, &table, &cols);
            (table, (cols, rows))
        })
        .collect()
}

fn select(conn: &Connection, table: &str, cols: &[String]) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT {} FROM {table}", cols.join(", ")))
        .unwrap_or_else(|e| panic!("select from `{table}`: {e}"));
    let rows = stmt
        .query_map([], |r| {
            (0..cols.len())
                .map(|i| r.get::<_, Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read rows");
    rows
}

/// Everything `before` held is still in the migrated vault: every table,
/// every column, and every row with every value it had, except where a
/// [`DELIBERATE_CHANGES`] entry newer than `from_v` says otherwise. Rows and
/// columns the migrations *add* are not differences.
fn assert_nothing_lost(name: &str, from_v: u32, before: &Snapshot, after: &Connection) {
    let changes: Vec<&DeliberateChange> = DELIBERATE_CHANGES
        .iter()
        .filter(|c| c.migration > from_v)
        .collect();
    for (table, (cols, rows)) in before {
        if changes
            .iter()
            .any(|c| c.table == table && c.column.is_none())
        {
            continue;
        }
        let present = columns(after, table)
            .unwrap_or_else(|| panic!("{name}: table `{table}` is gone after migrating"));
        let kept: Vec<usize> = (0..cols.len())
            .filter(|&i| {
                !changes
                    .iter()
                    .any(|c| c.table == table && c.column == Some(cols[i].as_str()))
            })
            .collect();
        let kept_cols: Vec<String> = kept
            .iter()
            .map(|&i| renamed(table, &cols[i], from_v))
            .collect();
        for col in &kept_cols {
            assert!(
                present.contains(col),
                "{name}: column `{table}.{col}` is gone after migrating"
            );
        }
        let mut remaining = select(after, table, &kept_cols);
        for row in rows {
            let projected: Vec<Value> = kept.iter().map(|&i| row[i].clone()).collect();
            let at = remaining
                .iter()
                .position(|r| *r == projected)
                .unwrap_or_else(|| {
                    panic!(
                        "{name}: a `{table}` row did not survive migrating from v{from_v}:\n  \
                         columns {kept_cols:?}\n  before  {projected:?}\n  after   {remaining:?}"
                    )
                });
            remaining.swap_remove(at);
        }
    }
}

/// The gate. A migration appended without a fixture at the version before it
/// fails here, and `mise run storage-fixtures` writes the one it asks for.
#[test]
fn every_migration_has_a_fixture_at_the_version_before_it() {
    for m in MIGRATIONS.iter().filter(|m| m.id > BASELINE_STORAGE_V) {
        let name = fixture_name(version_before(m.id).expect("the baseline is first"));
        assert!(
            fixture_dir().join(&name).exists(),
            "migration {:04} ({}) has no fixture at the version before it: \
             `{name}` is missing. Run `mise run storage-fixtures`, which writes \
             it from `seed::SEED`, and commit it.",
            m.id,
            m.name
        );
    }
}

/// Every committed fixture is encrypted and stamped at the version its name
/// says. One regenerated at the current version would pass every other test
/// here while exercising no migration.
#[test]
fn every_committed_fixture_is_sealed_and_stamped_at_its_version() {
    for v in fixture_versions() {
        let name = fixture_name(v);
        let bytes =
            fs::read(fixture_dir().join(&name)).unwrap_or_else(|e| panic!("read `{name}`: {e}"));
        assert!(
            !bytes.starts_with(b"SQLite format 3\0"),
            "`{name}` must be encrypted, or it is not a vault"
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(&name);
        fs::copy(fixture_dir().join(&name), &path).expect("copy");
        let raw = Db::open_unmigrated(&path, &fixture_key()).expect("key the fixture");
        assert_eq!(stamped_storage_v(&raw), v, "`{name}` is stamped wrong");
    }
}

/// Every fixture, opened with the ordinary `Db::open`, which runs every
/// migration after its version, keeps every row it held.
///
/// A fixture filled from [`SEED`](super::seed::SEED) must also hold a row in every table its
/// version has, or the check would be passing over empty tables.
#[test]
fn every_fixture_keeps_every_row_through_every_later_migration() {
    for v in fixture_versions() {
        let name = fixture_name(v);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(&name);
        fs::copy(fixture_dir().join(&name), &path).expect("copy");
        let before = {
            let raw = Db::open_unmigrated(&path, &fixture_key()).expect("key the fixture");
            snapshot(raw.conn())
        };
        if !is_curated(v) {
            for (table, (_, rows)) in &before {
                assert!(!rows.is_empty(), "`{name}` holds no `{table}` row");
            }
        }

        let db = Db::open(&path, &fixture_key())
            .unwrap_or_else(|e| panic!("the migration chain must open `{name}`: {e}"));
        assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V), "`{name}`");
        assert_nothing_lost(&name, v, &before, db.conn());
    }
}

/// [`SEED`](super::seed::SEED) fills every table this build creates, and names no column that
/// does not exist — a misspelt one would otherwise be left out of every
/// fixture without a word.
#[test]
fn the_seed_reaches_every_table_and_column_this_build_creates() {
    let db = Db::open_memory(&fixture_key()).expect("a current vault");
    let current = snapshot(db.conn());
    for table in current.keys() {
        assert!(
            seed_rows().any(|row| row.table == table),
            "`{table}` exists at STORAGE_V {STORAGE_V} and `seed::SEED` fills no \
             row of it, so no fixture written from now on would carry one"
        );
    }
    for row in seed_rows() {
        // A table a later migration drops is still filled in the fixtures
        // written before it, which is where that migration's drop is checked.
        let dropped = DELIBERATE_CHANGES
            .iter()
            .any(|c| c.table == row.table && c.column.is_none());
        if dropped && !current.contains_key(row.table) {
            continue;
        }
        let (cols, _) = current.get(row.table).unwrap_or_else(|| {
            panic!(
                "`seed::SEED` names `{}`, which no migration creates",
                row.table
            )
        });
        for (column, _) in row.values {
            // A column a later migration renames is still filled in the
            // fixtures written before it, under the name it had there.
            if RENAMED_COLUMNS
                .iter()
                .any(|r| r.table == row.table && r.from == *column)
            {
                continue;
            }
            assert!(
                cols.iter().any(|c| c == column),
                "`seed::SEED` names `{}.{column}`, which no migration creates",
                row.table
            );
        }
    }
    // And it inserts cleanly into the current schema, so the next fixture it
    // writes will not fail halfway.
    seed_every_table(db.conn());
}
