//! The integrity check every open runs
//! (`docs/04-storage/migrations.md` §Migration rigor rule 3).

use super::{Db, DbError};
use rusqlite::Connection;
use sunrise_error::ErrorCode;

impl Db {
    /// Run `PRAGMA quick_check` and turn any answer but `ok` into
    /// [`DbError::IntegrityCheckFailed`].
    ///
    /// A failure to run the pragma at all, such as a wrong key, stays a
    /// [`DbError::Sqlite`]: that is a file this key cannot read, not a damaged
    /// one, and the caller must not be told to rebuild it.
    ///
    /// `encrypted_file` says the connection is a keyed file, which is what
    /// SQLCipher's per-page HMAC check needs: an in-memory database answers
    /// it with "database file is undefined".
    pub(crate) fn quick_check(conn: &Connection, encrypted_file: bool) -> Result<(), DbError> {
        use rusqlite::ErrorCode::{DatabaseCorrupt, NotADatabase};
        let rows = |pragma: &str| -> rusqlite::Result<Vec<String>> {
            let mut stmt = conn.prepare(pragma)?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        let lines = match rows("PRAGMA quick_check") {
            Ok(lines) => lines,
            // A page SQLite cannot parse is reported as an error rather than
            // as a row. It is the same finding.
            Err(rusqlite::Error::SqliteFailure(e, msg)) if e.code == DatabaseCorrupt => {
                vec![msg.unwrap_or_else(|| e.to_string())]
            }
            // Under SQLCipher a page whose HMAC no longer verifies surfaces as
            // a bare `SQLITE_ERROR`, indistinguishable from any other. Only
            // then is the page check run, which reads every page and reports
            // each bad one as a row: running it on every open would read the
            // whole file a second time. `SQLITE_NOTADB` is excluded because a
            // wrong key produces it too, and that is not damage.
            Err(rusqlite::Error::SqliteFailure(e, msg))
                if encrypted_file && e.code != NotADatabase =>
            {
                let damaged_pages = rows("PRAGMA cipher_integrity_check")?;
                if damaged_pages.is_empty() {
                    return Err(rusqlite::Error::SqliteFailure(e, msg).into());
                }
                damaged_pages
            }
            Err(e) => return Err(e.into()),
        };
        if lines.len() == 1 && lines[0] == "ok" {
            return Ok(());
        }
        Err(Self::integrity_failed(&lines))
    }

    /// Log a failed check and build its error from the first few findings.
    pub(crate) fn integrity_failed(lines: &[String]) -> DbError {
        /// `quick_check` reports up to 100 problems by default. The first few
        /// say what is wrong; the rest only lengthen the error.
        const REPORTED: usize = 5;
        tracing::error!(
            ev = "db.integrity.failed",
            err_code = %ErrorCode::StorageIntegrityCheckFailed,
            err_kind = "user",
            retryable = false,
            "the vault failed its integrity check"
        );
        DbError::IntegrityCheckFailed {
            detail: lines
                .iter()
                .take(REPORTED)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use sunrise_crypto::keys::VaultRootKey;

    fn vault_key() -> VaultRootKey {
        VaultRootKey::from_bytes([0xab; 32])
    }

    #[test]
    fn a_sound_vault_passes_its_integrity_check() {
        let db = Db::open_memory(&vault_key()).unwrap();
        Db::quick_check(db.conn(), false).unwrap();
    }

    /// A row that breaks its table's declared constraint is a finding
    /// `quick_check` reports as a row, and it must come back typed rather than
    /// as a bare SQLite error.
    #[test]
    fn a_row_breaking_its_schema_fails_the_integrity_check_as_its_own_error() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a INTEGER);
             INSERT INTO t VALUES (1), (NULL);
             PRAGMA writable_schema = ON;
             UPDATE sqlite_schema SET sql = 'CREATE TABLE t (a INTEGER NOT NULL)'
               WHERE name = 't';
             PRAGMA writable_schema = RESET;",
        )
        .unwrap();
        let err = Db::quick_check(&conn, false).unwrap_err();
        assert!(
            matches!(err, DbError::IntegrityCheckFailed { .. }),
            "expected an integrity failure, got {err:?}"
        );
    }

    /// A page whose bytes were changed on disk fails the check on open, and
    /// the file is not migrated or otherwise written.
    #[test]
    fn a_vault_with_a_damaged_page_is_refused_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        {
            let db = Db::open(&path, &vault_key()).unwrap();
            // Enough rows that the file's last page is theirs, past the
            // pages migration 0044's table rebuilds free.
            for i in 0..1000u32 {
                db.conn()
                    .execute(
                        "INSERT INTO streams
                         (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
                         VALUES (?, X'00', 0, 'a stream name long enough to fill pages', 0, 0)",
                        rusqlite::params![i.to_be_bytes().to_vec()],
                    )
                    .unwrap();
            }
            db.conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        // Well past page 1, whose damage would read as a wrong key.
        let at = bytes.len() - 4096 + 100;
        for b in &mut bytes[at..at + 64] {
            *b ^= 0xff;
        }
        fs::write(&path, &bytes).unwrap();

        let err = Db::open(&path, &vault_key()).unwrap_err();
        assert!(
            matches!(err, DbError::IntegrityCheckFailed { .. }),
            "expected an integrity failure, got {err:?}"
        );
    }

    /// `quick_check` failing with an error other than `SQLITE_NOTADB` on a
    /// keyed file runs the page check, and when every page verifies the
    /// original error comes back as it was: the file is not damaged, so it
    /// must not be reported as damaged.
    ///
    /// The error here is a `CHECK` constraint naming a function this build
    /// does not have, which `quick_check` evaluates and cannot resolve.
    #[test]
    fn a_check_failure_on_sound_pages_stays_the_sqlite_error_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        drop(Db::open(&path, &vault_key()).unwrap());
        let conn = Connection::open(&path).unwrap();
        Db::apply_sqlcipher_key(&conn, &vault_key()).unwrap();
        Db::apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a INTEGER);
             INSERT INTO t VALUES (1);
             PRAGMA writable_schema = ON;
             UPDATE sqlite_schema
               SET sql = 'CREATE TABLE t (a INTEGER CHECK (no_such_function(a)))'
               WHERE name = 't';
             PRAGMA writable_schema = RESET;",
        )
        .unwrap();

        let err = Db::quick_check(&conn, true).unwrap_err();
        let DbError::Sqlite(rusqlite::Error::SqliteFailure(e, msg)) = &err else {
            panic!("expected the original SQLite error, got {err:?}");
        };
        assert_ne!(e.code, rusqlite::ErrorCode::NotADatabase);
        assert_ne!(e.code, rusqlite::ErrorCode::DatabaseCorrupt);
        assert!(
            msg.as_deref()
                .is_some_and(|m| m.contains("no_such_function")),
            "expected the unresolved function to be named, got {err:?}"
        );
    }
}
