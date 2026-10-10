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
    Migration {
        id: 3,
        name: "account_and_blob_deletion",
        apply: account_and_blob_deletion,
    },
    Migration {
        id: 4,
        name: "relay_frames_n_ops",
        apply: relay_frames_n_ops,
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

/// 0003: the state account deletion and blob garbage collection keep, which
/// `store::lifecycle` declares beside the statements over it.
fn account_and_blob_deletion(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(super::lifecycle::SCHEMA)
}

/// 0004: `relay_frames.n_ops`, the ops each stored frame carries, so a replay
/// counts what it delivers into `sunrise_sync_ops_delivered_total` without
/// decoding the frame.
///
/// The frames already stored are counted here, once each, by decoding them:
/// retention bounds how many there are, and leaving them at the column's
/// default would under-count every replay of them for up to the 30 days
/// retention keeps them.
fn relay_frames_n_ops(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE relay_frames ADD COLUMN n_ops INTEGER NOT NULL DEFAULT 0;")?;
    // Counted first and written after, so no row is updated while the read
    // that found it is still stepping; only the counts are held, one frame's
    // bytes at a time.
    let mut counts: Vec<(i64, i64)> = Vec::new();
    {
        let mut read = conn.prepare("SELECT id, bytes FROM relay_frames")?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            let n_ops = crate::relay_log::frame_op_count(&row.get::<_, Vec<u8>>(1)?);
            if n_ops > 0 {
                counts.push((row.get(0)?, i64::try_from(n_ops).unwrap_or(i64::MAX)));
            }
        }
    }
    let mut write = conn.prepare("UPDATE relay_frames SET n_ops = ?2 WHERE id = ?1")?;
    for (id, n_ops) in counts {
        write.execute(rusqlite::params![id, n_ops])?;
    }
    Ok(())
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
/// column, foreign key, `CHECK` constraint, `AUTOINCREMENT` key and index,
/// sorted.
///
/// Not `sqlite_master.sql` as a whole, which is the text each object was
/// created with: `ALTER TABLE ... ADD COLUMN` appends to that text, so a
/// database that got a column by migration and one that got it in its
/// `CREATE TABLE` describe the same table in different words. Column *order*
/// differs between those two as well, and nothing in this crate reads a column
/// by position, so it is left out of the comparison.
///
/// What no pragma reports is read out of that text instead, piece by piece:
/// each `CHECK (...)` clause, whether `AUTOINCREMENT` appears, and a partial
/// index's `WHERE` clause, each with whitespace and comments removed so that a
/// frozen copy laid out differently from the constant still compares equal.
/// The indexes a `UNIQUE` or `PRIMARY KEY` constraint makes
/// (`sqlite_autoindex_*`) are compared by what they cover and how, not by
/// their numbered names.
#[cfg(test)]
pub(super) fn schema_of(conn: &Connection) -> Vec<String> {
    let objects: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT type, name, IFNULL(sql, '') FROM sqlite_master
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
    for (kind, name, sql) in objects {
        match kind.as_str() {
            "table" => {
                for c in rows(
                    "SELECT name || ' ' || type || ' notnull=' || \"notnull\" || ' default=' || \
                     IFNULL(dflt_value, '-') || ' pk=' || pk FROM pragma_table_info(?1)",
                    &name,
                ) {
                    out.push(format!("table {name} column {c}"));
                }
                for f in rows(
                    "SELECT \"from\" || ' -> ' || \"table\" || '(' || \"to\" || ') on_update=' || \
                     on_update || ' on_delete=' || on_delete FROM pragma_foreign_key_list(?1)",
                    &name,
                ) {
                    out.push(format!("table {name} fk {f}"));
                }
                let (checks, autoincrement) = table_constraints(&sql);
                for c in checks {
                    out.push(format!("table {name} check {c}"));
                }
                if autoincrement {
                    out.push(format!("table {name} autoincrement"));
                }
                for (index, unique, origin, partial) in conn
                    .prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1)")
                    .unwrap()
                    .query_map([&name], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
                {
                    let columns = rows(
                        "SELECT IFNULL(name, '<expr>') || CASE \"desc\" WHEN 1 THEN ' DESC' \
                         ELSE '' END || ' COLLATE ' || coll FROM pragma_index_xinfo(?1) \
                         WHERE key = 1 ORDER BY seqno",
                        &index,
                    );
                    // A constraint's index is named for its position among the
                    // table's constraints, which is not part of what it means.
                    let label = if origin == "c" { index.as_str() } else { "-" };
                    let clause = if partial == 1 {
                        let sql: String = conn
                            .query_row(
                                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
                                [&index],
                                |r| r.get(0),
                            )
                            .unwrap();
                        format!(" where {}", partial_clause(&sql))
                    } else {
                        String::new()
                    };
                    out.push(format!(
                        "table {name} index {label} origin={origin} unique={unique} ({}){clause}",
                        columns.join(", ")
                    ));
                }
            }
            // Indexes are listed under their table above.
            "index" => {}
            _ => out.push(format!("{kind} {name} {}", squeeze(&sql))),
        }
    }
    out.sort();
    out
}

/// `sql` with comments and whitespace outside string literals removed, so two
/// spellings of one definition that differ only in layout compare equal.
#[cfg(test)]
fn squeeze(sql: &str) -> String {
    strip(sql, false)
}

/// `sql` with comments removed, and each run of whitespace outside string
/// literals either collapsed to one space (`keep_space`) or removed.
#[cfg(test)]
fn strip(sql: &str, keep_space: bool) -> String {
    let mut out = String::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                out.push(c);
                for d in chars.by_ref() {
                    out.push(d);
                    if d == c {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            c if c.is_whitespace() => {
                if keep_space && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Every `CHECK (...)` clause in a `CREATE TABLE` statement, squeezed, and
/// whether the statement declares an `AUTOINCREMENT` key.
///
/// Read with comments stripped, so a keyword inside a comment does not count.
/// String literals and quoted identifiers are stepped over, so one that spells
/// either keyword does not count either.
#[cfg(test)]
fn table_constraints(sql: &str) -> (Vec<String>, bool) {
    let text = strip(sql, true);
    let upper = text.to_ascii_uppercase();
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let keyword_at = |i: usize, kw: &str| {
        upper[i..].starts_with(kw)
            && (i == 0 || !word(bytes[i - 1]))
            && bytes.get(i + kw.len()).is_none_or(|&b| !word(b))
    };
    let mut checks = Vec::new();
    let mut autoincrement = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if matches!(c, b'\'' | b'"' | b'`') {
            i += 1;
            while i < bytes.len() && bytes[i] != c {
                i += 1;
            }
            i += 1;
            continue;
        }
        if keyword_at(i, "AUTOINCREMENT") {
            autoincrement = true;
        }
        if keyword_at(i, "CHECK") {
            let mut start = i + "CHECK".len();
            while bytes.get(start) == Some(&b' ') {
                start += 1;
            }
            let mut depth = 0usize;
            let mut j = start;
            while j < bytes.len() {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                    }
                    q @ (b'\'' | b'"' | b'`') => {
                        j += 1;
                        while j < bytes.len() && bytes[j] != q {
                            j += 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            let end = (j + 1).min(text.len());
            checks.push(squeeze(&text[start..end]));
            i = end;
            continue;
        }
        i += 1;
    }
    (checks, autoincrement)
}

/// The `WHERE` clause of a partial `CREATE INDEX` statement, squeezed.
#[cfg(test)]
fn partial_clause(sql: &str) -> String {
    let text = squeeze(sql);
    // The index's own column list closes before its `WHERE`, so the clause is
    // everything after the first `)WHERE`.
    let upper = text.to_ascii_uppercase();
    upper
        .find(")WHERE")
        .map_or(text.clone(), |at| text[at + ")WHERE".len()..].to_owned())
}

#[cfg(test)]
mod tests;
