//! Whole-file SQLCipher encryption of the relay database, ADR-0060.
//!
//! The relay database is keyed with a 32-byte raw key read from the file
//! `[storage] key_file` names. A raw key, rather than a passphrase, because the
//! file is already 256 bits of randomness: SQLCipher's PBKDF2 would add a
//! second of startup and nothing an attacker has to pay for.
//!
//! What lives here is everything about the key and the file's encrypted state,
//! so [`super::Store::open_keyed`] reads as the order of its steps:
//!
//! - [`DbKey`]: reading the key file, and refusing one anyone but its owner
//!   can read;
//! - [`file_state`]: telling a plaintext SQLite file from one that is not;
//! - [`apply_key`] and [`check_readable`]: keying a connection, and turning
//!   SQLCipher's "file is not a database" into a typed refusal;
//! - [`encrypt_in_place`]: the one-way migration of a plaintext database;
//! - [`Store::backup_to`](super::Store::backup_to) and
//!   [`Store::rekey`](super::Store::rekey): the online copy, under the same
//!   key, and the rotation to a new one.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode};
use zeroize::Zeroizing;

use super::StoreError;

/// The first 16 bytes of every plaintext SQLite database.
///
/// SQLCipher encrypts page 1 whole, header included, so an encrypted file
/// starts with its random salt instead.
const PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// What the encryption step leaves beside the database: the plaintext file as
/// it was before the migration.
const PRE_ENCRYPTION_SUFFIX: &str = "pre-encryption";

/// Where an encryption in progress writes, so a crash leaves the database
/// itself untouched.
const ENCRYPTING_SUFFIX: &str = "encrypting";

/// A relay database key: 32 bytes, wiped when dropped.
///
/// `Debug` prints nothing of the key, so a store or config that derives it
/// cannot leak the key into a log line.
#[derive(Clone)]
pub struct DbKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(..)")
    }
}

impl PartialEq for DbKey {
    fn eq(&self, other: &Self) -> bool {
        // Not constant-time, and does not need to be: two keys are compared
        // only by an operator rotating from one to the other.
        self.0[..] == other.0[..]
    }
}

impl DbKey {
    /// A key from its 32 bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Read a key file: 64 hex digits, with surrounding whitespace allowed so
    /// that `openssl rand -hex 32 > key` is a valid one.
    ///
    /// # Errors
    /// [`StoreError::KeyPermissions`] when the group or others can read the
    /// file — it opens every byte of the database, so it is held to the same
    /// rule as the APNs signing key — and [`StoreError::KeyFile`] when it is
    /// missing, unreadable, or not 64 hex digits.
    pub fn from_file(path: &Path) -> Result<Self, StoreError> {
        let bad = |cause: String| StoreError::KeyFile {
            path: path.to_owned(),
            cause,
        };
        let meta = std::fs::metadata(path).map_err(|e| bad(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(StoreError::KeyPermissions {
                    path: path.to_owned(),
                    mode,
                });
            }
        }
        if !meta.is_file() {
            return Err(bad("not a regular file".to_owned()));
        }
        // Read at most a little more than a key's worth, so a key file
        // pointed at something huge by mistake is refused rather than read.
        let mut text = Zeroizing::new(String::new());
        std::fs::File::open(path)
            .and_then(|f| f.take(256).read_to_string(&mut text))
            .map_err(|e| bad(e.to_string()))?;
        let mut key = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(text.trim(), &mut key[..]).map_err(|_| {
            bad("not 64 hex digits; generate one with `openssl rand -hex 32`".to_owned())
        })?;
        Ok(Self(key))
    }

