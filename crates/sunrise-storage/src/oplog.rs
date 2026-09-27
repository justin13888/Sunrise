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

    /// Mark an op already inserted into `ops` as parked (migration 0031,
    /// ADR-0045 §4).
    ///
    /// The caller inserts the `ops` row first, unapplied and under
    /// [`PARKED_KIND`], in the same transaction: that row is what counts the op
    /// toward the sync cursor. `INSERT OR IGNORE`, so a re-delivery of an op
    /// that is already parked keeps the row it has.
    pub fn park(tx: &rusqlite::Transaction<'_>, parked: &Parking<'_>) -> Result<(), OpLogError> {
        tx.execute(
            "INSERT OR IGNORE INTO parked_ops
             (op_id, reason, kind, hlc_logical, parked_under_doc_schema_v, parked_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                parked.op_id,
                parked.reason.as_str(),
                parked.kind,
                parked.hlc_logical,
                parked.doc_schema_v,
                parked.parked_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Take `op_id` out of `parked_ops` and record it as applied, returning
    /// whether it was parked at all.
    ///
    /// Only an op delivered with the **same envelope bytes** it was parked
    /// with is released. The op id is derived from `(stream, device, seq)`, so
    /// a sender that signed two different payloads under one seq produces two
    /// deliveries with one id; the op log keeps whichever arrived first, and
    /// this keeps it that way rather than applying the second one's payload
    /// against the first one's row.
    ///
    /// On `true` the `ops` row's kind columns and `applied_at` are filled in,
    /// and the caller goes on to materialize the op in the same transaction.
    pub fn unpark(
        tx: &rusqlite::Transaction<'_>,
        op_id: &[u8; 16],
        envelope: &[u8],
        inner_kind: &str,
        target_kind: &str,
        target_id: Option<&[u8; 16]>,
        applied_at_ms: u64,
    ) -> Result<bool, OpLogError> {
        let released = tx.execute(
            "DELETE FROM parked_ops WHERE op_id = ?1
               AND EXISTS (SELECT 1 FROM ops WHERE op_id = ?1 AND envelope = ?2)",
            params![op_id, envelope],
        )?;
        if released == 0 {
            return Ok(false);
        }
        tx.execute(
            "UPDATE ops SET inner_kind = ?, target_kind = ?, target_id = ?, applied_at = ?
             WHERE op_id = ?",
            params![
                inner_kind,
                target_kind,
                target_id.map(|b| &b[..]),
                applied_at_ms,
                op_id
            ],
        )?;
        Ok(true)
    }

    /// Every parked op a build at `doc_schema_v` has not yet tried, in
    /// `(hlc, device_id, seq)` order.
    ///
    /// That order is the one the ops were stamped in, so a replayed batch
    /// meets the entity rows in the order a replica that could read them all
    /// along would have. Entity ops would converge in any order, because LWW
    /// compares stamps rather than arrival; the order is what keeps a parked
    /// create ahead of the parked update that follows it (ADR-0045 §4).
    pub fn parked_for_replay(
        db: &Db,
        doc_schema_v: u16,
    ) -> Result<Vec<ParkedEnvelope>, OpLogError> {
        let mut stmt = db.conn().prepare(
            "SELECT o.op_id, o.envelope FROM parked_ops p JOIN ops o ON o.op_id = p.op_id
             WHERE p.parked_under_doc_schema_v <> ?
             ORDER BY o.ts_ms, p.hlc_logical, o.device_id, o.seq",
        )?;
        let rows = stmt.query_map(params![doc_schema_v], |row| {
            Ok(ParkedEnvelope {
                op_id: row.get(0)?,
                envelope: row.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Record that a build at `doc_schema_v` tried to replay `op_id` and it
    /// is still parked, so [`Self::parked_for_replay`] skips it until the
    /// build changes. A no-op for an op the replay released.
    pub fn restamp_parked(
        tx: &rusqlite::Transaction<'_>,
        op_id: &[u8; 16],
        doc_schema_v: u16,
    ) -> Result<(), OpLogError> {
        tx.execute(
            "UPDATE parked_ops SET parked_under_doc_schema_v = ? WHERE op_id = ?",
            params![doc_schema_v, op_id],
        )?;
        Ok(())
    }
}

/// The `ops.inner_kind` and `ops.target_kind` a parked op is stored under
/// until a replay releases it. Nothing that reads `ops` by kind matches it.
pub const PARKED_KIND: &str = "unknown";

/// Why an op is parked rather than applied (ADR-0045 §4).
///
/// One reason today. The others that ADR lists (an unknown field-op kind, a
/// payload undecodable at a newer `doc_schema_v`, a schema-fingerprint
/// mismatch) arrive with the issues that introduce what they detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkReason {
    /// The inner op's variant name is not one this build knows.
    UnknownKind,
}

impl ParkReason {
    /// The `parked_ops.reason` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownKind => "unknown_kind",
        }
    }
}

/// A parked op as [`OpLog::parked_for_replay`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedEnvelope {
    /// The op's id in `ops` and `parked_ops`.
    pub op_id: [u8; 16],
    /// The envelope bytes it was parked with.
    pub envelope: Vec<u8>,
}

/// One `parked_ops` row, as [`OpLog::park`] writes it.
#[derive(Debug, Clone, Copy)]
pub struct Parking<'a> {
    /// The op's id, already present in `ops`.
    pub op_id: &'a [u8; 16],
    /// Why it is parked.
    pub reason: ParkReason,
    /// The variant name this build did not know. Diagnostics only.
    pub kind: &'a str,
    /// The envelope's `hlc.logical`; `ops.ts_ms` holds its `physical_ms`.
    pub hlc_logical: u32,
    /// The `DOC_SCHEMA_V` of the build that parked it.
    pub doc_schema_v: u16,
    /// When it was parked.
    pub parked_at_ms: u64,
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

    fn sq(e: OpLogError) -> rusqlite::Error {
        match e {
            OpLogError::Sqlite(s) => s,
            OpLogError::Db(_) => rusqlite::Error::ExecuteReturnedResults,
        }
    }

    /// Park one op the way the engine does: an unapplied `ops` row under
    /// [`PARKED_KIND`], then its `parked_ops` marker, in one transaction.
    fn park(db: &mut Db, id: u8, device: u8, seq: u64, ts_ms: u64, logical: u32, under: u16) {
        db.with_tx(|tx| {
            let op_id = [id; 16];
            OpLog::insert(
                tx,
                &op_id,
                &[9u8; 16],
                &[device; 16],
                seq,
                ts_ms,
                &[id; 8],
                PARKED_KIND,
                PARKED_KIND,
                None,
                None,
                Some(&[device; 16]),
                1,
                &[],
            )
            .map_err(sq)?;
            OpLog::park(
                tx,
                &Parking {
                    op_id: &op_id,
                    reason: ParkReason::UnknownKind,
                    kind: "FutureKind",
                    hlc_logical: logical,
                    doc_schema_v: under,
                    parked_at_ms: 1,
                },
            )
            .map_err(sq)
        })
        .unwrap();
    }

    fn ids(rows: &[ParkedEnvelope]) -> Vec<u8> {
        rows.iter().map(|p| p.op_id[0]).collect()
    }

    #[test]
    fn parked_ops_replay_in_hlc_device_seq_order_and_only_for_another_build() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        // Inserted out of order on purpose; each pair differs in exactly one
        // component of the key, so a missing ORDER BY term reorders them.
        park(&mut db, 1, 2, 1, 200, 0, 5); // later physical
        park(&mut db, 2, 2, 2, 100, 1, 5); // same physical as 3, later logical
        park(&mut db, 3, 2, 3, 100, 0, 5);
        park(&mut db, 4, 1, 9, 200, 0, 5); // same hlc as 1, lower device
        park(&mut db, 5, 1, 8, 200, 0, 5); // same hlc and device as 4, lower seq
        park(&mut db, 6, 1, 1, 50, 0, 6); // already tried by the build at 6

        let at_6 = OpLog::parked_for_replay(&db, 6).unwrap();
        assert_eq!(ids(&at_6), vec![3, 2, 5, 4, 1]);
        assert_eq!(
            at_6[0].envelope,
            vec![3u8; 8],
            "the envelope is the stored one"
        );
        assert_eq!(ids(&OpLog::parked_for_replay(&db, 5).unwrap()), vec![6]);

        db.with_tx(|tx| OpLog::restamp_parked(tx, &[3u8; 16], 6).map_err(sq))
            .unwrap();
        assert_eq!(
            ids(&OpLog::parked_for_replay(&db, 6).unwrap()),
            vec![2, 5, 4, 1]
        );
    }

    #[test]
    fn unpark_releases_only_the_envelope_it_parked() {
        let mut db = Db::open_memory(&vault_key()).unwrap();
        park(&mut db, 7, 2, 1, 100, 0, 5);
        let op_id = [7u8; 16];

        let other = db
            .with_tx(|tx| {
                OpLog::unpark(tx, &op_id, b"other bytes", "task.create", "task", None, 9)
                    .map_err(sq)
            })
            .unwrap();
        assert!(!other, "a second payload under the same id is not released");

        let released = db
            .with_tx(|tx| {
                OpLog::unpark(
                    tx,
                    &op_id,
                    &[7u8; 8],
                    "task.create",
                    "task",
                    Some(&[4u8; 16]),
                    9,
                )
                .map_err(sq)
            })
            .unwrap();
        assert!(released);
        let (kind, target, applied): (String, Vec<u8>, i64) = db
            .conn()
            .query_row(
                "SELECT inner_kind, target_id, applied_at FROM ops WHERE op_id = ?",
                params![op_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), target, applied),
            ("task.create", vec![4u8; 16], 9)
        );
        assert!(OpLog::parked_for_replay(&db, 6).unwrap().is_empty());

        let again = db
            .with_tx(|tx| {
                OpLog::unpark(tx, &op_id, &[7u8; 8], "task.create", "task", None, 10).map_err(sq)
            })
            .unwrap();
        assert!(!again, "an op already released is not released twice");
    }
}
