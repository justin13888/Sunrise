//! The end of an account's and a blob's life: the state migration 0003 added,
//! and every statement over it.
//!
//! - **Account deletion** is two steps, as `docs/06-server/api.md` §Account
//!   specifies: a confirmation token, then the request that consumes it and
//!   marks the account. The mark refuses new work at once; the erasure is the
//!   maintenance pass's, once the grace period has run.
//! - **Blob deletion** writes a tombstone. The ciphertext is reclaimed only
//!   when the grace period has passed *and* every active device has declared,
//!   through its subscribe cursors, that it applied the op the tombstone
//!   names (`docs/02-domain/attachments.md` §Deletion).
//! - **The operator's read side** — the summaries and counts `admin` prints —
//!   lives here too, because it reads the same tables.

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::{unsigned, Store, StoreError};

/// Migration 0003, and therefore frozen: change these tables by a new
/// migration, never an edit here.
///
/// - `accounts.delete_requested_at_ms`: set by `DELETE /api/v1/accounts/me`.
///   A non-NULL value refuses new sync sessions, op publishes and uploads at
///   once, and the maintenance pass erases the account once
///   `[storage] account_delete_grace_days` have passed since it.
/// - `account_delete_tokens`: the one live confirmation phrase per account,
///   held as a BLAKE3 hash so a copy of the database cannot confirm a
///   deletion.
/// - `blob_tombstones`: `DELETE /api/v1/blobs/{blob_id}`'s record, naming the
///   op every active device must acknowledge before the blob is reclaimed.
/// - `device_cursors`: the per-stream, per-origin cursors each device last
///   declared on `POST /sync/subscribe` — the acknowledgement the quorum is
///   computed from, since the relay can read nothing inside an op.
///
/// Every table cascades from the account or device row it hangs off, so
/// erasing an account row erases all of it.
pub(super) const SCHEMA: &str = r"
ALTER TABLE accounts ADD COLUMN delete_requested_at_ms INTEGER;
CREATE INDEX IF NOT EXISTS accounts_pending_deletion
    ON accounts(delete_requested_at_ms)
    WHERE delete_requested_at_ms IS NOT NULL;
CREATE TABLE IF NOT EXISTS account_delete_tokens (
    account_id    TEXT PRIMARY KEY REFERENCES accounts(account_id) ON DELETE CASCADE,
    token_h       BLOB NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS blob_tombstones (
    account_id       TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    blob_key         BLOB NOT NULL,
    stream_id        BLOB NOT NULL,
    origin_device    BLOB NOT NULL,
    seq              INTEGER NOT NULL,
    deleted_by       TEXT,
    tombstoned_at_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, blob_key)
);
CREATE INDEX IF NOT EXISTS blob_tombstones_by_age ON blob_tombstones(tombstoned_at_ms);
CREATE TABLE IF NOT EXISTS device_cursors (
    device_id      TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    stream_id      BLOB NOT NULL,
    origin_device  BLOB NOT NULL,
    applied_seq    INTEGER NOT NULL,
    reported_at_ms INTEGER NOT NULL,
    PRIMARY KEY (device_id, stream_id, origin_device)
);";

/// The op whose application retires a blob, as `DELETE /blobs/{blob_id}`
/// names it: the routing head the relay already reads off every op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTombstone {
    /// The blob's 16-byte content key.
    pub blob_key: [u8; 16],
    /// The stream the detaching op was published in.
    pub stream_id: [u8; 16],
    /// The device that published it.
    pub origin_device: [u8; 16],
    /// Its `seq` from that device.
    pub seq: u64,
    /// The relay device row that asked, excused from the quorum because it
    /// wrote the op. `None` where no device signed.
    pub deleted_by: Option<String>,
}

/// One cursor a device declared on `POST /sync/subscribe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredCursor {
    /// The stream.
    pub stream_id: [u8; 16],
    /// The originating device the cursor is about.
    pub origin_device: [u8; 16],
    /// The highest `seq` from it the declaring device has applied.
    pub applied_seq: u64,
}

