//! The relay database's versioned schema history, and the runner that applies
//! it.
//!
//! Before this module the relay had no schema version at all: `Store::open`
//! re-ran a `CREATE TABLE IF NOT EXISTS` batch and one hand-written column
//! patch on every start, so a binary could not tell a database from a newer
//! release and would happily write into it, and a change that was not a
//! nullable `ADD COLUMN` had no upgrade path. Now every schema change is a
//! numbered entry in [`MIGRATIONS`], and the database records how far along
//! that list it is in `PRAGMA user_version`.
//!
//! # The rules
//!
//! - **Append only.** A migration that has shipped is never edited, reordered
//!   or removed; a schema change is a new entry with the next id. Migration
//!   0001 reads the tenants' `SCHEMA` constants, so those are frozen too.
//!   `fresh_and_upgraded_databases_reach_the_same_schema` in the store's tests
//!   is what notices an edit: a database built from the literal pre-runner DDL
//!   and migrated forward must end up with exactly the schema a fresh one gets.
//! - **One transaction per migration**, `BEGIN IMMEDIATE`, with the version
//!   stamp written inside it. `user_version` lives in the database header and
//!   is written by the same commit as the DDL, so a crash leaves the database
//!   at the old version with the old schema or at the new version with the new
//!   one, never between. `IMMEDIATE` takes the write lock before the version is
//!   re-read, so two processes opening one file cannot both apply a step.
//! - **A newer database is refused, never written.** [`refuse_newer`] reads
//!   the version before anything on the open path writes: switching the
//!   journal to WAL rewrites the header, so it runs only after this check.
//!
//! # Adopting a pre-runner database
//!
//! Every database a released relay created is at `user_version = 0`, the
//! SQLite default, whatever its tables. Migration 0001 is the DDL those
//! releases ran, still `IF NOT EXISTS`, so against such a database it creates
//! nothing and only stamps version 1; migration 0002 is the one column patch
//! they ran, still guarded by a presence check, because a database from the
//! last release already has the column and one from before it does not.
//!
//! # Portability
//!
//! The history is an ordered list of `(id, name, step)` and the only state is
//! one integer, which is the shape a second backend can carry: a Postgres
//! store records the same ids in a one-row table and applies its own
//! dialect's step for each, so both backends share one version numbering.
//! Nothing here assumes more of SQLite than a transactional integer.

use rusqlite::{Connection, TransactionBehavior};

use super::StoreError;

/// One step of the history.
pub(super) struct Migration {
    /// The `user_version` a database is at once this step has applied.
    pub(super) id: u32,
    /// What the step does, for the log line and for a reader of this list.
    pub(super) name: &'static str,
    /// The schema change. Runs inside the step's transaction.
    apply: fn(&Connection) -> rusqlite::Result<()>,
}

/// Every migration, in apply order, ids `1..=LATEST` with no gaps.
pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        id: 1,
        name: "baseline",
        apply: baseline,
    },
    Migration {
        id: 2,
        name: "devices_vault_device_id",
        apply: devices_vault_device_id,
    },
];

/// The version this binary migrates to, and the newest it will open.
pub(super) const LATEST: u32 = MIGRATIONS[MIGRATIONS.len() - 1].id;

/// 0001: the DDL every pre-runner release ran on each start, in dependency
/// order — accounts, then the devices that reference them, then the relay
/// log's own tables, which reference neither.
fn baseline(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(super::accounts::SCHEMA)?;
    conn.execute_batch(super::devices::SCHEMA)?;
    conn.execute_batch(crate::relay_log::SCHEMA)
}

/// 0002: `devices.vault_device_id`, and the index revocation-by-vault-id
/// runs.
///
/// The column is in `devices::SCHEMA` already, because the release that added
/// it put it there, so on a fresh database 0001 created it and only the index
/// is new here. A database a release before that created has a `devices` table
/// without it, which `CREATE TABLE IF NOT EXISTS` left alone; that is the
/// database this step adds it to. SQLite has no `ADD COLUMN IF NOT EXISTS`, so
/// presence is a `pragma_table_info` count.
///
/// The index comes after the column for that second database: created first,
/// it names a column that does not exist yet and fails the whole step. It is
/// deliberately *not* unique: `docs/06-server/api.md` records that a device
/// re-registering is a second row rather than an error, and all of that
/// device's rows must be revoked together.
fn devices_vault_device_id(conn: &Connection) -> rusqlite::Result<()> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('devices') WHERE name = 'vault_device_id'",
        [],
        |r| r.get(0),
    )?;
    if present == 0 {
        conn.execute_batch("ALTER TABLE devices ADD COLUMN vault_device_id TEXT;")?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS devices_by_vault_id ON devices(account_id, vault_device_id);",
    )
}

/// The database's `user_version`.
///
/// Read as `i64` because the header field is a signed 32-bit integer and a
/// file this binary did not write could hold a negative one.
pub(super) fn version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

