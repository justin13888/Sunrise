//! Op log access on top of [`crate::Db`].
//!
//! Per `docs/04-storage/op-log.md`. The op log is an append-only table of
//! envelope blobs keyed by `op_id`; insertion happens in the same
//! transaction as any materialized-state mutation derived from the op.

use crate::db::{Db, DbError};
use rusqlite::{params, OptionalExtension};
use thiserror::Error;

/// Op-log errors.
#[derive(Debug, Error)]
pub enum OpLogError {
    /// Underlying DB error.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Underlying SQLite error.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Append-only op log accessor.
#[derive(Debug)]
pub struct OpLog;

impl OpLog {
    /// Insert one envelope plus its dep edges. Idempotent on `op_id` (UNIQUE
    /// constraint); a duplicate insertion is silently ignored to support
    /// at-least-once delivery from sync.
    ///
    /// `applied_at` should be `None` if the op cannot be applied yet (deps
    /// unsatisfied); the materializer will set it later.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        tx: &rusqlite::Transaction<'_>,
        op_id: &[u8; 16],
        stream_id: &[u8; 16],
        device_id: &[u8; 16],
        seq: u64,
        ts_ms: u64,
        envelope: &[u8],
        inner_kind: &str,
        target_kind: &str,
        target_id: Option<&[u8; 16]>,
        applied_at_ms: Option<u64>,
        received_from: Option<&[u8; 16]>,
        received_at_ms: u64,
        deps: &[[u8; 16]],
    ) -> Result<(), OpLogError> {
        // INSERT OR IGNORE so duplicate op_id (idempotent re-receive) doesn't
        // error.
        tx.execute(
            "INSERT OR IGNORE INTO ops
             (op_id, stream_id, device_id, seq, ts_ms, envelope,
              inner_kind, target_kind, target_id, applied_at,
              received_from, received_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                op_id,
                stream_id,
                device_id,
                seq,
                ts_ms,
                envelope,
                inner_kind,
                target_kind,
                target_id.map(|b| &b[..]),
                applied_at_ms,
                received_from.map(|b| &b[..]),
                received_at_ms,
            ],
        )?;
        for dep in deps {
            tx.execute(
                "INSERT OR IGNORE INTO op_dep (op_id, dep_id) VALUES (?, ?)",
                params![op_id, dep],
            )?;
        }
        Ok(())
    }

    /// Fetch the raw envelope bytes for a given op_id.
    pub fn get_envelope(db: &Db, op_id: &[u8; 16]) -> Result<Option<Vec<u8>>, OpLogError> {
        let row = db
            .conn()
            .query_row(
                "SELECT envelope FROM ops WHERE op_id = ?",
                params![op_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// All ops for a Stream in ascending `seq` order.
    pub fn list_for_stream(
        db: &Db,
        stream_id: &[u8; 16],
    ) -> Result<Vec<(Vec<u8>, u64)>, OpLogError> {
        let mut stmt = db
            .conn()
            .prepare("SELECT envelope, seq FROM ops WHERE stream_id = ? ORDER BY seq")?;
        let rows = stmt.query_map(params![stream_id], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u64>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mark `op_id` as applied at `ts_ms`.
    pub fn mark_applied(
        tx: &rusqlite::Transaction<'_>,
        op_id: &[u8; 16],
        ts_ms: u64,
    ) -> Result<(), OpLogError> {
        tx.execute(
            "UPDATE ops SET applied_at = ? WHERE op_id = ?",
            params![ts_ms, op_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::keys::VaultRootKey;

    fn vault_key() -> VaultRootKey {
        VaultRootKey::from_bytes([0xab; 32])
    }

    #[test]
    fn insert_and_fetch() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        let op_id = [1u8; 16];
        let stream_id = [2u8; 16];
        let device_id = [3u8; 16];
        let envelope = b"raw envelope bytes".to_vec();
        db.with_tx(|tx| {
            OpLog::insert(
                tx,
                &op_id,
                &stream_id,
                &device_id,
                1,
                1_700_000_000_000,
                &envelope,
                "task.create",
                "task",
                Some(&[4u8; 16]),
                None,
                None,
                1_700_000_000_001,
                &[],
            )
            .map_err(|e| match e {
                OpLogError::Sqlite(s) => s,
                OpLogError::Db(_) => rusqlite::Error::ExecuteReturnedResults,
            })?;
            Ok(())
        })
        .unwrap();
        let env = OpLog::get_envelope(&db, &op_id).unwrap().unwrap();
        assert_eq!(env, envelope);
    }

    #[test]
    fn duplicate_insert_is_idempotent() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        let op_id = [1u8; 16];
        let envelope = b"x".to_vec();
        let insert = |db: &mut Db| {
            db.with_tx(|tx| {
                OpLog::insert(
                    tx,
                    &op_id,
                    &[2u8; 16],
                    &[3u8; 16],
                    1,
                    1,
                    &envelope,
                    "task.create",
                    "task",
                    None,
                    None,
                    None,
                    1,
                    &[],
                )
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                Ok(())
            })
        };
        insert(&mut db).unwrap();
        insert(&mut db).unwrap();
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM ops WHERE op_id = ?",
                params![op_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn list_for_stream_ordered() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        let stream = [9u8; 16];
        let device = [8u8; 16];
        for s in 1..=3u8 {
            let op_id = {
                let mut a = [0u8; 16];
                a[15] = s;
                a
            };
            db.with_tx(|tx| {
                OpLog::insert(
                    tx,
                    &op_id,
                    &stream,
                    &device,
                    u64::from(s),
                    1,
                    &[s; 4],
                    "x",
                    "task",
                    None,
                    None,
                    None,
                    1,
                    &[],
                )
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                Ok(())
            })
            .unwrap();
        }
        let list = OpLog::list_for_stream(&db, &stream).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].1, 1);
        assert_eq!(list[2].1, 3);
    }
}