    /// The key `[storage] encrypt` and `[storage] key_file` name, for a
    /// database in `data_dir`, or `None` with encryption off.
    ///
    /// # Errors
    /// `encrypt` without a `key_file`, or a `key_file` without `encrypt` —
    /// the second because an operator who wrote a key file believes the
    /// database is encrypted, and it would not be. A key file inside the data
    /// dir, because every copy of the data dir would then carry the key that
    /// opens it. And each refusal [`DbKey::from_file`] makes.
    pub fn for_storage(
        encrypt: bool,
        key_file: Option<&Path>,
        data_dir: Option<&Path>,
    ) -> Result<Option<Self>, StoreError> {
        let key_file = match (encrypt, key_file) {
            (false, None) => return Ok(None),
            (false, Some(_)) => {
                return Err(StoreError::KeyConfig(
                    "[storage] key_file is set but encrypt is not: set encrypt = true to \
                     encrypt the database, or remove key_file",
                ))
            }
            (true, None) => {
                return Err(StoreError::KeyConfig(
                    "[storage] encrypt = true needs key_file: a file of 64 hex digits, mode \
                     0600, outside the data dir (openssl rand -hex 32)",
                ))
            }
            (true, Some(p)) => p,
        };
        // A bare `sunrise.db` has the empty path as its parent, which every
        // path "starts with"; its data dir is the working directory.
        let data_dir = data_dir.map(|d| {
            if d.as_os_str().is_empty() {
                Path::new(".")
            } else {
                d
            }
        });
        if let Some(data_dir) = data_dir {
            // Canonical where the path exists, so `..` and a symlinked data
            // dir cannot hide the key inside it.
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
            if canon(key_file).starts_with(canon(data_dir)) {
                return Err(StoreError::KeyInDataDir {
                    key_file: key_file.to_owned(),
                    data_dir: data_dir.to_owned(),
                });
            }
        }
        Self::from_file(key_file).map(Some)
    }

    /// The value `PRAGMA key` and `ATTACH … KEY` take: SQLCipher's raw-key
    /// literal, quoted.
    fn literal(&self) -> Zeroizing<String> {
        use std::fmt::Write as _;
        // Written into the zeroized buffer directly: `hex::encode` would
        // leave an unwiped copy of the key behind.
        let mut out = Zeroizing::new(String::with_capacity(4 + 64));
        out.push_str("\"x'");
        for b in self.0.iter() {
            let _ = write!(out, "{b:02x}");
        }
        out.push_str("'\"");
        out
    }

    /// `PRAGMA <pragma> = <literal>;`, in a zeroized buffer.
    fn statement(&self, pragma: &str) -> Zeroizing<String> {
        let mut out = Zeroizing::new(format!("PRAGMA {pragma} = "));
        out.push_str(&self.literal());
        out.push(';');
        out
    }
}

/// What a database file holds, read from its first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileState {
    /// No file, or an empty one: SQLite writes the header on first commit.
    Absent,
    /// A plaintext SQLite database.
    Plaintext,
    /// Anything else: an encrypted database, or not a database at all.
    Opaque,
}

/// Classify the file at `path`.
pub(super) fn file_state(path: &Path) -> Result<FileState, StoreError> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileState::Absent),
        Err(e) => return Err(io(path, &e)),
    };
    let mut head = Vec::with_capacity(16);
    (&mut file)
        .take(16)
        .read_to_end(&mut head)
        .map_err(|e| io(path, &e))?;
    Ok(if head.is_empty() {
        FileState::Absent
    } else if head == PLAINTEXT_HEADER {
        FileState::Plaintext
    } else {
        FileState::Opaque
    })
}

/// Key `conn`. SQLCipher requires this before any other statement touches
/// the database.
pub(super) fn apply_key(conn: &Connection, key: &DbKey) -> rusqlite::Result<()> {
    conn.execute_batch(&key.statement("key"))
}

