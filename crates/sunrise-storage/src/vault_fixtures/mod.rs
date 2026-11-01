//! Real old vaults, checked in, and the chain run over them.
//!
//! Everything else in this crate tests migrations against a schema this build
//! just built. `db.rs` replays `MIGRATIONS[n].sql` by hand into a fresh
//! in-memory connection, and `tests/vault_at_rest.rs` opens a *current* vault
//! whose version stamp has been lied down. Neither is a 13-era vault: the
//! first never goes through `Db::open`, and the second has every column the
//! migrations were supposed to add already present. So until this module the
//! seven migrations from 13 to 20 had never been run against the thing they
//! exist for — old data, written by an old schedule, opened by this build.
//!
//! A fixture is the only honest way to close that. Each one is an encrypted
//! `SQLCipher` vault, produced by `Db::create_at_storage_v` at some historical
//! `STORAGE_V`, seeded with rows this module also states as expectations, and
//! committed. The tests copy it to a temporary directory, call the ordinary
//! public `Db::open` on the copy, and assert on the *contents* afterwards —
//! not on the fact that the open returned `Ok`. A migration that drops a
//! column's data opens perfectly.
//!
//! # The key is committed, on purpose
//!
//! `FIXTURE_VAULT_ROOT` is the vault root key for every file under
//! `crates/sunrise-storage/fixtures/`, and it is in this repository in plain
//! sight because that is what a test vault is for. There is nothing behind it:
//! the rows are the invented ones in this module, and no user, device or account has
//! ever been keyed with it. It is **not** a leaked secret and it must not be
//! "fixed" — rotating it would only mean regenerating the fixtures under a
//! different constant that is equally public, and the same note is repeated in
//! `fixtures/README.md` so that a reader who finds the binary before the code
//! learns it there too.
//!
//! # Writing a missing fixture
//!
//! `mise run storage-fixtures`, which runs `write_the_missing_vault_fixtures`
//! — an `#[ignore]`d test, so an ordinary `cargo test` reads the committed
//! files and never writes them. It writes only a fixture that is not already
//! committed, and never rewrites one: a fixture rewritten at today's schema
//! stops being old, and the chain it is supposed to exercise then runs over
//! nothing. To rewrite one deliberately, delete it first. There is no reason
//! to rewrite the v13 file at all short of the baseline itself changing,
//! which ADR-0018 says happens once and has happened.
//!
//! # Which versions get a fixture
//!
//! Every one: a fixture for each `STORAGE_V` from the baseline to the one
//! before this build's (`docs/04-storage/migrations.md` §Migration rigor rule
//! 1). `every_migration_has_a_fixture_at_the_version_before_it` is the gate: a
//! migration appended without one fails it, and `mise run storage-fixtures`
//! writes the one it asks for.
//!
//! Two kinds of seed fill them.
//!
//! - **Curated.** The v13 and v17 files were written first, by `seed_v13` and
//!   `seed_v17`, each shaped around the migrations that move data the other
//!   cannot contain: 0014's backfill and 0017's moves for 13, and 0019's,
//!   0022's and 0023's `identity` backfills for 17, because 0017 creates
//!   `identity` empty and so no vault that started at 13 has a row for them.
//!   The tests named after those two files assert what each migration does to
//!   those rows.
//! - **Every table.** Every other version is filled by `seed_every_table`
//!   from [`SEED`](seed::SEED): one or more rows in every table that exists at
//!   that version, with a value in every column it names. A column the version
//!   does not have yet is left out, so one list fills every version.
//!   `the_seed_reaches_every_table_and_column_this_build_creates` keeps it
//!   complete, so a table a later migration adds is filled in every fixture
//!   written after it.
//!
//! Over all of them, `every_fixture_keeps_every_row_through_every_later_migration`
//! reads every row of every table before the open and checks each one is still
//! there afterwards, column for column. The only differences it allows are the
//! migrations' deliberate ones, listed in `chain::DELIBERATE_CHANGES`, each
//! with the migration that makes it.
//!
//! # Where things are
//!
//! This file holds the helpers every test shares and the generator. The
//! generator's test stays here because `mise run storage-fixtures` runs it by
//! its exact path.
//!
//! - [`contents`]: the values the curated seeds write and the tests assert.
//! - [`curated`]: the v13 and v17 seeds.
//! - [`seed`]: the one list every other fixture is filled from.
//! - [`chain`]: the checks over every fixture and every later migration.
//! - [`from_v13`] and [`from_v17`]: what the chain does to the curated rows.
//! - [`later_migrations`]: one test per migration from 0041 on, over the
//!   fixture at the version before it.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use sunrise_cbor::version::STORAGE_V;
use sunrise_crypto::keys::VaultRootKey;

use crate::db::Db;
use crate::migrations::{BASELINE_STORAGE_V, MIGRATIONS};

mod chain;
mod contents;
mod curated;
mod from_v13;
mod from_v17;
mod later_migrations;
mod seed;

use curated::{seed_v13, seed_v17};
use seed::seed_every_table;

