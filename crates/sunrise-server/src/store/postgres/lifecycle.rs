//! The rest of an account's life on Postgres: deletion, tombstones, cursors,
//! the blob ledger, the pins and the recovery material — the statements of
//! `store/lifecycle.rs`, `blob_ledger.rs`, `snapshots.rs`, `key_bundles.rs` and
//! `recovery.rs`, with the same contracts.
//!
//! A method that reads a row and then writes on the strength of what it read —
//! a conditional pin, a recovery blob replaced with its old check kept — first
//! locks the account's row, so two nodes doing it at once take turns as the
//! SQLite store's one connection makes them.

use tokio_postgres::{Row, Transaction};

use super::{key16, signed, unavailable, unsigned, PgStore};
use crate::store::recovery::MAX_RETIRED;
use crate::store::tx::op;
use crate::store::{
    AccountSummary, CommittedBlob, DeclaredCursor, IdentityChainPinned, KeyBundlePinned,
    MetadataError, NewTombstone, PinnedIdentityChain, PinnedKeyBundle, PinnedSnapshot,
    RetiredRecoveryBlob, StoreStats,
};

const SUMMARY: &str = "SELECT a.account_id, a.created_at_ms, a.delete_requested_at_ms,
        (SELECT COUNT(*) FROM devices d WHERE d.account_id = a.account_id AND NOT d.revoked),
        (SELECT COUNT(*) FROM devices d WHERE d.account_id = a.account_id AND d.revoked),
        (SELECT COUNT(*) FROM blob_tombstones t WHERE t.account_id = a.account_id)
   FROM accounts a";

fn summary(r: &Row) -> AccountSummary {
    AccountSummary {
        account_id: r.get(0),
        created_at_ms: unsigned(r.get(1)),
        delete_requested_at_ms: r.get::<_, Option<i64>>(2).map(unsigned),
        devices_active: unsigned(r.get(3)),
        devices_revoked: unsigned(r.get(4)),
        tombstones: unsigned(r.get(5)),
    }
}

/// Lock `account_id`'s row for the rest of `tx`, reporting whether it exists.
async fn lock_account(tx: &Transaction<'_>, account_id: &str) -> Result<bool, MetadataError> {
    Ok(tx
        .query_opt(
            "SELECT 1 FROM accounts WHERE account_id = $1 FOR UPDATE",
            &[&account_id],
        )
        .await
        .map_err(unavailable)?
        .is_some())
}

impl PgStore {
    pub(super) async fn put_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        expires_at_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.put_delete_token");
        self.client()
            .await?
            .execute(
                "INSERT INTO account_delete_tokens (account_id, token_h, expires_at_ms)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (account_id) DO UPDATE
                    SET token_h = EXCLUDED.token_h, expires_at_ms = EXCLUDED.expires_at_ms",
                &[&account_id, &&token_h[..], &signed(expires_at_ms)],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn consume_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<bool, MetadataError> {
        let _op = op("store.consume_delete_token");
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM account_delete_tokens
                  WHERE account_id = $1 AND token_h = $2 AND expires_at_ms > $3",
                &[&account_id, &&token_h[..], &signed(now_ms)],
            )
            .await
            .map_err(unavailable)?;
        Ok(n == 1)
    }

    pub(super) async fn request_account_deletion(
        &self,
        account_id: &str,
        now_ms: u64,
    ) -> Result<u64, MetadataError> {
        let _op = op("store.request_account_deletion");
        let row = self
            .client()
            .await?
            .query_opt(
                "UPDATE accounts
                    SET delete_requested_at_ms = COALESCE(delete_requested_at_ms, $2)
                  WHERE account_id = $1
              RETURNING delete_requested_at_ms",
                &[&account_id, &signed(now_ms)],
            )
            .await
            .map_err(unavailable)?
            .ok_or(MetadataError::NotFound)?;
        Ok(unsigned(row.get(0)))
    }

    pub(super) async fn account_deletion_requested(
        &self,
        account_id: &str,
    ) -> Result<Option<u64>, MetadataError> {
        let _op = op("store.account_deletion_requested");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT delete_requested_at_ms FROM accounts WHERE account_id = $1",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.and_then(|r| r.get::<_, Option<i64>>(0)).map(unsigned))
    }