/// Read page 1, which is the first moment SQLCipher tries the key.
///
/// A wrong key — and a missing one on an encrypted file — fails here with
/// `SQLITE_NOTADB`, which is turned into the refusal that names the remedy.
pub(super) fn check_readable(
    conn: &Connection,
    path: &Path,
    keyed: bool,
) -> Result<(), StoreError> {
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::NotADatabase => {
            Err(if keyed {
                StoreError::WrongKey {
                    path: path.to_owned(),
                }
            } else {
                StoreError::KeyRequired {
                    path: path.to_owned(),
                }
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// Where the one-way encryption keeps the plaintext original of the database at
/// `db`: `<db>.pre-encryption`, beside it.
#[must_use]
pub fn pre_encryption_copy(db: &Path) -> PathBuf {
    sibling(db, PRE_ENCRYPTION_SUFFIX)
}

/// `<path>.<suffix>`, beside the database.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Turn the plaintext database at `path` into an encrypted one under `key`,
/// keeping the original at `<path>.pre-encryption`. Returns that path.
///
/// One way. The steps, and what a crash at each leaves:
///
/// 1. The plaintext file is refused if a newer release wrote it, then taken
///    out of WAL mode, which checkpoints every committed write into the file
///    and fails while any other connection — a running relay — has it open.
///    A crash leaves a plaintext database.
/// 2. `sqlcipher_export` copies every table, index and row into
///    `<path>.encrypting`, and the schema version is copied after it, since
///    the export does not carry the header. A crash leaves the plaintext
///    database and a partial file the next attempt deletes.
/// 3. The plaintext file is hard-linked (copied, where the filesystem cannot
///    link) to `<path>.pre-encryption`.
/// 4. The encrypted file is renamed over `path`, the one atomic step. Before
///    it the database is plaintext; after it, encrypted.
pub(super) fn encrypt_in_place(
    path: &Path,
    key: &DbKey,
    busy_timeout: Duration,
) -> Result<PathBuf, StoreError> {
    let staging = sibling(path, ENCRYPTING_SUFFIX);
    let kept = pre_encryption_copy(path);
    remove_if_present(&staging)?;

    {
        let conn = Connection::open(path)?;
        conn.busy_timeout(busy_timeout)?;
        let version = super::migrations::refuse_newer(&conn)?;
        leave_wal(&conn, path)?;
        let staging_str = staging
            .to_str()
            .ok_or_else(|| rusqlite::Error::InvalidPath(staging.clone()))?;
        let mut attach = Zeroizing::new("ATTACH DATABASE ?1 AS encrypted KEY ".to_owned());
        attach.push_str(&key.literal());
        conn.execute(&attach, [staging_str])?;
        conn.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
        // `PRAGMA` takes no bound parameters; `version` is an integer this
        // function read and range-checked.
        conn.execute_batch(&format!("PRAGMA encrypted.user_version = {version};"))?;
        conn.execute_batch("DETACH DATABASE encrypted;")?;
    }
    std::fs::File::open(&staging)
        .and_then(|f| f.sync_all())
        .map_err(|e| io(&staging, &e))?;

    remove_if_present(&kept)?;
    if std::fs::hard_link(path, &kept).is_err() {
        std::fs::copy(path, &kept).map_err(|e| io(&kept, &e))?;
    }
    std::fs::rename(&staging, path).map_err(|e| io(path, &e))?;
    if let Some(dir) = path.parent() {
        // The rename is durable once the directory is. Not every platform
        // can open a directory to sync it; there the rename is as durable as
        // the filesystem makes it.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(kept)
}

/// Rewrite every page of the database `conn` holds under `new`.
///
/// SQLCipher's `rekey` rewrites the file in one transaction. It is run out of
/// WAL mode, which is also the proof that nothing else has the file open: a
/// running relay holding the old key would fail on every page it read
/// afterwards. The connection is put back in WAL mode after.
pub(super) fn rekey(conn: &Connection, path: &Path, new: &DbKey) -> Result<(), StoreError> {
    leave_wal(conn, path)?;
    let rekeyed = conn.execute_batch(&new.statement("rekey"));
    // Back to WAL whether or not the rekey took, so a failed rotation leaves
    // the store as it found it.
    let mode: rusqlite::Result<String> =
        conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0));
    rekeyed?;
    mode?;
    Ok(())
}

/// Switch `conn` to the rollback journal, which checkpoints the WAL into the
/// file and removes it.
///
/// SQLite allows the switch only to the last connection holding the file, and
/// refuses it with `SQLITE_BUSY` otherwise — once the busy timeout has run
/// out — which is what makes it the test that no other process, a running
/// relay included, has the database open.
fn leave_wal(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let in_use = || StoreError::InUse {
        path: path.to_owned(),
    };
    match conn.query_row("PRAGMA journal_mode = DELETE", [], |r| {
        r.get::<_, String>(0)
    }) {
        Ok(mode) if mode.eq_ignore_ascii_case("delete") => Ok(()),
        Ok(_) => Err(in_use()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::DatabaseBusy => {
            Err(in_use())
        }
        Err(e) => Err(e.into()),
    }
}

impl super::Store {
    /// Whether the file is SQLCipher-encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.key.lock().is_some()
    }

    /// Copy the database to a new file at `dest`, under the same key, while
    /// the relay keeps writing.
    ///
    /// SQLite's online backup API, run as **one step over every page**. That
    /// step reads from a single snapshot, so the copy is one committed instant
    /// of the database. In WAL mode a reader never blocks a writer, so the
    /// relay's appends proceed through the whole copy; what they cost is WAL
    /// growth, because a checkpoint cannot move past a snapshot still being
    /// read. Copying a page range per step instead would release the snapshot
    /// between steps, and the backup API restarts from the first page
    /// whenever another process writes the source in between — under a steady
    /// append load it would never finish.
    ///
    /// # Errors
    /// [`StoreError::Io`] if `dest` exists, and SQLite's failure otherwise. A
    /// failed copy may leave a partial file at `dest`.
    pub fn backup_to(&self, dest: &Path) -> Result<(), StoreError> {
        if dest.exists() {
            return Err(StoreError::Io {
                path: dest.to_owned(),
                cause: "already exists".to_owned(),
            });
        }
        let mut out = Connection::open(dest)?;
        let key = self.key.lock().clone();
        if let Some(k) = &key {
            apply_key(&out, k)?;
        }
        let conn = self.conn.lock();
        let backup = rusqlite::backup::Backup::new(&conn, &mut out)?;
        // A lock another process holds for an instant — a checkpoint, the
        // relay's write lock — is waited out, a bounded number of times.
        for _ in 0..BACKUP_BUSY_RETRIES {
            match backup.step(-1)? {
                rusqlite::backup::StepResult::Done => return Ok(()),
                // `More` cannot follow a step over every page, and is retried
                // like a lock if it ever does; `StepResult` is non-exhaustive.
                _ => std::thread::sleep(BACKUP_BUSY_PAUSE),
            }
        }
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("the database stayed locked through every backup attempt".to_owned()),
        )
        .into())
    }

    /// Re-encrypt the whole file under `new`, which takes effect for every
    /// later open. The relay must be stopped.
    ///
    /// # Errors
    /// [`StoreError::NotEncrypted`] for a plaintext database,
    /// [`StoreError::InUse`] while another process has it open, and SQLite's
    /// failure otherwise, in which case the old key still opens it.
    pub fn rekey(&self, new: &DbKey) -> Result<(), StoreError> {
        let mut key = self.key.lock();
        let (Some(path), Some(_)) = (&self.path, key.as_ref()) else {
            return Err(StoreError::NotEncrypted);
        };
        rekey(&self.conn.lock(), path, new)?;
        // The connection is under the new key now, and so is any backup this
        // store takes from here on.
        *key = Some(new.clone());
        Ok(())
    }
}

/// How many times [`Store::backup_to`](super::Store::backup_to) retries a step
/// another process's lock refused.
const BACKUP_BUSY_RETRIES: u32 = 100;

/// How long [`Store::backup_to`](super::Store::backup_to) waits before that
/// retry: with
/// [`BACKUP_BUSY_RETRIES`], five seconds in all, the default busy timeout.
const BACKUP_BUSY_PAUSE: Duration = Duration::from_millis(50);

fn remove_if_present(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, &e)),
    }
}

fn io(path: &Path, e: &std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_owned(),
        cause: e.to_string(),
    }
}

#[cfg(test)]
#[path = "cipher_tests.rs"]
mod tests;
