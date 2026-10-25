//! The SQLite substrate the self-host server keeps its state in.
//!
//! This module owns the handle: one `Connection`, and a [`Store::open`] that
//! sets its pragmas and brings its schema to the newest version through the
//! numbered history in `store/migrations`.
//!
//! **Accounts and devices** are the tenant implemented here, and each half
//! declares its own tables beside the statements that write them.
//! `docs/06-server/auth.md` keys an account on the OIDC pair `(iss, sub)` and
//! hangs devices off it, so `accounts` owns the account row and the identity
//! material written onto it, and `devices` owns the device rows
//! `docs/06-server/api.md` describes together with the push tokens that cascade
//! off them.
//!
//! **The durable relay log** is the other tenant, and it is entirely
//! [`crate::relay_log`]'s: its tables are declared there, beside the only
//! methods that read or write them. Migration 0001 applies that DDL, because
//! one `Connection` opens one database — which is the only reason the relay is
//! named in this module at all.
//!
//! Backing file comes from [`crate::ServerConfig::sqlite_path`]; `None` opens
//! an in-memory database, which is what tests use — every `ServerState` then
//! owns a private database and no test can observe another's rows.
//!
//! **At rest** the file is SQLCipher-encrypted when `[storage] encrypt = true`
//! names a key file, and plain SQLite otherwise; ADR-0060 records why the
//! whole file and not chosen columns, and why a key file. `store/cipher.rs` holds the
//! key, the file classification and the one-way plaintext migration that
//! [`Store::open_keyed`] runs before anything else touches the file.
//!
//! # Concurrency
//!
//! One `Connection` behind a `parking_lot::Mutex`, held by both tenants. Every
//! statement *in this module* is a point lookup or a single-row write against a
//! small table, so the critical section is microseconds; a self-host relay
//! does not have the write volume to justify a pool, and a single connection
//! makes "revocation takes effect in the request's transaction" trivially
//! true. A multi-node deployment puts Postgres behind [`MetadataStore`], the
//! seam this `Store` is the first implementation of (ADR-0062 §1), and
//! `ServerState` holds the trait, not this type.
//!
//! The relay append path is the exception worth knowing about before reading
//! the paragraph above as the whole story: it takes this same mutex for a
//! multi-statement transaction and a retention sweep, so it holds the lock for
//! considerably longer than a point lookup does. [`crate::relay_log`] is where
//! that cost is described.

mod accounts;
mod blob_ledger;
mod cipher;
#[cfg(test)]
pub(crate) mod conformance;
mod devices;
mod error;
mod key_bundles;
mod lifecycle;
mod metadata;
mod migrations;
mod pragmas;
pub(crate) mod recovery;
mod snapshots;
mod tx;

use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;

// The suite in `tests.rs` reaches this through its `use super::*`; every
// statement that names a subject itself is in `accounts`.
#[cfg(test)]
use crate::auth::Subject;

pub use accounts::Account;
pub use blob_ledger::CommittedBlob;
pub use cipher::{pre_encryption_copy, DbKey};
pub use devices::{Device, NewDevice};
pub use error::{busy_total, StoreError};
pub use key_bundles::{KeyBundlePinned, PinnedKeyBundle};
pub use lifecycle::{AccountSummary, DeclaredCursor, NewTombstone, StoreStats};
pub use metadata::{MetadataError, MetadataStore};
pub use pragmas::DEFAULT_BUSY_TIMEOUT;
pub use recovery::{
    IdentityChainPinned, PinnedIdentityChain, RetiredRecoveryBlob, MAX_CHAIN_BYTES,
    RETIRED_CHECK_LEN,
};
pub use snapshots::PinnedSnapshot;
pub use tx::{metered, StoreTime};

