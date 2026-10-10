//! What every relay connection runs under, and the integrity check run when
//! one opens.
//!
//! These are the connection's settings rather than its schema: the schema is
//! `store/migrations`, and nothing here creates or alters a table. They sit
//! together because they share one reason to change — how the relay trades
//! durability, contention and startup time against each other — and because
//! the store's open path calls them in one place, after the version check and
//! before the migrations.

use std::time::Duration;

use rusqlite::Connection;

/// How long a statement waits on a lock another connection holds before it
/// fails with `SQLITE_BUSY`, when the config does not say.
///
/// The relay's own statements share one connection and never contend with
/// each other; what this waits out is a second process on the same file — the
/// `sqlite3` shell, a backup tool, a relay started twice — and the moment a WAL
/// checkpoint holds the file. Five seconds rides out any of those without
/// letting a wedged peer stall a request indefinitely.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The most time the startup integrity check may take before it is cut off.
///
/// `PRAGMA quick_check` reads every page, so its cost grows with the
/// database. Ten seconds covers a self-host database of several gigabytes on
/// ordinary disk; past that the check is interrupted and reported as
/// unfinished, because a relay that will not start until a scan of an
/// arbitrarily large file finishes is an outage the check was meant to
/// prevent.
pub(super) const QUICK_CHECK_BUDGET: Duration = Duration::from_secs(10);

/// Put the database in WAL mode, with the `synchronous` level that is safe
/// for whichever journal it actually got.
///
/// **WAL** lets the readers of a checkpoint and a second process proceed
/// while the relay writes, and turns each commit into one sequential append
/// instead of a journal file written, synced and deleted.
///
/// **`synchronous = NORMAL`** under WAL syncs at checkpoints rather than at
/// every commit. The trade-off, stated plainly: a power cut or kernel crash
/// can roll back the last transactions committed before it, but cannot
/// corrupt the database; a crash of the relay process alone loses nothing.
/// For the relay that window means a frame acknowledged just before the power
/// cut can be gone after it, when its sender has already dropped it from its
/// outbox on the ack. The window is real, and it is the one every WAL
/// deployment of SQLite accepts. Rollback-journal mode gets `FULL`, because
/// `NORMAL` there can corrupt the file on power loss.
///
/// `wal_autocheckpoint` keeps SQLite's default of 1000 pages (about 4 MiB):
/// the relay's writes are small frames, so the WAL is checkpointed every few
/// thousand appends and never grows large enough to slow reads.
///
/// A file that cannot be put in WAL mode — a filesystem without shared
/// memory, such as some network mounts — keeps working in the journal it has,
/// and says so in the log. An in-memory database reports `memory` and is not
/// worth a warning.
pub(super) fn set_durability(conn: &Connection, file_backed: bool) -> rusqlite::Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        conn.execute_batch("PRAGMA synchronous = NORMAL;")
    } else {
        if file_backed {
            tracing::warn!(
                ev = "srv.store.wal_unavailable",
                mode = %mode,
                "the relay database could not be put in WAL mode; running with synchronous = FULL"
            );
        }
        conn.execute_batch("PRAGMA synchronous = FULL;")
    }
}

/// What the startup integrity check found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum QuickCheck {
    /// SQLite reported `ok`.
    Ok,
    /// SQLite reported problems; the first few, verbatim.
    Problems(Vec<String>),
    /// The budget ran out before the check finished.
    Unfinished,
    /// The check could not run at all.
    Failed(String),
}

/// Run `PRAGMA quick_check`, interrupted if it outlasts `budget`.
///
/// The bound is a watchdog thread holding the connection's interrupt handle:
/// it waits on a channel for at most `budget`, and interrupts the check if
/// nothing arrived. Waiting on a channel rather than reading a clock keeps the
/// workspace's ban on ambient `Instant::now` intact. The watchdog is joined
/// before this returns, so an interrupt can only land on the check itself; one
/// that lands just after it finished is cleared by SQLite when the next
/// statement starts with none running.
pub(super) fn quick_check(conn: &Connection, budget: Duration) -> QuickCheck {
    /// Enough rows to say what is wrong without logging a page per row.
    const MAX_PROBLEMS: usize = 10;

    let interrupt = conn.get_interrupt_handle();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if finished.recv_timeout(budget) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            interrupt.interrupt();
        }
    });
    let rows = conn
        .prepare(&format!("PRAGMA quick_check({MAX_PROBLEMS})"))
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        });
    // A send can only fail once the watchdog has stopped listening, which is
    // the timeout it already acted on.
    let _ = done.send(());
    let _ = watchdog.join();
    match rows {
        Ok(rows) if rows.len() == 1 && rows[0] == "ok" => QuickCheck::Ok,
        Ok(rows) => QuickCheck::Problems(rows),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            QuickCheck::Unfinished
        }
        Err(e) => QuickCheck::Failed(e.to_string()),
    }
}