/// The vault root every committed fixture is keyed with.
///
/// Public in the repository on purpose; see this module's header.
const FIXTURE_VAULT_ROOT: [u8; 32] = [0x7e; 32];

/// A vault written when `STORAGE_V` was 13, the ADR-0018 baseline.
const V13_FIXTURE: &str = "vault_storage_v13.db";

/// A vault written when `STORAGE_V` was 17, the key hierarchy (ADR-0024).
const V17_FIXTURE: &str = "vault_storage_v17.db";

/// The committed fixture written at `storage_v`.
fn fixture_name(storage_v: u32) -> String {
    format!("vault_storage_v{storage_v}.db")
}

/// Every version a fixture must exist for: each version a migration leaves
/// a vault at, from the baseline through the one before this build's. The
/// current version needs none, because nothing migrates from it yet.
///
/// The versions are the migration ids, not every integer below `STORAGE_V`:
/// a migration number reserved by a parallel change that has not landed yet
/// is a version no vault was ever at (`docs/04-storage/migrations.md`), and
/// a fixture "written at" it would be one at the version before it, stamped
/// wrong.
fn fixture_versions() -> Vec<u32> {
    MIGRATIONS
        .iter()
        .map(|m| m.id)
        .filter(|&v| v >= BASELINE_STORAGE_V && v < u32::from(STORAGE_V))
        .collect()
}

/// The version a vault stood at before migration `id` ran: the id of the
/// migration before it in [`MIGRATIONS`].
fn version_before(id: u32) -> Option<u32> {
    MIGRATIONS.iter().map(|m| m.id).filter(|&v| v < id).max()
}

/// The directory the committed fixtures live in.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fixture_key() -> VaultRootKey {
    VaultRootKey::from_bytes(FIXTURE_VAULT_ROOT)
}

/// Copy `name` out of the committed fixtures and open the copy through the
/// ordinary public entry point, applying whatever migrations it needs.
///
/// The copy is not politeness: `Db::open` migrates in place, so a test that
/// opened the committed file would upgrade it and every later run would be
/// reading a current vault while believing it was reading an old one. The
/// `TempDir` is returned because dropping it deletes the copy.
fn open_migrated(name: &str) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let source = fixture_dir().join(name);
    fs::copy(&source, &path).unwrap_or_else(|e| {
        panic!(
            "copy {}: {e} — run `mise run storage-fixtures`",
            source.display()
        )
    });
    let db = Db::open(&path, &fixture_key())
        .unwrap_or_else(|e| panic!("the migration chain must open `{name}`: {e}"));
    (dir, db)
}

fn stamped_storage_v(db: &Db) -> u32 {
    db.conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
        .expect("read schema_meta")
}

fn table_exists(db: &Db, table: &str) -> bool {
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            rusqlite::params![table],
            |r| r.get(0),
        )
        .expect("probe sqlite_master");
    n == 1
}

fn row_count(db: &Db, table: &str) -> i64 {
    db.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

// --- the generator --------------------------------------------------------

/// Write one fixture: a vault at `storage_v`, seeded, checkpointed, single-file.
fn write_fixture(name: &str, storage_v: u32, seed: fn(&Connection)) {
    let dir = fixture_dir();
    fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join(name);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.join(format!("{name}{suffix}")));
    }

    let db = Db::create_at_storage_v(&path, &fixture_key(), storage_v)
        .expect("create the fixture vault");
    seed(db.conn());
    assert_eq!(
        stamped_storage_v(&db),
        storage_v,
        "the stamp is the fixture"
    );
    // WAL leaves `-wal`/`-shm` beside the database. A fixture is one file or
    // it is not a fixture: half of it would be uncommitted and the committed
    // half would be missing every row.
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
    drop(db);
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.join(format!("{name}{suffix}"));
        assert!(
            !sidecar.exists() || fs::metadata(&sidecar).expect("stat").len() == 0,
            "`{}` still holds data; the fixture is incomplete",
            sidecar.display()
        );
        let _ = fs::remove_file(&sidecar);
    }
}

/// Whether the fixture at `storage_v` has a curated seed of its own rather
/// than [`SEED`](seed::SEED).
const fn is_curated(storage_v: u32) -> bool {
    matches!(storage_v, 13 | 17)
}

/// The seed a fixture at `storage_v` is filled with.
fn seed_for(storage_v: u32) -> fn(&Connection) {
    match storage_v {
        13 => seed_v13,
        17 => seed_v17,
        _ => seed_every_table,
    }
}

/// Write every fixture [`fixture_versions`] asks for that is not committed
/// yet, and leave every committed one alone.
///
/// `#[ignore]`d: an ordinary `cargo test` must read the committed files, not
/// write them. Run it through `mise run storage-fixtures`. To rewrite a
/// fixture on purpose, delete it first; see this module's header for why that
/// is rare.
#[test]
#[ignore = "writes the committed fixtures; run it through `mise run storage-fixtures`"]
fn write_the_missing_vault_fixtures() {
    for v in fixture_versions() {
        let name = fixture_name(v);
        if fixture_dir().join(&name).exists() {
            continue;
        }
        write_fixture(&name, v, seed_for(v));
    }
}
