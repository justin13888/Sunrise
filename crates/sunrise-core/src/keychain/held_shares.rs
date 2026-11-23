//! The Stream keys accepted share grants sealed to this account
//! (`docs/03-crypto/sharing-with-others.md` §Granting access step 6), held in
//! `held_share_keys` (migration 0056).
//!
//! Never in `stream_keys`, and never in [`Keychain::stream_keys_at`]'s cache.
//! Those are this account's own key set: every Stream id in them is rotated
//! (`Keychain::rotation_set`), its writable epochs go into the key bundle,
//! and the cache answers by `(stream_id, epoch)` alone. The owner picks its
//! Stream ids, so a held key filed there under one of this account's own ids
//! would be a key the owner knows among the keys this account seals under.
//! Here the granter's identity is part of every key, and nothing that
//! rotates, publishes or seals reads the table.
//!
//! The wrap is `stream_keys`'s own (`sunrise_crypto::wrap_stream_key` under
//! the vault root, binding the Stream id and epoch). Moving a wrapped blob
//! from this table into `stream_keys` takes a write to the vault, which takes
//! the vault root, so a domain string of its own would protect nothing.

use super::{Keychain, KeychainError, RngAdapter};
use crate::bytes::to16;
use crate::config::Rng;
use rusqlite::params;
use sunrise_crypto::{wrap_stream_key, StreamKey};
use sunrise_storage::Db;

/// A share this account accepted, as `held_share_keys` records it: the
/// newest epoch held for one `(granter, stream)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldShare {
    /// The identity that signed the grant.
    pub granter_identity_id: [u8; 16],
    /// The owner's Stream.
    pub stream_id: [u8; 16],
    /// The newest epoch held.
    pub epoch: u32,
    /// The Stream's name, as the owner sealed it at that epoch.
    pub name: String,
    /// The Stream's color, in its wire spelling.
    pub color: String,
    /// When the share ends; `None` for never.
    pub expires_at_ms: Option<u64>,
    /// When this account accepted that epoch.
    pub accepted_at_ms: u64,
    /// Every relay viewer grant this account claimed for the share, across
    /// the epochs held, without repeats, in byte order: what a decline
    /// withdraws.
    pub relay_grant_ids: Vec<[u8; 16]>,
}

/// One epoch to hold: the row `held_share_keys` takes, less the key.
pub(crate) struct HoldShareKey<'a> {
    pub(crate) granter_identity_id: [u8; 16],
    pub(crate) stream_id: [u8; 16],
    pub(crate) epoch: u32,
    pub(crate) relay_grant_id: [u8; 16],
    pub(crate) name: &'a str,
    pub(crate) color: &'a str,
    pub(crate) expires_at_ms: Option<u64>,
    pub(crate) accepted_at_ms: u64,
}

impl Keychain {
    /// Wrap `key` under the vault root and hold it as `row` says. Accepting
    /// the same epoch from the same granter again replaces the row, so the
    /// relay grant claimed last is the one recorded.
    ///
    /// # Errors
    /// [`KeychainError::WrapFailed`] if the wrap fails; storage errors.
    pub(crate) fn hold_share_key(
        &self,
        db: &Db,
        row: &HoldShareKey<'_>,
        key: &StreamKey,
        rng: &dyn Rng,
    ) -> Result<(), KeychainError> {
        let wrapped = wrap_stream_key(
            &self.vault_root,
            key,
            &row.stream_id,
            row.epoch,
            &mut RngAdapter(rng),
        )
        .map_err(|_| KeychainError::WrapFailed("a held share key"))?;
        db.conn().execute(
            "INSERT OR REPLACE INTO held_share_keys
             (granter_identity_id, stream_id, epoch, wrapped, relay_grant_id,
              name, color, expires_at_ms, accepted_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &row.granter_identity_id[..],
                &row.stream_id[..],
                row.epoch,
                wrapped,
                &row.relay_grant_id[..],
                row.name,
                row.color,
                row.expires_at_ms.map(clamp_ms),
                clamp_ms(row.accepted_at_ms),
            ],
        )?;
        Ok(())
    }

