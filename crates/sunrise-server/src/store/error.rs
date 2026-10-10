//! [`StoreError`]: every way the SQLite store can fail, from opening the file
//! to the statements behind each method. What a request path sees of it is
//! [`super::MetadataError`], which keeps the refusals and folds the rest into
//! one retryable failure.

use std::path::PathBuf;

use thiserror::Error;

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
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
    /// The one connection was not free within the deadline a probe allowed.
    #[error("the database did not answer in time")]
    Busy,
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
