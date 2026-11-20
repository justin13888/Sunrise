//! The gate every SQLCipher key in a Sunrise process waits on.
//!
//! Both the client vault (`sunrise-storage`'s `Db`) and the relay's store
//! (`sunrise-server`) key SQLCipher connections, and both must wait for
//! SQLCipher to finish initialising before the first key (issue #521). This
//! crate holds that one wait, so neither has to depend on the other for it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// "SQLCipher" and "SQLite" are product names, not code, as in `sunrise-storage`.
#![allow(clippy::doc_markdown)]

use rusqlite::Connection;
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;

/// How many times [`ensure_cipher_ready`] keys its throwaway database before
/// it gives up, and how long it waits between tries: five seconds in all.
const CIPHER_READY_ATTEMPTS: u32 = 1_000;
const CIPHER_READY_BACKOFF: Duration = Duration::from_millis(5);

/// Whether SQLCipher finished initialising in this process. Settled once, by
/// the first key this process applies.
static CIPHER_READY: OnceLock<Result<(), String>> = OnceLock::new();

/// SQLCipher never finished initialising in this process, so no database can
/// be keyed.
///
/// SQLCipher itself reports this as a `PRAGMA key` failure whose message
/// ("PRAGMA key requires a key of one or more characters") blames the key, and
/// the key is not at fault (issue #521). It is terminal for the process:
/// SQLite does not run SQLCipher's initialisation a second time.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SQLCipher could not be initialised, so no database can be keyed: {detail}")]
pub struct CipherUnavailable {
    /// The last error keying a throwaway in-memory database returned.
    pub detail: String,
}

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
/// database then fails together: issue #521, seen as several tests of one
/// test binary failing at once on a loaded machine. It is not memory locking:
/// `cipher_memory_security` is off by default in SQLCipher 4.14, and an
/// `mlock` that fails is logged and ignored.
///
/// The gate keys a throwaway in-memory database until that succeeds, and
/// every other first opener waits on the `OnceLock` meanwhile. The first try
/// succeeds unless some connection that did not wait here is mid-way through
/// initialising SQLite at that moment; then the gate waits for it to finish.
/// Nothing here can call `sqlcipher_extra_init` itself, which would be the
/// exact barrier, because the workspace forbids `unsafe`.
///
/// The outcome is kept for the life of the process. A failure is too: SQLite
/// does not run SQLCipher's initialisation again, so a process whose cipher
/// failed to start can key nothing, and says so as [`CipherUnavailable`] with
/// the real error instead of blaming the key.
///
/// Call it before the first `PRAGMA key` or `ATTACH … KEY` on any connection.
///
/// # Errors
/// [`CipherUnavailable`], with the last error the probe returned.
pub fn ensure_cipher_ready() -> Result<(), CipherUnavailable> {
    CIPHER_READY
        .get_or_init(|| {
            await_cipher_ready(probe_cipher, CIPHER_READY_ATTEMPTS, || {
                std::thread::sleep(CIPHER_READY_BACKOFF);
            })
        })
        .clone()
        .map_err(|detail| CipherUnavailable { detail })
}

/// Key a fresh in-memory database. It succeeds exactly when SQLCipher has
/// finished initialising; the key is a constant and nothing is written.
fn probe_cipher() -> rusqlite::Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "PRAGMA key = \"x'0000000000000000000000000000000000000000000000000000000000000000'\"",
    )
}

/// Run `probe` until it succeeds, at most `attempts` times, calling `wait`
/// between tries. The error is the last one `probe` returned.
fn await_cipher_ready(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What SQLCipher returns for a key applied before it has initialised.
    fn uninitialised_cipher() -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some("An error occurred with PRAGMA key or rekey.".into()),
        )
    }

    #[test]
    fn the_cipher_gate_waits_out_an_initialisation_in_progress() {
        let mut probes = 0;
        let mut waits = 0;
        let ready = await_cipher_ready(
            || {
                probes += 1;
                if probes < 4 {
                    Err(uninitialised_cipher())
                } else {
                    Ok(())
                }
            },
            10,
            || waits += 1,
        );
        assert_eq!(ready, Ok(()));
        assert_eq!((probes, waits), (4, 3));
    }

    #[test]
    fn a_cipher_that_never_initialises_is_reported_as_unavailable_with_its_error() {
        let mut probes = 0;
        let detail = await_cipher_ready(
            || {
                probes += 1;
                Err(uninitialised_cipher())
            },
            3,
            || {},
        )
        .unwrap_err();
        assert_eq!(probes, 3, "the gate gives up after its attempts");
        assert!(detail.contains("failed 3 times"), "{detail}");
        assert!(detail.contains("PRAGMA key or rekey"), "{detail}");

        let e = CipherUnavailable { detail };
        assert!(
            e.to_string()
                .starts_with("SQLCipher could not be initialised"),
            "{e}"
        );
    }

    #[test]
    fn zero_attempts_report_that_none_was_made() {
        let detail = await_cipher_ready(|| Ok(()), 0, || {}).unwrap_err();
        assert!(detail.contains("no attempt was made"), "{detail}");
    }

    #[test]
    fn this_process_cipher_is_ready() {
        ensure_cipher_ready().unwrap();
        probe_cipher().unwrap();
    }
}
