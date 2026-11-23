//! What a share leaves on this device that no op carries (migration 0056).
//!
//! The recipient's: the Stream key an accepted grant sealed, held under the
//! granter's identity and the owner's Stream id, beside the relay viewer
//! grant the account claimed for it
//! (`docs/03-crypto/sharing-with-others.md` §Granting access step 6). A
//! decline withdraws that relay grant and drops the key.
//!
//! The owner's: the id of each relay viewer grant this device registered for
//! a share, so a revocation can withdraw it (§Revoking access step 3). The
//! relay names a grant only by that id, and no share op carries it, so a
//! device that did not register the grant has none to withdraw; the
//! revocation's key rotation is what stops the recipient reading.

use super::{Core, CoreError, ReceivedShare};
use crate::keychain::{drop_held_share, held_shares, HeldShare, HoldShareKey};
use sunrise_id::EntityRef;

impl Core {
    /// Accept `grant`, signed by `granter_id_s_pub`, whose relay viewer grant
    /// this account claimed as `relay_grant_id`: open it as
    /// [`Self::open_share_grant`] does, then hold the Stream key it seals,
    /// wrapped under the vault root, beside that relay grant id and the
    /// Stream's sealed name, color and expiry.
    ///
    /// The key is held in `held_share_keys`, under the granter's identity and
    /// the owner's Stream id, never among this account's own Stream keys:
    /// nothing rotates, publishes or seals under it. Accepting the same epoch
    /// from the same granter again replaces what was held for it.
    ///
    /// # Errors
    /// As [`Self::open_share_grant`], writing nothing; storage failures.
    pub fn accept_share(
        &self,
        granter_id_s_pub: &[u8; 32],
        grant: &[u8],
        relay_grant_id: [u8; 16],
    ) -> Result<ReceivedShare, CoreError> {
        self.ensure_open()?;
        let (share, key) = self.open_grant(granter_id_s_pub, grant)?;
        let db = self.db.lock();
        self.engine.keychain().hold_share_key(
            &db,
            &HoldShareKey {
                granter_identity_id: share.granter_identity_id,
                stream_id: share.stream_id,
                epoch: share.epoch,
                relay_grant_id,
                name: &share.name,
                color: &share.color,
                expires_at_ms: share.expires_at_ms,
                accepted_at_ms: self.now_ms(),
            },
            &key,
            self.rng(),
        )?;
        Ok(share)
    }

    /// Every share this account accepted and still holds a key for, one per
    /// granter and Stream at its newest epoch, ordered by granter and then
    /// Stream.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub fn held_shares(&self) -> Result<Vec<HeldShare>, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok(held_shares(&db)?)
    }

    /// Drop every key held for the owner's `stream_id` from
    /// `granter_identity_id`: the recipient's side of a decline. Withdrawing
    /// the relay grants the share records ([`HeldShare::relay_grant_ids`])
    /// is the caller's, first. Returns whether anything was held.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub fn drop_held_share(
        &self,
        granter_identity_id: [u8; 16],
        stream_id: [u8; 16],
    ) -> Result<bool, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        let dropped = drop_held_share(&db, &granter_identity_id, &stream_id)?;
        Ok(dropped > 0)
    }

    /// Record that this device registered the relay viewer grant
    /// `relay_grant_id` for `stream`'s share with `recipient_identity_id`.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub fn record_share_relay_grant(
        &self,
        stream: EntityRef,
        recipient_identity_id: [u8; 16],
        relay_grant_id: [u8; 16],
    ) -> Result<(), CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        relay_grants::record(
            &db,
            stream.bytes(),
            &recipient_identity_id,
            &relay_grant_id,
            self.now_ms(),
        )?;
        Ok(())
    }

    /// The relay viewer grants this device registered for `stream`'s share
    /// with `recipient_identity_id` and has not forgotten, in byte order.
    /// Empty where another device made the share, or every grant was
    /// withdrawn.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub fn share_relay_grants(
        &self,
        stream: EntityRef,
        recipient_identity_id: [u8; 16],
    ) -> Result<Vec<[u8; 16]>, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok(relay_grants::list(
            &db,
            stream.bytes(),
            &recipient_identity_id,
        )?)
    }

    /// Forget `relay_grant_id` for `stream`'s share with
    /// `recipient_identity_id`, once the relay has let it go.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub fn forget_share_relay_grant(
        &self,
        stream: EntityRef,
        recipient_identity_id: [u8; 16],
        relay_grant_id: [u8; 16],
    ) -> Result<(), CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        relay_grants::forget(&db, stream.bytes(), &recipient_identity_id, &relay_grant_id)?;
        Ok(())
    }
}

/// `share_relay_grants`'s rows.
mod relay_grants {
    use rusqlite::params;
    use sunrise_storage::Db;

    pub(super) fn record(
        db: &Db,
        stream_id: &[u8; 16],
        recipient: &[u8; 16],
        grant: &[u8; 16],
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        db.conn().execute(
            "INSERT OR IGNORE INTO share_relay_grants
             (stream_id, recipient_identity_id, relay_grant_id, created_at_ms)
             VALUES (?, ?, ?, ?)",
            params![
                &stream_id[..],
                &recipient[..],
                &grant[..],
                i64::try_from(now_ms).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    pub(super) fn list(
        db: &Db,
        stream_id: &[u8; 16],
        recipient: &[u8; 16],
    ) -> rusqlite::Result<Vec<[u8; 16]>> {
        let mut stmt = db.conn().prepare(
            "SELECT relay_grant_id FROM share_relay_grants
             WHERE stream_id = ? AND recipient_identity_id = ?
             ORDER BY relay_grant_id",
        )?;
        let ids = stmt
            .query_map(params![&stream_id[..], &recipient[..]], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // A row whose id is not 16 bytes names no grant the relay could
        // withdraw.
        Ok(ids.iter().filter_map(|id| crate::bytes::to16(id)).collect())
    }

    pub(super) fn forget(
        db: &Db,
        stream_id: &[u8; 16],
        recipient: &[u8; 16],
        grant: &[u8; 16],
    ) -> rusqlite::Result<()> {
        db.conn().execute(
            "DELETE FROM share_relay_grants
             WHERE stream_id = ? AND recipient_identity_id = ? AND relay_grant_id = ?",
            params![&stream_id[..], &recipient[..], &grant[..]],
        )?;
        Ok(())
    }
}
