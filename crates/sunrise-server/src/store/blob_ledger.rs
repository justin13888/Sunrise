//! The committed-blob ledger: one row per readable blob and the ciphertext
//! bytes it holds, so `sunrise_blob_storage_bytes` is one `SUM` at scrape
//! time rather than a walk of the blob tree.
//!
//! Ciphertext at rest lives in the [`BlobBackend`](crate::blob::BlobBackend),
//! and before this table nothing in the database said how much of it there
//! was. A gauge kept by paired increments would drift
//! (`docs/06-server/metrics.md` §Rules), so the ledger is a fact per blob,
//! written beside the step that changes it:
//!
//! - `finalize` records a blob once its manifest is written;
//! - the maintenance pass forgets one once its collection has removed it;
//! - erasing an account cascades from its row.
//!
//! A finalize interrupted between its manifest and its row leaves a readable
//! blob the ledger does not know, and blobs committed before migration 0005
//! have no row either. The maintenance pass backfills both from the manifests
//! on disk: [`Store::backfill_blobs`] inserts only what is missing.

use rusqlite::params;

use super::{Store, StoreError};

/// Migration 0005, and therefore frozen: change this table by a new
/// migration, never an edit here.
///
/// Keyed by the account id rather than its hash so that it cascades from the
/// account row with everything else the account owns.
pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS committed_blobs (
    account_id TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    blob_key   BLOB NOT NULL,
    size_bytes INTEGER NOT NULL,
    PRIMARY KEY (account_id, blob_key)
);";

/// One readable blob, as the ledger records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedBlob {
    /// The account that owns it.
    pub account_id: String,
    /// Its 16-byte content key.
    pub blob_key: [u8; 16],
    /// Its ciphertext bytes, as its manifest states them.
    pub size_bytes: u64,
}

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    /// Record a readable blob. The same content committed again states the
    /// same size, so a second record is the first one again.
    pub fn record_blob(&self, blob: &CommittedBlob) -> Result<(), StoreError> {
        self.tx("store.record_blob").execute(
            "INSERT INTO committed_blobs (account_id, blob_key, size_bytes)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (account_id, blob_key) DO UPDATE SET size_bytes = excluded.size_bytes",
            params![blob.account_id, &blob.blob_key[..], signed(blob.size_bytes)],
        )?;
        Ok(())
    }

    /// Forget a blob that is no longer readable. Absent is not an error.
    pub fn forget_blob(&self, account_id: &str, blob_key: &[u8; 16]) -> Result<(), StoreError> {
        self.tx("store.forget_blob").execute(
            "DELETE FROM committed_blobs WHERE account_id = ?1 AND blob_key = ?2",
            params![account_id, &blob_key[..]],
        )?;
        Ok(())
    }

    /// Record each of `blobs` the ledger does not already hold, in one
    /// transaction, returning how many were new.
    ///
    /// A row already present is left alone, and a blob whose account is gone
    /// is skipped rather than failing the rest: the account may have been
    /// erased after its manifests were listed.
    pub fn backfill_blobs(&self, blobs: &[CommittedBlob]) -> Result<u64, StoreError> {
        let mut conn = self.tx("store.backfill_blobs");
        let tx = conn.transaction()?;
        let mut added = 0;
        {
            let mut insert = tx.prepare(
                "INSERT INTO committed_blobs (account_id, blob_key, size_bytes)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM accounts WHERE account_id = ?1)
                 ON CONFLICT (account_id, blob_key) DO NOTHING",
            )?;
            for blob in blobs {
                let inserted = insert.execute(params![
                    blob.account_id,
                    &blob.blob_key[..],
                    signed(blob.size_bytes)
                ])?;
                added += u64::from(inserted > 0);
            }
        }
        tx.commit()?;
        Ok(added)
    }
}