/// Refuse a database whose version this binary does not know.
///
/// A version above [`LATEST`] means a newer release migrated this file, and a
/// negative one means something other than this relay stamped it. Either way
/// the schema is one this binary has never seen, so writing a row into it — or
/// even switching its journal mode — could corrupt what the newer release
/// relies on. The operator's remedy is the newer binary or a backup.
pub(super) fn refuse_newer(conn: &Connection) -> Result<i64, StoreError> {
    let found = version(conn)?;
    if found < 0 || found > i64::from(LATEST) {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: LATEST,
        });
    }
    Ok(found)
}

/// Bring the database to [`LATEST`], returning the version it started at.
pub(super) fn migrate(conn: &mut Connection) -> Result<i64, StoreError> {
    migrate_to(conn, LATEST)
}

/// Apply every migration with an id in `(version, target]`, each in its own
/// transaction.
///
/// The version is re-read inside each transaction rather than trusted from
/// before it: another process may have migrated the file between this one's
/// check and its lock, and then the step is already applied.
pub(super) fn migrate_to(conn: &mut Connection, target: u32) -> Result<i64, StoreError> {
    let from = refuse_newer(conn)?;
    for m in MIGRATIONS.iter().filter(|m| m.id <= target) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = refuse_newer(&tx)?;
        if at >= i64::from(m.id) {
            // Dropping the transaction rolls it back; it wrote nothing.
            continue;
        }
        (m.apply)(&tx)?;
        // `PRAGMA` takes no bound parameters; `m.id` is a literal in this file.
        tx.execute_batch(&format!("PRAGMA user_version = {};", m.id))?;
        tx.commit()?;
        tracing::info!(
            ev = "srv.store.migrated",
            from_v = at,
            to_v = m.id,
            "applied relay database migration {:04} {}",
            m.id,
            m.name
        );
    }
    Ok(from)
}

/// What a database's schema *is*, for comparing two of them: one line per
/// column, foreign key and index, sorted.
///
/// Not `sqlite_master.sql`, which is the text each object was created with:
/// `ALTER TABLE ... ADD COLUMN` appends to that text, so a database that got a
/// column by migration and one that got it in its `CREATE TABLE` describe the
/// same table in different words. Column *order* differs between those two as
/// well, and nothing in this crate reads a column by position, so it is left
/// out of the comparison.
#[cfg(test)]
pub(super) fn schema_of(conn: &Connection) -> Vec<String> {
    let objects: Vec<(String, String, Option<String>)> = conn
        .prepare(
            "SELECT type, name, tbl_name FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let rows = |sql: &str, name: &str| -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([name], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let mut out = Vec::new();
    for (kind, name, table) in objects {
        if kind == "table" {
            for c in rows(
                "SELECT name || ' ' || type || ' notnull=' || \"notnull\" || ' default=' || \
                 IFNULL(dflt_value, '-') || ' pk=' || pk FROM pragma_table_info(?1)",
                &name,
            ) {
                out.push(format!("table {name} column {c}"));
            }
            for f in rows(
                "SELECT \"from\" || ' -> ' || \"table\" || '(' || \"to\" || ') on_delete=' || \
                 on_delete FROM pragma_foreign_key_list(?1)",
                &name,
            ) {
                out.push(format!("table {name} fk {f}"));
            }
        } else {
            let columns = rows(
                "SELECT name FROM pragma_index_info(?1) ORDER BY seqno",
                &name,
            );
            out.push(format!(
                "{kind} {name} on {} ({})",
                table.unwrap_or_default(),
                columns.join(", ")
            ));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ids are the versions, so a gap or a reordering would stamp a
    /// database with a version whose step never ran.
    #[test]
    fn migration_ids_count_up_from_one_without_gaps() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                m.id,
                u32::try_from(i + 1).unwrap(),
                "migration {} is out of place",
                m.name
            );
        }
        assert_eq!(LATEST, u32::try_from(MIGRATIONS.len()).unwrap());
    }

    /// Running the runner twice is the restart case, and must change nothing.
    #[test]
    fn a_migrated_database_is_left_alone_by_a_second_run() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        let before = schema_of(&conn);
        assert_eq!(migrate(&mut conn).unwrap(), i64::from(LATEST));
        assert_eq!(schema_of(&conn), before);
        assert_eq!(version(&conn).unwrap(), i64::from(LATEST));
    }

    /// A step that fails leaves the database at the version before it, with
    /// nothing of the step applied: the stamp and the DDL share one commit.
    #[test]
    fn a_failing_step_leaves_the_version_where_it_was() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A `devices` table from before the column, which 0001 leaves alone
        // and 0002 has to extend.
        conn.execute_batch(
            "CREATE TABLE devices (device_id TEXT PRIMARY KEY, account_id TEXT NOT NULL);",
        )
        .unwrap();
        migrate_to(&mut conn, 1).unwrap();
        // The index name 0002 creates last, taken by a table, so the step
        // fails *after* its `ALTER TABLE` has run.
        conn.execute_batch("CREATE TABLE devices_by_vault_id (x);")
            .unwrap();

        assert!(migrate(&mut conn).is_err());

        assert_eq!(version(&conn).unwrap(), 1);
        let column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('devices') WHERE name = 'vault_device_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(column, 0, "the step's ALTER TABLE must have rolled back");
    }
}
