//! Keying a connection, and the gate that waits for SQLCipher to be able to.

use super::{Db, DbError};
use rusqlite::Connection;
use std::sync::OnceLock;
use std::time::Duration;
use sunrise_crypto::keys::VaultRootKey;

impl Db {
    pub(crate) fn apply_sqlcipher_key(
        conn: &Connection,
        vault_root: &VaultRootKey,
    ) -> Result<(), DbError> {
        ensure_cipher_ready()?;
        // Derive a 32-byte SQLCipher raw key from vault_root.
        let raw = sunrise_crypto::derive_key("sunrise.sqlcipher_key.v1", vault_root.as_bytes(), 32);
        let mut hex_buf = String::with_capacity(64);
        for b in &raw {
            use core::fmt::Write;
            let _ = write!(hex_buf, "{b:02x}");
        }
        // SQLCipher's `PRAGMA key = "x'<64 hex>'"` accepts a raw key.
        let key_pragma = format!("PRAGMA key = \"x'{hex_buf}'\"");
        conn.execute_batch(&key_pragma)?;
        // Pre-derived: skip SQLCipher's own KDF iterations.
        conn.execute_batch("PRAGMA kdf_iter = 1;")?;
        Ok(())
    }
}

/// How many times [`ensure_cipher_ready`] keys its throwaway database before
/// it gives up, and how long it waits between tries: five seconds in all.
const CIPHER_READY_ATTEMPTS: u32 = 1_000;
const CIPHER_READY_BACKOFF: Duration = Duration::from_millis(5);

/// Whether SQLCipher finished initialising in this process. Settled once, by
/// the first key this process applies.
static CIPHER_READY: OnceLock<Result<(), String>> = OnceLock::new();

/// Wait until SQLCipher can key a database, once per process.
///
/// SQLite marks itself initialised *before* it runs SQLCipher's own
/// initialisation (`sqlite3_initialize` sets `isInit`, then calls
/// `SQLITE_EXTRA_INIT`, which is `sqlcipher_extra_init`: OpenSSL provider,
/// private heap, random shield mask). A thread that calls into SQLite in that
/// window returns from `sqlite3_initialize` at once, opens its connection,
/// and has `PRAGMA key` refused by `sqlcipherCodecAttach` with
/// `SQLITE_ERROR`. SQLCipher logs `sqlcipherCodecAttach: sqlcipher not
/// initialized 1` to stderr and returns the message "PRAGMA key requires a
/// key of one or more characters", which is about a key nobody passed. The
/// window is the first moments of a process, so every thread that opens a
/// vault then fails together: issue #521, seen as several tests of one test
/// binary failing at once on a loaded machine. It is not memory locking:
/// `cipher_memory_security` is off by default in SQLCipher 4.14, and an
/// `mlock` that fails is logged and ignored.
///
/// The gate keys a throwaway in-memory database until that succeeds, and
/// every other first opener waits on the `OnceLock` meanwhile. The first try
/// succeeds unless some connection outside this crate is mid-way through
/// initialising SQLite at that moment; then the gate waits for it to finish.
/// Nothing here can call `sqlcipher_extra_init` itself, which would be the
/// exact barrier, because the workspace forbids `unsafe`.
///
/// The outcome is kept for the life of the process. A failure is too: SQLite
/// does not run SQLCipher's initialisation again, so a process whose cipher
/// failed to start can key nothing, and says so as
/// [`DbError::CipherUnavailable`] with the real error instead of blaming the
/// key.
///
/// Public for code that keys `SQLCipher` connections without going through
/// [`Db`], such as the relay's store: call it before the first `PRAGMA key`
/// or `ATTACH … KEY`. [`Db`]'s own opens already do.
///
/// # Errors
/// Only [`DbError::CipherUnavailable`].
pub fn ensure_cipher_ready() -> Result<(), DbError> {
    CIPHER_READY
        .get_or_init(|| {
            await_cipher_ready(probe_cipher, CIPHER_READY_ATTEMPTS, || {
                std::thread::sleep(CIPHER_READY_BACKOFF);
            })
        })
        .clone()
        .map_err(|detail| DbError::CipherUnavailable { detail })
}

/// Key a fresh in-memory database. It succeeds exactly when SQLCipher has
/// finished initialising; the key is a constant and nothing is written.
pub(super) fn probe_cipher() -> rusqlite::Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "PRAGMA key = \"x'0000000000000000000000000000000000000000000000000000000000000000'\"",
    )
}

/// Run `probe` until it succeeds, at most `attempts` times, calling `wait`
/// between tries. The error is the last one `probe` returned.
pub(super) fn await_cipher_ready(
    mut probe: impl FnMut() -> rusqlite::Result<()>,
    attempts: u32,
    mut wait: impl FnMut(),
) -> Result<(), String> {
    let mut last = String::from("no attempt was made");
    for attempt in 0..attempts {
        if attempt > 0 {
            wait();
        }
        match probe() {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!(
        "keying an in-memory database failed {attempts} times; the last error: {last}"
    ))
}