/// The main database file plus its write-ahead log, in bytes.
///
/// The WAL is counted because under WAL mode it is where recent writes live
/// until a checkpoint, and an operator sizing a disk needs both. A file that
/// cannot be read contributes nothing.
#[must_use]
pub fn file_bytes(path: &Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    [path, Path::new(&wal)]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

/// SQLite-backed account/device store.
pub struct Store {
    /// Shared with [`crate::relay_log`], which puts the durable relay op log
    /// in this same database so one file — with its `-wal` beside it while the
    /// relay runs — is the whole server's state, and a `tar` of the stopped
    /// data dir is a consistent backup.
    pub(crate) conn: Mutex<Connection>,
    /// The file, or `None` in memory.
    path: Option<PathBuf>,
    /// The key the file is encrypted under, kept so a backup is written under
    /// the same one, and replaced by [`Store::rekey`]. `None` for a plaintext
    /// or in-memory database. Taken before `conn` wherever both are held.
    key: Mutex<Option<DbKey>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open the store with [`DEFAULT_BUSY_TIMEOUT`]. `None` opens a private
    /// in-memory database.
    pub fn open(path: Option<&Path>) -> Result<Self, StoreError> {
        Self::open_with(path, DEFAULT_BUSY_TIMEOUT)
    }

    /// Open the store, migrating it to the newest schema this binary knows.
    ///
    /// The order is what keeps a refused database untouched: the version is
    /// read before anything writes, and a database from a newer release is
    /// refused with [`StoreError::SchemaTooNew`] there. Only then are the
    /// journal and durability pragmas set — `journal_mode = WAL` rewrites the
    /// file header — the integrity check run and logged, and the migrations in
    /// `store/migrations` applied.
    pub fn open_with(path: Option<&Path>, busy_timeout: Duration) -> Result<Self, StoreError> {
        Self::open_keyed(path, busy_timeout, None)
    }

    /// [`Store::open_with`], with the file SQLCipher-encrypted under `key`.
    ///
    /// Before the steps `open_with` describes, the file's first bytes decide
    /// what the key does, and nothing is written until they have:
    ///
    /// - **no file**, with a key: the new database is created encrypted;
    /// - **a plaintext file**, with a key: it is encrypted in place, once, and
    ///   the original is kept beside it as `<file>.pre-encryption` — the
    ///   migration ADR-0060 settles, logged as `srv.store.encrypted`;
    /// - **an encrypted file** without a key: [`StoreError::KeyRequired`];
    ///   with the wrong one: [`StoreError::WrongKey`]. Both exit 78.
    ///
    /// The key is applied before any other statement, which SQLCipher
    /// requires. An in-memory database ignores the key: it has no rest to
    /// encrypt at.
    pub fn open_keyed(
        path: Option<&Path>,
        busy_timeout: Duration,
        key: Option<&DbKey>,
    ) -> Result<Self, StoreError> {
        // Checked before the file is opened, so a refused timeout creates no
        // database either.
        if busy_timeout.as_millis() > i32::MAX.unsigned_abs().into() {
            return Err(StoreError::BusyTimeoutTooLong {
                ms: busy_timeout.as_millis(),
                max: i32::MAX,
            });
        }
        let key = path.and(key);
        if let Some(p) = path {
            match (cipher::file_state(p)?, key) {
                (cipher::FileState::Plaintext, Some(k)) => {
                    cipher::encrypt_in_place(p, k, busy_timeout)?;
                    // No path field: a path is not on the log allowlist, and
                    // the copy's name is fixed beside the configured database.
                    tracing::warn!(
                        ev = "srv.store.encrypted",
                        "the relay database was encrypted; its plaintext original is kept beside \
                         it as sunrise.db.pre-encryption until the operator deletes it"
                    );
                }
                (cipher::FileState::Opaque, None) => {
                    return Err(StoreError::KeyRequired { path: p.to_owned() });
                }
                _ => {}
            }
        }
        let mut conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        if let Some(k) = key {
            cipher::apply_key(&conn, k)?;
        }
        // Set first, so that even the version read below waits out a peer's
        // lock rather than failing on it.
        conn.busy_timeout(busy_timeout)?;
        if let Some(p) = path {
            cipher::check_readable(&conn, p, key.is_some())?;
        }
        migrations::refuse_newer(&conn)?;
        pragmas::set_durability(&conn, path.is_some())?;
        // The `ON DELETE CASCADE` each tenant declares is inert unless foreign
        // keys are on, and a revoked device leaving its push tokens behind
        // would keep waking a device its owner believes is gone.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        pragmas::log_quick_check(&pragmas::quick_check(&conn, pragmas::QUICK_CHECK_BUDGET));
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.map(Path::to_owned),
            key: Mutex::new(key.cloned()),
        })
    }

    /// `SELECT 1` within `deadline`: the readiness probe's store check.
    ///
    /// The store is one connection behind a mutex, so the failure this can see
    /// is a wedged lock as much as a broken file. `try_lock_for` rather than a
    /// timeout around `lock`, so a wedged store does not leave the caller's
    /// thread parked behind it for every probe.
    ///
    /// # Errors
    /// [`StoreError::Busy`] when the connection is not free within `deadline`,
    /// or the query's own failure.
    pub fn ping(&self, deadline: Duration) -> Result<(), StoreError> {
        let conn = self.conn.try_lock_for(deadline).ok_or(StoreError::Busy)?;
        let one: i64 = conn.query_row("SELECT 1", [], |row| row.get(0))?;
        if one == 1 {
            Ok(())
        } else {
            Err(StoreError::Busy)
        }
    }

    /// `PRAGMA quick_check(1)`'s answer: `ok`, or the first problem found.
    /// The doctor's database check.
    ///
    /// # Errors
    /// The pragma's own failure.
    pub fn quick_check(&self) -> Result<String, StoreError> {
        Ok(self
            .conn
            .lock()
            .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?)
    }

    /// Write `len` bytes beside the database file, fsync, read them back,
    /// compare, and remove them: the doctor's check that the disk under the
    /// database takes a write. In memory there is no disk, and the check runs
    /// in the working directory, as it always has.
    ///
    /// # Errors
    /// The first I/O failure, or a read-back that differs from the write.
    pub fn probe_data_dir(&self, len: usize) -> std::io::Result<()> {
        use std::io::Write as _;
        let dir = self
            .path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let path = dir.join(".sunrise-doctor.tmp");
        // A pattern rather than zeros, so a filesystem that hands back a zeroed
        // page for an unwritten extent does not pass.
        let data: Vec<u8> = (0..=250u8).cycle().take(len).collect();
        let result = (|| {
            {
                let mut f = std::fs::File::create(&path)?;
                f.write_all(&data)?;
                f.sync_all()?;
            }
            if std::fs::read(&path)? != data {
                return Err(std::io::Error::other(
                    "read back differs from what was written",
                ));
            }
            Ok(())
        })();
        let removed = std::fs::remove_file(&path);
        result.and(removed)
    }

    /// Fold the write-ahead log back into the database file.
    ///
    /// Every acknowledged frame is already durable in the WAL, so this loses
    /// nothing if it fails; what it buys is a data directory that is one file
    /// once the relay stops, which is what `self-hosting.md` tells an operator
    /// to back up. `TRUNCATE` rather than `PASSIVE` because no request is left
    /// to contend with it.
    ///
    /// # Errors
    /// The checkpoint's own failure.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        Ok(self
            .conn
            .lock()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?)
    }
}

