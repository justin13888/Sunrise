//! Local sync bookkeeping: outbox + per-(stream, device) cursors.
//!
//! Per `docs/04-storage/` and the op-envelope sealing slice. The outbox tracks
//! which locally-emitted ops still need to be pushed to peers; cursors track
//! the highest applied `seq` per `(stream_id, device_id)` for gap detection and
//! pull resumption. Both follow [`crate::OpLog`]'s static-accessor style.

use crate::db::{Db, DbError};
use rusqlite::{params, OptionalExtension};
use thiserror::Error;

/// Errors produced by the outbox / cursor accessors.
#[derive(Debug, Error)]
pub enum SyncLocalError {
    /// Underlying DB error.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Underlying SQLite error.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// A pending (or acked) outbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// The op awaiting push.
    pub op_id: [u8; 16],
    /// The op's routing stream id (`0x00..00` = vault-meta).
    pub stream_id: [u8; 16],
    /// When the op was enqueued (ms since epoch).
    pub enqueued_at_ms: u64,
}

/// Append-only outbox accessor.
#[derive(Debug)]
pub struct Outbox;

impl Outbox {
    /// Enqueue an op for push. Idempotent on `op_id` (PRIMARY KEY); a duplicate
    /// enqueue is silently ignored.
    ///
    /// Runs inside the caller's transaction so the outbox row commits together
    /// with the op it references.
    pub fn enqueue(
        tx: &rusqlite::Transaction<'_>,
        op_id: &[u8; 16],
        stream_id: &[u8; 16],
        enqueued_at_ms: u64,
    ) -> Result<(), SyncLocalError> {
        tx.execute(
            "INSERT OR IGNORE INTO outbox (op_id, stream_id, enqueued_at_ms, acked_at_ms)
             VALUES (?, ?, ?, NULL)",
            params![&op_id[..], &stream_id[..], enqueued_at_ms],
        )?;
        Ok(())
    }

    /// List all unacked entries in enqueue order (oldest first).
    pub fn list_unacked(db: &Db) -> Result<Vec<OutboxEntry>, SyncLocalError> {
        let mut stmt = db.conn().prepare(
            "SELECT op_id, stream_id, enqueued_at_ms FROM outbox
             WHERE acked_at_ms IS NULL
             ORDER BY enqueued_at_ms ASC, op_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OutboxEntry {
                op_id: blob16(&row.get::<_, Vec<u8>>(0)?),
                stream_id: blob16(&row.get::<_, Vec<u8>>(1)?),
                enqueued_at_ms: row.get::<_, u64>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mark an op acked at `acked_at_ms`. No-op if the op is absent.
    pub fn mark_acked(
        tx: &rusqlite::Transaction<'_>,
        op_id: &[u8; 16],
        acked_at_ms: u64,
    ) -> Result<(), SyncLocalError> {
        tx.execute(
            "UPDATE outbox SET acked_at_ms = ? WHERE op_id = ?",
            params![acked_at_ms, &op_id[..]],
        )?;
        Ok(())
    }

    /// Count unacked entries (cheap; used for sync-status surfaces).
    pub fn pending_count(db: &Db) -> Result<u64, SyncLocalError> {
        let n: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM outbox WHERE acked_at_ms IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

/// Per-(stream, device) applied-seq cursor accessor.
#[derive(Debug)]
pub struct SyncCursors;

impl SyncCursors {
    /// Read the highest applied seq for `(stream_id, device_id)`, if any.
    pub fn get(
        db: &Db,
        stream_id: &[u8; 16],
        device_id: &[u8; 16],
    ) -> Result<Option<u64>, SyncLocalError> {
        let v: Option<i64> = db
            .conn()
            .query_row(
                "SELECT last_applied_seq FROM sync_cursors
                 WHERE stream_id = ? AND device_id = ?",
                params![&stream_id[..], &device_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.map(|n| u64::try_from(n).unwrap_or(0)))
    }

    /// Set (upsert) the highest applied seq for `(stream_id, device_id)`.
    pub fn set(
        tx: &rusqlite::Transaction<'_>,
        stream_id: &[u8; 16],
        device_id: &[u8; 16],
        last_applied_seq: u64,
    ) -> Result<(), SyncLocalError> {
        tx.execute(
            "INSERT INTO sync_cursors (stream_id, device_id, last_applied_seq)
             VALUES (?, ?, ?)
             ON CONFLICT (stream_id, device_id)
             DO UPDATE SET last_applied_seq = excluded.last_applied_seq",
            params![&stream_id[..], &device_id[..], last_applied_seq],
        )?;
        Ok(())
    }
}

fn blob16(b: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    let take = b.len().min(16);
    a[..take].copy_from_slice(&b[..take]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::OpLog;
    use sunrise_crypto::keys::VaultRootKey;

    fn vault_key() -> VaultRootKey {
        VaultRootKey::from_bytes([0xab; 32])
    }

    fn insert_op(db: &mut Db, op_id: [u8; 16], stream_id: [u8; 16], enq_ms: u64) {
        db.with_tx(|tx| {
            OpLog::insert(
                tx,
                &op_id,
                &stream_id,
                &[7u8; 16],
                1,
                enq_ms,
                b"env",
                "task.create",
                "task",
                None,
                None,
                None,
                enq_ms,
                &[],
            )
            .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Outbox::enqueue(tx, &op_id, &stream_id, enq_ms)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn outbox_enqueue_list_ack() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        insert_op(&mut db, [1u8; 16], [0u8; 16], 100);
        insert_op(&mut db, [2u8; 16], [9u8; 16], 200);

        let pending = Outbox::list_unacked(&db).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].op_id, [1u8; 16]);
        assert_eq!(pending[1].stream_id, [9u8; 16]);
        assert_eq!(Outbox::pending_count(&db).unwrap(), 2);

        db.with_tx(|tx| {
            Outbox::mark_acked(tx, &[1u8; 16], 300)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Ok(())
        })
        .unwrap();

        let pending = Outbox::list_unacked(&db).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, [2u8; 16]);
        assert_eq!(Outbox::pending_count(&db).unwrap(), 1);
    }

    #[test]
    fn outbox_enqueue_is_idempotent() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        insert_op(&mut db, [1u8; 16], [0u8; 16], 100);
        // Re-enqueue the same op id: must not duplicate or error.
        db.with_tx(|tx| {
            Outbox::enqueue(tx, &[1u8; 16], &[0u8; 16], 999)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(Outbox::pending_count(&db).unwrap(), 1);
    }

    #[test]
    fn cursors_get_set() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        let stream = [3u8; 16];
        let device = [4u8; 16];
        assert_eq!(SyncCursors::get(&db, &stream, &device).unwrap(), None);
        db.with_tx(|tx| {
            SyncCursors::set(tx, &stream, &device, 5)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(SyncCursors::get(&db, &stream, &device).unwrap(), Some(5));
        // Upsert overwrites.
        db.with_tx(|tx| {
            SyncCursors::set(tx, &stream, &device, 9)
                .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(SyncCursors::get(&db, &stream, &device).unwrap(), Some(9));
    }
}