    /// The key held for `stream_id` at `epoch` from `granter_identity_id`,
    /// or `None` where none is held: what a shared Stream's envelope at that
    /// epoch opens under (`Core::read_shared_envelope`).
    ///
    /// # Errors
    /// [`KeychainError::UnreadableRow`] for a row whose wrap does not open under
    /// this vault's root; storage errors.
    pub(crate) fn held_share_key(
        &self,
        db: &Db,
        granter_identity_id: &[u8; 16],
        stream_id: &[u8; 16],
        epoch: u32,
    ) -> Result<Option<StreamKey>, KeychainError> {
        use rusqlite::OptionalExtension;
        use sunrise_crypto::unwrap_stream_key;

        let wrapped: Option<Vec<u8>> = db
            .conn()
            .query_row(
                "SELECT wrapped FROM held_share_keys
                 WHERE granter_identity_id = ? AND stream_id = ? AND epoch = ?",
                params![&granter_identity_id[..], &stream_id[..], epoch],
                |r| r.get(0),
            )
            .optional()?;
        wrapped
            .map(|w| {
                unwrap_stream_key(&self.vault_root, &w, stream_id, epoch)
                    .map_err(|_| KeychainError::UnreadableRow("held_share_keys.wrapped"))
            })
            .transpose()
    }
}

/// Every share this account holds a key for, one per `(granter, stream)` at
/// its newest epoch, ordered by granter and then Stream. A row whose ids are
/// not 16 bytes or whose epoch is outside `u32` is skipped: it names no share
/// anything could read.
///
/// # Errors
/// Storage errors.
pub(crate) fn held_shares(db: &Db) -> rusqlite::Result<Vec<HeldShare>> {
    let mut stmt = db.conn().prepare(
        "SELECT granter_identity_id, stream_id, epoch, relay_grant_id, name, color,
                expires_at_ms, accepted_at_ms
         FROM held_share_keys
         ORDER BY granter_identity_id, stream_id, epoch",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: Vec<HeldShare> = Vec::new();
    for (granter, stream, epoch, grant, name, color, expires, accepted) in rows {
        let (Some(granter), Some(stream), Ok(epoch), Some(grant)) = (
            to16(&granter),
            to16(&stream),
            u32::try_from(epoch),
            to16(&grant),
        ) else {
            continue;
        };
        let row = HeldShare {
            granter_identity_id: granter,
            stream_id: stream,
            epoch,
            name,
            color,
            expires_at_ms: expires.and_then(|e| u64::try_from(e).ok()),
            accepted_at_ms: u64::try_from(accepted).unwrap_or(0),
            relay_grant_ids: vec![grant],
        };
        match out.last_mut() {
            // Ordered by epoch within a share, so the later row is newer.
            Some(last) if last.granter_identity_id == granter && last.stream_id == stream => {
                let mut ids = std::mem::take(&mut last.relay_grant_ids);
                ids.push(grant);
                *last = HeldShare {
                    relay_grant_ids: ids,
                    ..row
                };
            }
            _ => out.push(row),
        }
    }
    for share in &mut out {
        share.relay_grant_ids.sort_unstable();
        share.relay_grant_ids.dedup();
    }
    Ok(out)
}

/// Drop every key held for `stream_id` from `granter_identity_id`. Returns
/// how many epochs were dropped.
///
/// # Errors
/// Storage errors.
pub(crate) fn drop_held_share(
    db: &Db,
    granter_identity_id: &[u8; 16],
    stream_id: &[u8; 16],
) -> rusqlite::Result<usize> {
    db.conn().execute(
        "DELETE FROM held_share_keys WHERE granter_identity_id = ? AND stream_id = ?",
        params![&granter_identity_id[..], &stream_id[..]],
    )
}

/// A time for an `INTEGER` column: SQLite's integers are signed.
fn clamp_ms(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}