/// SQLite hands back `i64`; the row mappers in both halves want `u64`.
///
/// Private on purpose. In `store/mod.rs`, `super` is the crate root, so
/// `pub(super)` here would read as "the store's neighbours may call this" and
/// mean exactly `pub(crate)` — crate-wide reach for a helper with three call
/// sites, all of them inside `store`. The halves that need it are `accounts`
/// and `devices`, which are *children* of this module rather than siblings of
/// it, and a child can already reach an ancestor's private items: their
/// `use super::unsigned` compiles with no visibility modifier at all.
fn unsigned(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Mint a fresh 16-byte id, rendered Crockford base-32 per
/// `docs/06-server/api.md`.
///
/// ULID layout: the injected clock's millisecond stamp in the high 48 bits,
/// 80 bits of OS entropy below it. Timestamp-prefixed ids keep the SQLite
/// primary-key index appending rather than inserting into the middle, and the
/// entropy is what makes an id unguessable. `getrandom` is used directly rather
/// than a `rand` RNG because the determinism gate bans ambient `rand` sources
/// and there is nothing here worth seeding deterministically — an id a test
/// could predict would be an id an attacker could predict.
#[must_use]
pub(crate) fn mint_id(now_ms: u64) -> String {
    let mut random = [0u8; 10];
    if getrandom::getrandom(&mut random).is_err() {
        // No OS entropy is not a condition we can paper over with a
        // predictable id, so fall back to a value that is at least unique
        // per-process-per-ms and let the UNIQUE constraints catch collisions.
        random[..8].copy_from_slice(&now_ms.to_be_bytes());
    }
    let ulid = sunrise_id::ulid::Ulid::from_timestamp_and_random(now_ms, random);
    sunrise_id::crockford::encode_bytes(ulid.as_bytes())
}

#[cfg(test)]
mod tests;
