//! The rows every fixture after v17 is filled with.
//!
//! One list for every version. [`seed_every_table`] inserts each row into its
//! table if the table exists at the fixture's version, with only the columns
//! the table has at that version. So the v14 fixture gets the pre-0017
//! `stream_keys` shape and no `identity` row, and the v31 fixture gets every
//! table this list names.
//!
//! The values are chosen to be told apart, not to be realistic. Nearly every
//! column holds a non-default value, because a migration that rewrote a column
//! to its default would go unnoticed on a row that already held the default.
//! Ids agree across tables wherever a foreign key or a reader joins them: the
//! task is in the stream, the block holds the task, the outbox row names the
//! op.
//!
//! `the_seed_reaches_every_table_and_column_this_build_creates` fails when a
//! migration adds a table this list does not fill, or when a column named here
//! exists in no version, which is what a typo would otherwise do silently.
//!
//! The list is cut into contiguous runs, one file each, and [`SEED`] puts them
//! back in the order they were cut from. A run is named for what most of it
//! holds, not for all of it: a table stays where foreign-key order has it.

use rusqlite::types::ToSqlOutput;
use rusqlite::types::ValueRef;
use rusqlite::Connection;

mod entities;
mod keys;
mod merge;
mod oplog;
mod vault_state;

// The ids more than one run names, or a test outside this module reads.
const TASK_BLOCKER: [u8; 16] = [0x03; 16];
const ATTACHMENT: [u8; 16] = [0x09; 16];
const BLOB_ID: [u8; 16] = [0x0b; 16];
/// The old Inbox sentinel's id, which 0044 reads as "no stream".
pub(super) const INBOX_SENTINEL: [u8; 16] = *b"\x00\x00\x00sunrise.inbox";
/// A task, and a block, in the Inbox sentinel.
pub(super) const TASK_NO_STREAM: [u8; 16] = [0x2d; 16];
pub(super) const BLOCK_NO_STREAM: [u8; 16] = [0x2e; 16];

/// A stamp one millisecond into each row's own range, so no two columns
/// share a value by accident.
const HLC_MS: i64 = 1_700_000_000_001;

/// One row: its table, and a value for each column it names.
pub(super) struct SeedRow {
    pub(super) table: &'static str,
    pub(super) values: &'static [(&'static str, ValueRef<'static>)],
}

/// Every row, in insertion order: a row comes after the rows its foreign keys
/// name. Read it through [`seed_rows`].
pub(super) const SEED: &[&[SeedRow]] = &[
    entities::ROWS,
    oplog::ROWS,
    keys::ROWS,
    merge::ROWS,
    vault_state::ROWS,
];

/// Every row of [`SEED`], in insertion order.
pub(super) fn seed_rows() -> impl Iterator<Item = &'static SeedRow> {
    SEED.iter().copied().flatten()
}

/// The columns `table` has in `conn`'s schema, or `None` if it has no such
/// table.
pub(super) fn columns(conn: &Connection, table: &str) -> Option<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?)")
        .expect("prepare pragma_table_info");
    let names = stmt
        .query_map([table], |r| r.get::<_, String>(0))
        .expect("pragma_table_info")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read pragma_table_info");
    (!names.is_empty()).then_some(names)
}

/// Fill a vault at whatever version `conn` holds from [`SEED`]: every row
/// whose table exists, with every column that exists.
pub(super) fn seed_every_table(conn: &Connection) {
    for row in seed_rows() {
        let Some(present) = columns(conn, row.table) else {
            continue;
        };
        let values: Vec<&(&str, ValueRef<'static>)> = row
            .values
            .iter()
            .filter(|(column, _)| present.iter().any(|p| p == column))
            .collect();
        let names: Vec<&str> = values.iter().map(|(c, _)| *c).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            row.table,
            names.join(", "),
            vec!["?"; names.len()].join(", ")
        );
        let inserted = conn
            .execute(
                &sql,
                rusqlite::params_from_iter(values.iter().map(|(_, v)| ToSqlOutput::Borrowed(*v))),
            )
            .unwrap_or_else(|e| panic!("seed `{}`: {e}", row.table));
        assert_eq!(inserted, 1, "seed `{}`", row.table);
    }
}
