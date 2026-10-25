//! The one snapshot record the relay keeps per stream (ADR-0068).
//!
//! A stream's compactor seals a snapshot of its state (ADR-0059 §4), uploads
//! it through the ordinary blob routes, and then **pins** it here as that
//! stream's latest. A device whose cursor the relay's op retention has passed,
//! or a new device joining an old account, reads the pin and fetches the blob.
//!
//! What this table holds is a pointer, not the record: the bytes live in the
//! [`BlobBackend`](crate::blob::BlobBackend), which has no frame cap and
//! already isolates accounts. A pin is never trimmed by the op log's retention,
//! and a newer pin replaces the older one, whose blob the handler then
//! deletes, so the relay holds one record per stream.

use rusqlite::{params, OptionalExtension};

use super::{unsigned, Store, StoreError};

/// Migration 0006, and therefore frozen: change this table by a new
/// migration, never an edit here.
///
/// Keyed by the account id so it cascades from the account row with
/// everything else the account owns; `stream_id` is the 16 bytes the relay
/// already files the stream's channel under.
pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS stream_snapshots (
    account_id      TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    stream_id       BLOB NOT NULL,
    blob_key        BLOB NOT NULL,
    size_bytes      INTEGER NOT NULL,
    published_by    TEXT,
    published_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, stream_id)
);";

/// One stream's pinned snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedSnapshot {
    /// The stream it is a snapshot of.
    pub stream_id: [u8; 16],
    /// The committed blob holding the record, by its content key.
    pub blob_key: [u8; 16],
    /// The record's size in bytes, as the blob's manifest states it.
    pub size_bytes: u64,
    /// The relay device row that pinned it. `None` where no device signed.
    pub published_by: Option<String>,
    /// When it was pinned, ms since the epoch.
    pub published_at_ms: u64,
}

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn key16(v: &[u8]) -> Option<[u8; 16]> {
    v.try_into().ok()
}

impl Store {
    /// Make `pin` its stream's latest snapshot, replacing whatever was pinned
    /// before, and return the blob the replaced pin named when nothing else
    /// of this account's still names it: the caller deletes that blob.
    ///
    /// The relay cannot order two records it cannot read, so the latest is
    /// the last one pinned. Only the stream's compactor publishes, so two
    /// writers race only while the election changes hands.
    pub fn pin_snapshot(
        &self,
        account_id: &str,
        pin: &PinnedSnapshot,
    ) -> Result<Option<[u8; 16]>, StoreError> {
        let mut conn = self.tx("store.pin_snapshot");
        let tx = conn.transaction()?;
        let before: Option<Vec<u8>> = tx
            .query_row(
                "SELECT blob_key FROM stream_snapshots WHERE account_id = ?1 AND stream_id = ?2",
                params![account_id, &pin.stream_id[..]],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO stream_snapshots
                 (account_id, stream_id, blob_key, size_bytes, published_by, published_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id, stream_id) DO UPDATE SET
                 blob_key = excluded.blob_key, size_bytes = excluded.size_bytes,
                 published_by = excluded.published_by,
                 published_at_ms = excluded.published_at_ms",
            params![
                account_id,
                &pin.stream_id[..],
                &pin.blob_key[..],
                signed(pin.size_bytes),
                pin.published_by,
                signed(pin.published_at_ms),
            ],
        )?;
        let released = match before.as_deref().and_then(key16) {
            Some(old) if old != pin.blob_key => {
                let still: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM stream_snapshots WHERE account_id = ?1 AND blob_key = ?2",
                    params![account_id, &old[..]],
                    |r| r.get(0),
                )?;
                (still == 0).then_some(old)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(released)
    }

    /// The snapshot pinned for `stream_id`, if there is one.
    pub fn pinned_snapshot(
        &self,
        account_id: &str,
        stream_id: &[u8; 16],
    ) -> Result<Option<PinnedSnapshot>, StoreError> {
        let row = self
            .tx("store.pinned_snapshot")
            .query_row(
                "SELECT blob_key, size_bytes, published_by, published_at_ms
                 FROM stream_snapshots WHERE account_id = ?1 AND stream_id = ?2",
                params![account_id, &stream_id[..]],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(blob, size, by, at)| {
            Some(PinnedSnapshot {
                stream_id: *stream_id,
                blob_key: key16(&blob)?,
                size_bytes: unsigned(size),
                published_by: by,
                published_at_ms: unsigned(at),
            })
        }))
    }
}