    pub(super) async fn accounts_due_for_erasure(
        &self,
        requested_by_ms: u64,
    ) -> Result<Vec<String>, MetadataError> {
        let _op = op("store.accounts_due_for_erasure");
        let rows = self
            .client()
            .await?
            .query(
                "SELECT account_id FROM accounts
                  WHERE delete_requested_at_ms IS NOT NULL AND delete_requested_at_ms <= $1
                  ORDER BY delete_requested_at_ms",
                &[&signed(requested_by_ms)],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// `Store::erase_account`: the relay log, keyed by the account's hash,
    /// and the account row, which cascades to everything else, together.
    ///
    /// Under the account's lock held exclusively, which every append to one of
    /// its channels holds shared before it touches a row, so the two take
    /// turns instead of deadlocking.
    pub(super) async fn erase_account(&self, account_id: &str) -> Result<bool, MetadataError> {
        let _op = op("store.erase_account");
        let account_h = crate::account_ns::account_key(account_id);
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        super::relay_log::lock_account(&tx, account_h).await?;
        for table in ["relay_batches", "relay_evicted", "relay_frames"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE account_h = $1"),
                &[&&account_h[..]],
            )
            .await
            .map_err(unavailable)?;
        }
        let n = tx
            .execute("DELETE FROM accounts WHERE account_id = $1", &[&account_id])
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(n == 1)
    }

    pub(super) async fn tombstone_blob(
        &self,
        account_id: &str,
        t: &NewTombstone,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.tombstone_blob");
        self.client()
            .await?
            .execute(
                "INSERT INTO blob_tombstones (account_id, blob_key, stream_id, origin_device, seq,
                     deleted_by, tombstoned_at_ms)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (account_id, blob_key) DO NOTHING",
                &[
                    &account_id,
                    &&t.blob_key[..],
                    &&t.stream_id[..],
                    &&t.origin_device[..],
                    &signed(t.seq),
                    &t.deleted_by,
                    &signed(now_ms),
                ],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn clear_tombstone(
        &self,
        account_id: &str,
        blob_key: &[u8; 16],
    ) -> Result<(), MetadataError> {
        let _op = op("store.clear_tombstone");
        self.client()
            .await?
            .execute(
                "DELETE FROM blob_tombstones WHERE account_id = $1 AND blob_key = $2",
                &[&account_id, &&blob_key[..]],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn record_cursors(
        &self,
        device_id: &str,
        cursors: &[DeclaredCursor],
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.record_cursors");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        for cur in cursors {
            tx.execute(
                "INSERT INTO device_cursors
                     (device_id, stream_id, origin_device, applied_seq, reported_at_ms)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (device_id, stream_id, origin_device) DO UPDATE
                    SET applied_seq = EXCLUDED.applied_seq,
                        reported_at_ms = EXCLUDED.reported_at_ms",
                &[
                    &device_id,
                    &&cur.stream_id[..],
                    &&cur.origin_device[..],
                    &signed(cur.applied_seq),
                    &signed(now_ms),
                ],
            )
            .await
            .map_err(unavailable)?;
        }
        tx.commit().await.map_err(unavailable)
    }

    /// `Store::collectable_blobs`: the same quorum, word for word but for
    /// `IS DISTINCT FROM`, Postgres's spelling of SQLite's `IS NOT`.
    pub(super) async fn collectable_blobs(
        &self,
        tombstoned_by_ms: u64,
        active_since_ms: u64,
    ) -> Result<Vec<(String, [u8; 16])>, MetadataError> {
        let _op = op("store.collectable_blobs");
        let rows = self
            .client()
            .await?
            .query(
                "SELECT t.account_id, t.blob_key FROM blob_tombstones t
                  WHERE t.tombstoned_at_ms <= $1
                    AND NOT EXISTS (
                        SELECT 1 FROM devices d
                         WHERE d.account_id = t.account_id
                           AND NOT d.revoked
                           AND d.device_id IS DISTINCT FROM t.deleted_by
                           AND EXISTS (SELECT 1 FROM device_cursors c
                                        WHERE c.device_id = d.device_id
                                          AND c.reported_at_ms >= $2)
                           AND NOT EXISTS (SELECT 1 FROM device_cursors c
                                            WHERE c.device_id = d.device_id
                                              AND c.stream_id = t.stream_id
                                              AND c.origin_device = t.origin_device
                                              AND c.applied_seq >= t.seq))
                    AND NOT EXISTS (SELECT 1 FROM stream_snapshots s
                                     WHERE s.account_id = t.account_id
                                       AND s.blob_key = t.blob_key)
                    AND NOT EXISTS (SELECT 1 FROM key_bundles k
                                     WHERE k.account_id = t.account_id
                                       AND k.blob_key = t.blob_key)
                  ORDER BY t.tombstoned_at_ms",
                &[&signed(tombstoned_by_ms), &signed(active_since_ms)],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows
            .iter()
            .filter_map(|r| key16(r.get(1)).map(|k| (r.get(0), k)))
            .collect())
    }

    pub(super) async fn account_summaries(&self) -> Result<Vec<AccountSummary>, MetadataError> {
        let _op = op("store.account_summaries");
        let rows = self
            .client()
            .await?
            .query(&format!("{SUMMARY} ORDER BY a.created_at_ms"), &[])
            .await
            .map_err(unavailable)?;
        Ok(rows.iter().map(summary).collect())
    }

    pub(super) async fn account_summary(
        &self,
        account_id: &str,
    ) -> Result<Option<AccountSummary>, MetadataError> {
        let _op = op("store.account_summary");
        let row = self
            .client()
            .await?
            .query_opt(
                &format!("{SUMMARY} WHERE a.account_id = $1"),
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.as_ref().map(summary))
    }

    pub(super) async fn stats(&self) -> Result<StoreStats, MetadataError> {
        let _op = op("store.stats");
        let r = self
            .client()
            .await?
            .query_one(
                "SELECT
                    (SELECT COUNT(*) FROM accounts),
                    (SELECT COUNT(*) FROM accounts WHERE delete_requested_at_ms IS NOT NULL),
                    (SELECT COUNT(*) FROM devices WHERE NOT revoked),
                    (SELECT COUNT(*) FROM devices WHERE revoked),
                    (SELECT COUNT(*) FROM push_tokens),
                    (SELECT COUNT(*) FROM blob_tombstones),
                    (SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM committed_blobs),
                    (SELECT COUNT(*) FROM relay_frames),
                    (SELECT COALESCE(SUM(n_bytes), 0)::BIGINT FROM relay_frames)",
                &[],
            )
            .await
            .map_err(unavailable)?;
        Ok(StoreStats {
            accounts: unsigned(r.get(0)),
            accounts_pending_deletion: unsigned(r.get(1)),
            devices_active: unsigned(r.get(2)),
            devices_revoked: unsigned(r.get(3)),
            push_tokens: unsigned(r.get(4)),
            blob_tombstones: unsigned(r.get(5)),
            blob_bytes: unsigned(r.get(6)),
            relay_frames: unsigned(r.get(7)),
            relay_bytes: unsigned(r.get(8)),
        })
    }

    pub(super) async fn record_blob(&self, blob: &CommittedBlob) -> Result<(), MetadataError> {
        let _op = op("store.record_blob");
        self.client()
            .await?
            .execute(
                "INSERT INTO committed_blobs (account_id, blob_key, size_bytes)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (account_id, blob_key) DO UPDATE SET size_bytes = EXCLUDED.size_bytes",
                &[
                    &blob.account_id,
                    &&blob.blob_key[..],
                    &signed(blob.size_bytes),
                ],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn forget_blob(
        &self,
        account_id: &str,
        blob_key: &[u8; 16],
    ) -> Result<(), MetadataError> {
        let _op = op("store.forget_blob");
        self.client()
            .await?
            .execute(
                "DELETE FROM committed_blobs WHERE account_id = $1 AND blob_key = $2",
                &[&account_id, &&blob_key[..]],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn backfill_blobs(
        &self,
        blobs: &[CommittedBlob],
    ) -> Result<u64, MetadataError> {
        let _op = op("store.backfill_blobs");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        let mut added = 0;
        for blob in blobs {
            added += tx
                .execute(
                    "INSERT INTO committed_blobs (account_id, blob_key, size_bytes)
                     SELECT $1, $2, $3 WHERE EXISTS (SELECT 1 FROM accounts WHERE account_id = $1)
                     ON CONFLICT (account_id, blob_key) DO NOTHING",
                    &[
                        &blob.account_id,
                        &&blob.blob_key[..],
                        &signed(blob.size_bytes),
                    ],
                )
                .await
                .map_err(unavailable)?;
        }
        tx.commit().await.map_err(unavailable)?;
        Ok(added)
    }

    /// `Store::pin_snapshot`.
    pub(super) async fn pin_snapshot(
        &self,
        account_id: &str,
        pin: &PinnedSnapshot,
    ) -> Result<Option<[u8; 16]>, MetadataError> {
        let _op = op("store.pin_snapshot");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        lock_account(&tx, account_id).await?;
        let before = tx
            .query_opt(
                "SELECT blob_key FROM stream_snapshots WHERE account_id = $1 AND stream_id = $2",
                &[&account_id, &&pin.stream_id[..]],
            )
            .await
            .map_err(unavailable)?
            .and_then(|r| key16(r.get(0)));
        tx.execute(
            "INSERT INTO stream_snapshots
                 (account_id, stream_id, blob_key, size_bytes, published_by, published_at_ms)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (account_id, stream_id) DO UPDATE SET
                 blob_key = EXCLUDED.blob_key, size_bytes = EXCLUDED.size_bytes,
                 published_by = EXCLUDED.published_by,
                 published_at_ms = EXCLUDED.published_at_ms",
            &[
                &account_id,
                &&pin.stream_id[..],
                &&pin.blob_key[..],
                &signed(pin.size_bytes),
                &pin.published_by,
                &signed(pin.published_at_ms),
            ],
        )
        .await
        .map_err(unavailable)?;
        let released = match before {
            Some(old) if old != pin.blob_key => {
                let still: i64 = tx
                    .query_one(
                        "SELECT COUNT(*) FROM stream_snapshots
                         WHERE account_id = $1 AND blob_key = $2",
                        &[&account_id, &&old[..]],
                    )
                    .await
                    .map_err(unavailable)?
                    .get(0);
                (still == 0).then_some(old)
            }
            _ => None,
        };
        tx.commit().await.map_err(unavailable)?;
        Ok(released)
    }

    pub(super) async fn pinned_snapshot(
        &self,
        account_id: &str,
        stream_id: &[u8; 16],
    ) -> Result<Option<PinnedSnapshot>, MetadataError> {
        let _op = op("store.pinned_snapshot");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT blob_key, size_bytes, published_by, published_at_ms
                 FROM stream_snapshots WHERE account_id = $1 AND stream_id = $2",
                &[&account_id, &&stream_id[..]],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.and_then(|r| {
            Some(PinnedSnapshot {
                stream_id: *stream_id,
                blob_key: key16(r.get(0))?,
                size_bytes: unsigned(r.get(1)),
                published_by: r.get(2),
                published_at_ms: unsigned(r.get(3)),
            })
        }))
    }

    /// `Store::pin_key_bundle`: replaces only the pin the publisher read.
    pub(super) async fn pin_key_bundle(
        &self,
        account_id: &str,
        pin: &PinnedKeyBundle,
        replaces: Option<[u8; 16]>,
    ) -> Result<KeyBundlePinned, MetadataError> {
        let _op = op("store.pin_key_bundle");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        lock_account(&tx, account_id).await?;
        let before = tx
            .query_opt(
                "SELECT blob_key FROM key_bundles WHERE account_id = $1",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?
            .and_then(|r| key16(r.get(0)));
        if before != replaces {
            return Ok(KeyBundlePinned::Superseded);
        }
        tx.execute(
            "INSERT INTO key_bundles
                 (account_id, blob_key, size_bytes, manifest_digest, published_by, published_at_ms)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (account_id) DO UPDATE SET
                 blob_key = EXCLUDED.blob_key, size_bytes = EXCLUDED.size_bytes,
                 manifest_digest = EXCLUDED.manifest_digest,
                 published_by = EXCLUDED.published_by,
                 published_at_ms = EXCLUDED.published_at_ms",
            &[
                &account_id,
                &&pin.blob_key[..],
                &signed(pin.size_bytes),
                &&pin.manifest_digest[..],
                &pin.published_by,
                &signed(pin.published_at_ms),
            ],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(KeyBundlePinned::Pinned {
            released: before.filter(|old| *old != pin.blob_key),
        })
    }

    pub(super) async fn pinned_key_bundle(
        &self,
        account_id: &str,
    ) -> Result<Option<PinnedKeyBundle>, MetadataError> {
        let _op = op("store.pinned_key_bundle");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT blob_key, size_bytes, manifest_digest, published_by, published_at_ms
                 FROM key_bundles WHERE account_id = $1",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.and_then(|r| {
            Some(PinnedKeyBundle {
                blob_key: key16(r.get(0))?,
                size_bytes: unsigned(r.get(1)),
                manifest_digest: r.get::<_, &[u8]>(2).try_into().ok()?,
                published_by: r.get(3),
                published_at_ms: unsigned(r.get(4)),
            })
        }))
    }

    /// `Store::pin_identity_chain`: replaces only the chain the publisher
    /// read.
    pub(super) async fn pin_identity_chain(
        &self,
        account_id: &str,
        pin: &PinnedIdentityChain,
        replaces: Option<[u8; 32]>,
    ) -> Result<IdentityChainPinned, MetadataError> {
        let _op = op("store.pin_identity_chain");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        lock_account(&tx, account_id).await?;
        let before: Option<[u8; 32]> = tx
            .query_opt(
                "SELECT digest FROM identity_chains WHERE account_id = $1",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?
            .and_then(|r| r.get::<_, &[u8]>(0).try_into().ok());
        if before != replaces {
            return Ok(IdentityChainPinned::Superseded);
        }
        tx.execute(
            "INSERT INTO identity_chains (account_id, chain, digest, published_by, published_at_ms)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (account_id) DO UPDATE SET
                 chain = EXCLUDED.chain, digest = EXCLUDED.digest,
                 published_by = EXCLUDED.published_by,
                 published_at_ms = EXCLUDED.published_at_ms",
            &[
                &account_id,
                &pin.chain,
                &&pin.digest[..],
                &pin.published_by,
                &signed(pin.published_at_ms),
            ],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(IdentityChainPinned::Pinned)
    }

    pub(super) async fn pinned_identity_chain(
        &self,
        account_id: &str,
    ) -> Result<Option<PinnedIdentityChain>, MetadataError> {
        let _op = op("store.pinned_identity_chain");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT chain, digest, published_by, published_at_ms
                 FROM identity_chains WHERE account_id = $1",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.and_then(|r| {
            Some(PinnedIdentityChain {
                chain: r.get(0),
                digest: r.get::<_, &[u8]>(1).try_into().ok()?,
                published_by: r.get(2),
                published_at_ms: unsigned(r.get(3)),
            })
        }))
    }

    /// `Store::replace_recovery_blob`: the new blob, and the old one's check
    /// kept with the newest [`MAX_RETIRED`] of them.
    pub(super) async fn replace_recovery_blob(
        &self,
        account_id: &str,
        blob: &str,
        check: Option<&[u8]>,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.replace_recovery_blob");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        if !lock_account(&tx, account_id).await? {
            return Err(MetadataError::NotFound);
        }
        tx.execute(
            "UPDATE accounts SET recovery_blob = $2 WHERE account_id = $1",
            &[&account_id, &blob],
        )
        .await
        .map_err(unavailable)?;
        if let Some(check) = check {
            tx.execute(
                "INSERT INTO retired_recovery_blobs (account_id, retired_at_ms, check_prefix)
                 VALUES ($1, $2, $3)",
                &[&account_id, &signed(now_ms), &check],
            )
            .await
            .map_err(unavailable)?;
            tx.execute(
                "DELETE FROM retired_recovery_blobs
                 WHERE account_id = $1 AND id NOT IN (
                     SELECT id FROM retired_recovery_blobs WHERE account_id = $1
                     ORDER BY retired_at_ms DESC, id DESC LIMIT $2)",
                &[&account_id, &MAX_RETIRED],
            )
            .await
            .map_err(unavailable)?;
        }
        tx.commit().await.map_err(unavailable)
    }

    pub(super) async fn retired_recovery_blobs(
        &self,
        account_id: &str,
    ) -> Result<Vec<RetiredRecoveryBlob>, MetadataError> {
        let _op = op("store.retired_recovery_blobs");
        let rows = self
            .client()
            .await?
            .query(
                "SELECT retired_at_ms, check_prefix FROM retired_recovery_blobs
                 WHERE account_id = $1 ORDER BY retired_at_ms DESC, id DESC",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows
            .iter()
            .map(|r| RetiredRecoveryBlob {
                retired_at_ms: unsigned(r.get(0)),
                check: r.get(1),
            })
            .collect())
    }
}