/// An account as the operator sees it. No email and no OIDC subject: the CLI's
/// output ends up in terminals, tickets and shell history.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AccountSummary {
    /// The relay's account id.
    pub account_id: String,
    /// Creation time, ms since the epoch.
    pub created_at_ms: u64,
    /// When deletion was requested, if it has been.
    pub delete_requested_at_ms: Option<u64>,
    /// Unrevoked device rows.
    pub devices_active: u64,
    /// Revoked device rows.
    pub devices_revoked: u64,
    /// Blobs tombstoned and not yet collected.
    pub tombstones: u64,
}

/// Row counts across the database, for `admin stats`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct StoreStats {
    /// Account rows.
    pub accounts: u64,
    /// Accounts marked for deletion and not yet erased.
    pub accounts_pending_deletion: u64,
    /// Unrevoked device rows.
    pub devices_active: u64,
    /// Revoked device rows.
    pub devices_revoked: u64,
    /// Registered push tokens.
    pub push_tokens: u64,
    /// Frames in the durable relay log.
    pub relay_frames: u64,
    /// Ciphertext bytes in the durable relay log.
    pub relay_bytes: u64,
    /// Blobs tombstoned and not yet collected.
    pub blob_tombstones: u64,
}

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn count(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<u64> {
    Ok(unsigned(r.get::<_, i64>(i)?))
}

const SUMMARY: &str = "SELECT a.account_id, a.created_at_ms, a.delete_requested_at_ms,
        (SELECT COUNT(*) FROM devices d WHERE d.account_id = a.account_id AND d.revoked = 0),
        (SELECT COUNT(*) FROM devices d WHERE d.account_id = a.account_id AND d.revoked = 1),
        (SELECT COUNT(*) FROM blob_tombstones t WHERE t.account_id = a.account_id)
   FROM accounts a";

fn row_to_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountSummary> {
    Ok(AccountSummary {
        account_id: r.get(0)?,
        created_at_ms: count(r, 1)?,
        delete_requested_at_ms: r.get::<_, Option<i64>>(2)?.map(unsigned),
        devices_active: count(r, 3)?,
        devices_revoked: count(r, 4)?,
        tombstones: count(r, 5)?,
    })
}

impl Store {
    /// Hold `token_h` as the account's one live deletion token, replacing any
    /// earlier one: initiating again is how a user whose token lapsed gets
    /// another, and the old one should stop working when they do.
    pub fn put_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        expires_at_ms: u64,
    ) -> Result<(), StoreError> {
        self.conn.lock().execute(
            "INSERT INTO account_delete_tokens (account_id, token_h, expires_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (account_id) DO UPDATE
                SET token_h = excluded.token_h, expires_at_ms = excluded.expires_at_ms",
            params![account_id, &token_h[..], signed(expires_at_ms)],
        )?;
        Ok(())
    }

    /// Consume the account's token if `token_h` is it and it has not expired.
    ///
    /// Single use by construction: the row that matches is the row deleted, so
    /// a replay of the same phrase finds nothing. A wrong phrase leaves the
    /// live token in place, so a typo does not cost the user a fresh initiate.
    pub fn consume_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.lock().execute(
            "DELETE FROM account_delete_tokens
              WHERE account_id = ?1 AND token_h = ?2 AND expires_at_ms > ?3",
            params![account_id, &token_h[..], signed(now_ms)],
        )?;
        Ok(changed == 1)
    }

    /// Mark the account for deletion, returning when it was first marked.
    ///
    /// The first request's time stands: a second one must not push the
    /// erasure further out.
    pub fn request_account_deletion(
        &self,
        account_id: &str,
        now_ms: u64,
    ) -> Result<u64, StoreError> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE accounts
                SET delete_requested_at_ms = COALESCE(delete_requested_at_ms, ?2)
              WHERE account_id = ?1",
            params![account_id, signed(now_ms)],
        )?;
        conn.query_row(
            "SELECT delete_requested_at_ms FROM accounts WHERE account_id = ?1",
            params![account_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .map(unsigned)
        .ok_or(StoreError::NotFound)
    }

    /// When the account's deletion was requested, if it was.
    pub fn account_deletion_requested(&self, account_id: &str) -> Result<Option<u64>, StoreError> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT delete_requested_at_ms FROM accounts WHERE account_id = ?1",
                params![account_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
            .map(unsigned))
    }

    /// Accounts whose deletion was requested at or before `requested_by_ms`.
    pub fn accounts_due_for_erasure(
        &self,
        requested_by_ms: u64,
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT account_id FROM accounts
              WHERE delete_requested_at_ms IS NOT NULL AND delete_requested_at_ms <= ?1
              ORDER BY delete_requested_at_ms",
        )?;
        let ids = stmt
            .query_map(params![signed(requested_by_ms)], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Erase every row the account owns, in one transaction, returning whether
    /// there was an account to erase.
    ///
    /// The account row cascades to its devices, their push tokens and cursors,
    /// its deletion token and its tombstones. The relay log is keyed by the
    /// account's hash rather than by a foreign key, so it is named here. The
    /// blob tree on disk is the caller's: a filesystem delete cannot join this
    /// transaction, and doing it after the commit means a crash between the two
    /// leaves files with no account — which the next erasure pass, keyed on
    /// the same hash, can still find — rather than an account with no files.
    pub fn erase_account(&self, account_id: &str) -> Result<bool, StoreError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        crate::relay_log::erase_account(&tx, crate::relay_log::account_key(account_id))?;
        let erased = tx.execute(
            "DELETE FROM accounts WHERE account_id = ?1",
            params![account_id],
        )?;
        tx.commit()?;
        Ok(erased == 1)
    }

    /// Record a blob's tombstone. A blob already tombstoned keeps its first
    /// one, whose grace period is already running.
    pub fn tombstone_blob(
        &self,
        account_id: &str,
        t: &NewTombstone,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.conn.lock().execute(
            "INSERT INTO blob_tombstones
                 (account_id, blob_key, stream_id, origin_device, seq, deleted_by, tombstoned_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (account_id, blob_key) DO NOTHING",
            params![
                account_id,
                &t.blob_key[..],
                &t.stream_id[..],
                &t.origin_device[..],
                signed(t.seq),
                t.deleted_by,
                signed(now_ms),
            ],
        )?;
        Ok(())
    }

    /// Forget a blob's tombstone: it was re-uploaded, or it has been collected.
    pub fn clear_tombstone(&self, account_id: &str, blob_key: &[u8; 16]) -> Result<(), StoreError> {
        self.conn.lock().execute(
            "DELETE FROM blob_tombstones WHERE account_id = ?1 AND blob_key = ?2",
            params![account_id, &blob_key[..]],
        )?;
        Ok(())
    }

    /// Replace what `device_id` has declared about each stream it subscribed
    /// to, stamping every row `now_ms`.
    ///
    /// The newest declaration is the truth, so a cursor that moved backwards —
    /// a device that reset its vault — is believed: it really has not applied
    /// what it no longer claims.
    pub fn record_cursors(
        &self,
        device_id: &str,
        cursors: &[DeclaredCursor],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for c in cursors {
            tx.execute(
                "INSERT INTO device_cursors
                     (device_id, stream_id, origin_device, applied_seq, reported_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (device_id, stream_id, origin_device) DO UPDATE
                    SET applied_seq = excluded.applied_seq,
                        reported_at_ms = excluded.reported_at_ms",
                params![
                    device_id,
                    &c.stream_id[..],
                    &c.origin_device[..],
                    signed(c.applied_seq),
                    signed(now_ms),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Tombstoned blobs that may be reclaimed now: tombstoned at or before
    /// `tombstoned_by_ms`, and acknowledged by every active device.
    ///
    /// A device is **active** when it is unrevoked and has declared a cursor
    /// at or after `active_since_ms`; one silent longer is abandoned and left
    /// out, as `attachments.md` §Deletion specifies, so a lost phone does not
    /// pin a blob forever. The device that asked for the deletion is left out
    /// too: it published the op. Every other active device must have declared,
    /// for the tombstone's stream and origin device, a cursor at or past the
    /// tombstone's `seq`.
    pub fn collectable_blobs(
        &self,
        tombstoned_by_ms: u64,
        active_since_ms: u64,
    ) -> Result<Vec<(String, [u8; 16])>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT t.account_id, t.blob_key FROM blob_tombstones t
              WHERE t.tombstoned_at_ms <= ?1
                AND NOT EXISTS (
                    SELECT 1 FROM devices d
                     WHERE d.account_id = t.account_id
                       AND d.revoked = 0
                       AND d.device_id IS NOT t.deleted_by
                       AND EXISTS (SELECT 1 FROM device_cursors c
                                    WHERE c.device_id = d.device_id
                                      AND c.reported_at_ms >= ?2)
                       AND NOT EXISTS (SELECT 1 FROM device_cursors c
                                        WHERE c.device_id = d.device_id
                                          AND c.stream_id = t.stream_id
                                          AND c.origin_device = t.origin_device
                                          AND c.applied_seq >= t.seq))
              ORDER BY t.tombstoned_at_ms",
        )?;
        let rows = stmt
            .query_map(
                params![signed(tombstoned_by_ms), signed(active_since_ms)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(a, k)| <[u8; 16]>::try_from(k.as_slice()).ok().map(|k| (a, k)))
            .collect())
    }

    /// Every account, oldest first.
    pub fn account_summaries(&self) -> Result<Vec<AccountSummary>, StoreError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{SUMMARY} ORDER BY a.created_at_ms"))?;
        let rows = stmt
            .query_map([], row_to_summary)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// One account.
    pub fn account_summary(&self, account_id: &str) -> Result<Option<AccountSummary>, StoreError> {
        Ok(self
            .conn
            .lock()
            .query_row(
                &format!("{SUMMARY} WHERE a.account_id = ?1"),
                params![account_id],
                row_to_summary,
            )
            .optional()?)
    }

    /// The account a device row belongs to.
    pub fn device_owner(&self, device_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT account_id FROM devices WHERE device_id = ?1",
                params![device_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Row counts across the database.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let conn = self.conn.lock();
        let mut s = conn.query_row(
            "SELECT
                (SELECT COUNT(*) FROM accounts),
                (SELECT COUNT(*) FROM accounts WHERE delete_requested_at_ms IS NOT NULL),
                (SELECT COUNT(*) FROM devices WHERE revoked = 0),
                (SELECT COUNT(*) FROM devices WHERE revoked = 1),
                (SELECT COUNT(*) FROM push_tokens),
                (SELECT COUNT(*) FROM blob_tombstones)",
            [],
            |r| {
                Ok(StoreStats {
                    accounts: count(r, 0)?,
                    accounts_pending_deletion: count(r, 1)?,
                    devices_active: count(r, 2)?,
                    devices_revoked: count(r, 3)?,
                    push_tokens: count(r, 4)?,
                    blob_tombstones: count(r, 5)?,
                    ..StoreStats::default()
                })
            },
        )?;
        (s.relay_frames, s.relay_bytes) = crate::relay_log::totals(&conn)?;
        Ok(s)
    }

    /// Write a consistent copy of the whole database to `dest`, which must not
    /// exist.
    ///
    /// `VACUUM INTO` reads under one transaction, so the copy is a single
    /// instant even while the relay keeps writing, and the result is one
    /// self-contained file with no `-wal` beside it.
    pub fn snapshot_to(&self, dest: &std::path::Path) -> Result<(), StoreError> {
        let dest = dest
            .to_str()
            .ok_or_else(|| rusqlite::Error::InvalidPath(dest.to_path_buf()))?;
        self.conn.lock().execute("VACUUM INTO ?1", params![dest])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Subject;
    use crate::store::NewDevice;

    const NOW: u64 = 1_800_000_000_000;
    const DAY: u64 = 24 * 60 * 60 * 1000;
    const STREAM: [u8; 16] = [0x11; 16];
    const ORIGIN: [u8; 16] = [0x22; 16];
    const BLOB: [u8; 16] = [0x33; 16];

    fn account(s: &Store, sub: &str) -> String {
        s.resolve_account(&Subject::new("https://idp.example", sub), true, NOW)
            .unwrap()
            .account_id
    }

    fn device(s: &Store, account_id: &str) -> String {
        s.register_device(
            account_id,
            &NewDevice {
                device_pub_s: "k".into(),
                vault_device_id: None,
                device_pub_d: None,
                device_cert: None,
                nickname: "d".into(),
                platform: "linux".into(),
                app_version: None,
            },
            NOW,
        )
        .unwrap()
        .device_id
    }

    fn tombstone(seq: u64, deleted_by: Option<String>) -> NewTombstone {
        NewTombstone {
            blob_key: BLOB,
            stream_id: STREAM,
            origin_device: ORIGIN,
            seq,
            deleted_by,
        }
    }

    fn declare(s: &Store, device_id: &str, applied_seq: u64, at: u64) {
        s.record_cursors(
            device_id,
            &[DeclaredCursor {
                stream_id: STREAM,
                origin_device: ORIGIN,
                applied_seq,
            }],
            at,
        )
        .unwrap();
    }

    /// The quorum, one condition at a time: an active device behind the
    /// tombstone holds the blob, catching up releases it, a device silent past
    /// the activity window stops counting, and so does a revoked one.
    #[test]
    fn a_tombstone_waits_for_every_active_device_and_no_longer() {
        let s = Store::open(None).unwrap();
        let a = account(&s, "alice");
        let deleter = device(&s, &a);
        let peer = device(&s, &a);
        s.tombstone_blob(&a, &tombstone(7, Some(deleter.clone())), NOW)
            .unwrap();

        // Nobody has declared anything: no device is active, so none holds it.
        assert_eq!(s.collectable_blobs(NOW, NOW - 30 * DAY).unwrap().len(), 1);

        declare(&s, &peer, 6, NOW + 1);
        assert!(
            s.collectable_blobs(NOW, NOW - 30 * DAY).unwrap().is_empty(),
            "an active device that has not applied seq 7 holds the blob"
        );
        assert!(
            s.collectable_blobs(NOW - 1, 0).unwrap().is_empty(),
            "nor is a tombstone newer than the cutoff collectable"
        );

        // Silent for longer than the activity window: abandoned.
        assert_eq!(
            s.collectable_blobs(NOW, NOW + 2).unwrap(),
            vec![(a.clone(), BLOB)]
        );

        declare(&s, &peer, 7, NOW + 2);
        // The deleter declares nothing about the op it wrote, and is excused.
        declare(&s, &deleter, 0, NOW + 2);
        assert_eq!(
            s.collectable_blobs(NOW, NOW - 30 * DAY).unwrap(),
            vec![(a.clone(), BLOB)]
        );

        // A cursor that moves back is believed, and a revoked device stops
        // counting.
        declare(&s, &peer, 1, NOW + 3);
        assert!(s.collectable_blobs(NOW, NOW - 30 * DAY).unwrap().is_empty());
        s.revoke_device(&a, &peer, NOW + 4).unwrap();
        assert_eq!(s.collectable_blobs(NOW, NOW - 30 * DAY).unwrap().len(), 1);
    }

    /// The first tombstone's clock keeps running, and clearing one (a
    /// re-upload of the same ciphertext) takes the blob out of the queue.
    #[test]
    fn a_second_tombstone_does_not_restart_the_grace_period() {
        let s = Store::open(None).unwrap();
        let a = account(&s, "alice");
        s.tombstone_blob(&a, &tombstone(1, None), NOW).unwrap();
        s.tombstone_blob(&a, &tombstone(9, None), NOW + 10 * DAY)
            .unwrap();
        assert_eq!(s.collectable_blobs(NOW, 0).unwrap().len(), 1);
        s.clear_tombstone(&a, &BLOB).unwrap();
        assert!(s.collectable_blobs(NOW + 100 * DAY, 0).unwrap().is_empty());
    }

    /// The token is single use, expires, and a wrong one leaves the right one
    /// standing.
    #[test]
    fn a_deletion_token_is_consumed_once_and_only_before_it_expires() {
        let s = Store::open(None).unwrap();
        let a = account(&s, "alice");
        s.put_delete_token(&a, &[1; 32], NOW + 1000).unwrap();
        assert!(!s.consume_delete_token(&a, &[2; 32], NOW).unwrap());
        assert!(s.consume_delete_token(&a, &[1; 32], NOW).unwrap());
        assert!(!s.consume_delete_token(&a, &[1; 32], NOW).unwrap());

        s.put_delete_token(&a, &[3; 32], NOW + 1000).unwrap();
        assert!(!s.consume_delete_token(&a, &[3; 32], NOW + 1000).unwrap());
    }

    /// The first request's time stands, and erasure takes every row the
    /// account owns with it, the relay log included, and nobody else's.
    #[test]
    fn erasure_removes_the_account_and_everything_keyed_by_it() {
        let s = Store::open(None).unwrap();
        let a = account(&s, "alice");
        let b = account(&s, "bob");
        let d = device(&s, &a);
        s.upsert_push_token(&d, "apns", "tok", NOW).unwrap();
        declare(&s, &d, 1, NOW);
        s.tombstone_blob(&a, &tombstone(1, None), NOW).unwrap();
        s.put_delete_token(&a, &[1; 32], NOW + 1).unwrap();
        for who in [&a, &b] {
            let key = (crate::relay_log::account_key(who), STREAM);
            s.relay_append(
                key,
                b"frame",
                &[],
                None,
                1,
                NOW,
                crate::relay_log::DurableCaps::default(),
            )
            .unwrap();
        }

        assert_eq!(s.request_account_deletion(&a, NOW).unwrap(), NOW);
        assert_eq!(s.request_account_deletion(&a, NOW + 5).unwrap(), NOW);
        assert!(s.accounts_due_for_erasure(NOW - 1).unwrap().is_empty());
        assert_eq!(s.accounts_due_for_erasure(NOW).unwrap(), vec![a.clone()]);

        assert!(s.erase_account(&a).unwrap());
        assert!(!s.erase_account(&a).unwrap());

        let conn = s.conn.lock();
        let left = |sql: &str, arg: &dyn rusqlite::ToSql| -> i64 {
            conn.query_row(sql, [arg], |r| r.get(0)).unwrap()
        };
        for table in [
            "accounts",
            "devices",
            "account_delete_tokens",
            "blob_tombstones",
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE account_id = ?1");
            assert_eq!(left(&sql, &a), 0, "{table}");
        }
        for table in ["push_tokens", "device_cursors"] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE device_id = ?1");
            assert_eq!(left(&sql, &d), 0, "{table}");
        }
        let frames = "SELECT COUNT(*) FROM relay_frames WHERE account_h = ?1";
        let ah = crate::relay_log::account_key(&a).to_vec();
        assert_eq!(left(frames, &ah), 0);
        let bh = crate::relay_log::account_key(&b).to_vec();
        assert_eq!(left(frames, &bh), 1, "another account's log is untouched");
    }

    #[test]
    fn stats_and_summaries_count_what_is_there() {
        let s = Store::open(None).unwrap();
        let a = account(&s, "alice");
        let d = device(&s, &a);
        device(&s, &a);
        s.revoke_device(&a, &d, NOW).unwrap();
        s.request_account_deletion(&a, NOW).unwrap();
        account(&s, "bob");

        let stats = s.stats().unwrap();
        assert_eq!(stats.accounts, 2);
        assert_eq!(stats.accounts_pending_deletion, 1);
        assert_eq!((stats.devices_active, stats.devices_revoked), (1, 1));

        let summary = s.account_summary(&a).unwrap().unwrap();
        assert_eq!(summary.delete_requested_at_ms, Some(NOW));
        assert_eq!((summary.devices_active, summary.devices_revoked), (1, 1));
        assert_eq!(s.account_summaries().unwrap().len(), 2);
        assert_eq!(s.device_owner(&d).unwrap(), Some(a));
        assert!(s.account_summary("nobody").unwrap().is_none());
    }
}
