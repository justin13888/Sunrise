//! The pragmas every connection runs under, and returning freed pages to the
//! filesystem.

use super::{Db, DbError};
use rusqlite::Connection;

impl Db {
    pub(crate) fn apply_pragmas(conn: &Connection) -> Result<(), DbError> {
        // SQLCipher requires the key BEFORE any other pragma; the caller
        // ensures that by calling apply_sqlcipher_key first.
        //
        // `auto_vacuum` goes first. It takes effect only while the file has
        // no header page yet, and `journal_mode = WAL` is persistent: setting
        // it writes the header, after which an `auto_vacuum` change is
        // silently ignored until a `VACUUM` (issue #461). An existing file is
        // brought to it by `ensure_incremental_auto_vacuum`.
        conn.execute_batch(
            "PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(())
    }

    /// `PRAGMA auto_vacuum` as the file reports it: 0 NONE, 1 FULL,
    /// 2 INCREMENTAL.
    fn auto_vacuum_mode(conn: &Connection) -> Result<i64, DbError> {
        Ok(conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?)
    }

    /// Move a vault created before `auto_vacuum` was set in the right order
    /// to `INCREMENTAL`, once, with a full `VACUUM`.
    ///
    /// Such a file reports `auto_vacuum = 0`, and only a `VACUUM` can change
    /// that. It runs after `ensure_schema`, so a pre-migration backup is a
    /// copy of the file as it was. Once converted the mode is stored in the
    /// file's header, so every later open finds it set and does nothing.
    ///
    /// Best-effort: the rewrite needs free disk about the size of the vault,
    /// and a `VACUUM` that fails changes nothing, so the open goes on with the
    /// vault as it was and the next open tries again. A vault that cannot
    /// shrink is still a vault; one that will not open is not.
    pub(crate) fn ensure_incremental_auto_vacuum(conn: &Connection) -> Result<(), DbError> {
        if Self::auto_vacuum_mode(conn)? == 2 {
            return Ok(());
        }
        tracing::info!(
            ev = "db.auto_vacuum.convert",
            "rewriting the vault so freed pages can be returned to the filesystem"
        );
        if let Err(source) = conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;") {
            tracing::warn!(
                ev = "db.auto_vacuum.convert_failed",
                cause = %source,
                "could not rewrite the vault; it opens as it was"
            );
        }
        Ok(())
    }

    /// Return every page on the free list to the filesystem, and report how
    /// many that was.
    ///
    /// Meant for after a large delete, such as an op-log compaction run: the
    /// pages it freed otherwise stay in the file for later writes to reuse.
    /// Cheap when the free list is empty.
    ///
    /// # Errors
    /// Storage failures.
    pub fn incremental_vacuum(&self) -> Result<u64, DbError> {
        let free = |conn: &Connection| -> rusqlite::Result<i64> {
            conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))
        };
        let before = free(&self.conn)?;
        if before == 0 {
            return Ok(0);
        }
        // The pragma frees one page per step, so it is stepped to the end:
        // `execute_batch` steps it once and frees a single page.
        let mut stmt = self.conn.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        drop(rows);
        drop(stmt);
        let after = free(&self.conn)?;
        Ok(u64::try_from(before.saturating_sub(after)).unwrap_or(0))
    }
}