/// Report the integrity check. A failed check does not stop the relay: the
/// operator decides between restoring a backup and serving what is readable,
/// and a relay that refuses to start cannot even serve the readable part.
pub(super) fn log_quick_check(result: &QuickCheck) {
    match result {
        QuickCheck::Ok => tracing::info!(
            ev = "srv.store.quick_check",
            result = "ok",
            "relay database integrity check passed"
        ),
        QuickCheck::Problems(rows) => tracing::warn!(
            ev = "srv.store.quick_check",
            result = "problems",
            cause = %rows.join("; "),
            "relay database integrity check found problems; restore a backup or export what is readable"
        ),
        QuickCheck::Unfinished => tracing::warn!(
            ev = "srv.store.quick_check",
            result = "unfinished",
            "relay database integrity check outlasted its budget and was stopped"
        ),
        QuickCheck::Failed(cause) => tracing::warn!(
            ev = "srv.store.quick_check",
            result = "failed",
            cause = %cause,
            "relay database integrity check could not run"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// **The pragmas a file-backed relay runs under.** Every one of them is a
    /// per-connection setting except the journal mode, so a regression here is
    /// silent: the relay works, just slower or less safely.
    #[test]
    fn a_file_backed_store_runs_in_wal_with_normal_sync_and_a_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        let s = Store::open_with(Some(&path), Duration::from_millis(1234)).unwrap();
        let conn = s.conn.lock();
        let pragma = |name: &str| -> rusqlite::types::Value {
            conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .unwrap()
        };

        assert_eq!(
            pragma("journal_mode"),
            rusqlite::types::Value::Text("wal".into())
        );
        // 1 is NORMAL; FULL is 2.
        assert_eq!(pragma("synchronous"), rusqlite::types::Value::Integer(1));
        assert_eq!(
            pragma("busy_timeout"),
            rusqlite::types::Value::Integer(1234)
        );
        assert_eq!(pragma("foreign_keys"), rusqlite::types::Value::Integer(1));
    }

    /// The default timeout is non-zero: zero would turn every moment a backup
    /// tool holds the file into a failed request.
    #[test]
    fn the_default_busy_timeout_waits() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(Some(&dir.path().join("sunrise.db"))).unwrap();
        let ms: i64 = s
            .conn
            .lock()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, i64::try_from(DEFAULT_BUSY_TIMEOUT.as_millis()).unwrap());
        assert!(ms > 0);
    }

    /// A write another connection holds off past the busy timeout fails with
    /// `SQLITE_BUSY`, and that failure is what `sunrise_db_busy_total` counts.
    #[test]
    fn a_write_held_off_past_the_timeout_is_counted_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        let s = Store::open_with(Some(&path), Duration::from_millis(1)).unwrap();
        let other = rusqlite::Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let before = crate::store::busy_total();
        let refused = s.resolve_account(&crate::auth::Subject::new("i", "s"), true, 1);
        assert!(
            matches!(&refused, Err(crate::store::StoreError::Sqlite(e))
                if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)),
            "{refused:?}"
        );
        assert!(crate::store::busy_total() > before);

        other.execute_batch("ROLLBACK;").unwrap();
        assert!(s
            .resolve_account(&crate::auth::Subject::new("i", "s"), true, 1)
            .is_ok());
    }

    #[test]
    fn the_integrity_check_passes_on_a_sound_database() {
        let s = Store::open(None).unwrap();
        assert_eq!(
            quick_check(&s.conn.lock(), QUICK_CHECK_BUDGET),
            QuickCheck::Ok
        );
    }
}
