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
//! true. A multi-node deployment replaces this module with Postgres.
//!
//! The relay append path is the exception worth knowing about before reading
//! the paragraph above as the whole story: it takes this same mutex for a
//! multi-statement transaction and a retention sweep, so it holds the lock for
//! considerably longer than a point lookup does. [`crate::relay_log`] is where
//! that cost is described.

mod accounts;
mod cipher;
mod devices;
mod lifecycle;
mod migrations;
mod pragmas;
mod tx;

use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::Connection;
use thiserror::Error;

// The suite in `tests.rs` reaches this through its `use super::*`; every
// statement that names a subject itself is in `accounts`.
#[cfg(test)]
use crate::auth::Subject;

pub use accounts::Account;
pub use cipher::{pre_encryption_copy, DbKey};
pub use devices::{Device, NewDevice};
pub use lifecycle::{AccountSummary, DeclaredCursor, NewTombstone, StoreStats};
pub use pragmas::DEFAULT_BUSY_TIMEOUT;
pub use tx::{metered, StoreTime};

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[source] rusqlite::Error),
    /// The caller's `(iss, sub)` has no account and `allow_signup` is false.
    #[error("sign-up is disabled on this server")]
    SignupDisabled,
    /// No such row for this account.
    #[error("not found")]
    NotFound,
    /// A `recovery_blob` was offered for an account that already holds a
    /// different one.
    ///
    /// The column is write-once. It used to be silently so: `set_identity`
    /// wrote `COALESCE(?4, recovery_blob)`, so a second, different blob was
    /// accepted with a `201` and dropped on the floor — and the client that
    /// sent it had just shown its user a recovery code for a blob the server
    /// does not hold. Refusing is what makes a displayed code trustworthy;
    /// re-sending the *same* bytes is still idempotent and still succeeds.
    #[error("this account already holds a different recovery blob")]
    RecoveryBlobExists,
    /// The database's schema version is one this binary does not know.
    ///
    /// A newer release migrated the file, or something other than this relay
    /// stamped it. The store refuses it before writing anything, the journal
    /// mode included, and the binary exits 78 (`EX_CONFIG`) so a supervisor
    /// does not restart it into the same refusal.
    #[error(
        "the database is at schema version {found}, and this binary supports up to {supported}: \
         run the release that wrote it, or restore a backup taken before that upgrade"
    )]
    SchemaTooNew {
        /// The database's `PRAGMA user_version`.
        found: i64,
        /// The newest version this binary migrates to.
        supported: u32,
    },
    /// A busy timeout longer than SQLite can hold.
    ///
    /// `sqlite3_busy_timeout` takes a C `int` of milliseconds, and rusqlite
    /// panics on a `Duration` above `i32::MAX` of them rather than returning
    /// an error. Refusing it here is what turns `[storage] busy_timeout_ms =
    /// 3000000000` into exit 78 and `srv.start.refused` instead of a crash.
    #[error(
        "the SQLite busy timeout is {ms} ms, and SQLite takes at most {max} ms (about 24 days): \
         lower [storage] busy_timeout_ms"
    )]
    BusyTimeoutTooLong {
        /// The timeout asked for, in milliseconds.
        ms: u128,
        /// The longest SQLite accepts, `i32::MAX` milliseconds.
        max: i32,
    },
    /// The key file could not be read, or does not hold a key.
    #[error("[storage] key_file {}: {cause}", path.display())]
    KeyFile {
        /// The file.
        path: PathBuf,
        /// What was wrong with it.
        cause: String,
    },
    /// The key file can be read by its group or by others.
    #[error(
        "[storage] key_file {} has mode {mode:o}: it opens the whole database, so only its \
         owner may read it — chmod 600",
        path.display()
    )]
    KeyPermissions {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// The key file lies inside the data dir, so every backup of the data dir
    /// would carry the key beside the ciphertext it opens.
    #[error(
        "[storage] key_file {} is inside the data dir {}: keep it outside, so a copy of the \
         data dir is not also a copy of its key",
        key_file.display(),
        data_dir.display()
    )]
    KeyInDataDir {
        /// The key file.
        key_file: PathBuf,
        /// The data dir it is inside.
        data_dir: PathBuf,
    },
    /// `[storage] encrypt` and `[storage] key_file` disagree.
    #[error("{0}")]
    KeyConfig(&'static str),
    /// The database is not a plaintext SQLite file and no key was given: it
    /// is encrypted, or is not a database at all.
    #[error(
        "{} is encrypted (or is not a SQLite database): set [storage] encrypt = true and the \
         key_file it was encrypted with",
        path.display()
    )]
    KeyRequired {
        /// The database.
        path: PathBuf,
    },
    /// The key does not open the database.
    #[error(
        "the key in [storage] key_file does not open {}: name the key it was encrypted with, \
         or restore a backup taken under this one",
        path.display()
    )]
    WrongKey {
        /// The database.
        path: PathBuf,
    },
    /// An operation that needs the database to itself found another
    /// connection holding it.
    #[error(
        "{} is open in another process: stop the relay (and any other admin command) first",
        path.display()
    )]
    InUse {
        /// The database.
        path: PathBuf,
    },
    /// The database is not encrypted, so there is no key to rotate.
    #[error("the database is not encrypted: set [storage] encrypt = true and start once first")]
    NotEncrypted,
    /// A file beside the database could not be read or written.
    #[error("{}: {cause}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The operating system's reason.
        cause: String,
    },
}

/// The count [`busy_total`] reads.
static BUSY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Statements that failed with `SQLITE_BUSY` once the busy timeout ran out,
/// across every store in this process.
///
/// Counted where a `rusqlite` error becomes a [`StoreError`], the one place
/// every store failure passes and none of them holds a registry. That point
/// is reached only after SQLite's busy handler has retried for the whole
/// `[storage] busy_timeout_ms`, so a count here is a write another connection
/// held off for longer than the operator allowed, not a retry that succeeded.
/// Process-wide rather than per store because the contention is: a second
/// connection to the file is another process (`admin`, a backup), not another
/// [`Store`] in this one. `/metrics` copies it into `sunrise_db_busy_total`.
#[must_use]
pub fn busy_total() -> u64 {
    BUSY.load(std::sync::atomic::Ordering::Relaxed)
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) {
            BUSY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Self::Sqlite(e)
    }
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
