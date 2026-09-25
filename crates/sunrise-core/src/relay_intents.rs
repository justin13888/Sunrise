//! The `relay_revocation_intents` queue: "tell the relay this device is gone".
//!
//! Revoking a device is two facts in two places. In the vault it is a
//! `device_revoke` op sealed under the vault-meta Stream key, which the relay
//! holds no key for and so cannot read. At the relay it is a flag on its own
//! `devices` row, set through `DELETE /api/v1/devices/by-vault-id/{id}`.
//!
//! The queue is what connects them, and it is a table rather than a direct call
//! because `revoke_device` must work offline — a device that is gone is the
//! whole scenario. `crates/sunrise-storage/migrations/0020_relay_revocation_intents.sql`
//! carries the rest of the reasoning; the drain is
//! [`crate::sync_driver`]'s `drain_relay_revocations`.
//!
//! These four methods moved off `Core` rather than being written here: they are
//! the symbols that share one table, which is the cohesion boundary the
//! file-size gate names, and `core.rs` was at its threshold.
//!
//! # A row is owed only while the register agrees
//!
//! A row here is not, on its own, a revocation the relay is owed. The register
//! (`device_revocations`) is derived: `Engine::refold_device_revocations`
//! rebuilds it from the whole ledger on every applied `device_revoke`, and a
//! revocation it believed can **unwind** when its author turns out to have been
//! revoked first (ADR-0041). The fold does not touch this table, and should not
//! — so an intent can outlive the register row it was queued beside.
//!
//! [`OWED_RELAY_REVOCATIONS_SQL`] is therefore the one definition of what is
//! owed: an intent whose device the register **currently** calls revoked. The
//! drain sends only those, and [`Core::relay_revocation_pending`] answers from
//! the same set, so neither can tell the relay to cut a device every replica
//! shows as current (#257). The row itself stays: when a later fold revokes the
//! device again — the discount can restore an unwound revocation — the intent
//! is owed again, which deleting it on the unwind would have lost, leaving the
//! relay behind the register (#160's failure).
//!
//! What no row here can do is take back a revocation the relay has already
//! been told. The drain deletes the row on the relay's answer, and the relay
//! has no inverse of `DELETE`, so an unwind after that point leaves the relay
//! refusing the device. That matches the read bound, which an unwind never
//! releases either: an unwound device is not back in at either bound.
//! `docs/03-crypto/key-rotation.md` §Revocation states it for a reader.

use crate::core::{Core, CoreError};

/// The intents the relay is owed, oldest first: rows of
/// `relay_revocation_intents` whose device the register currently calls
/// revoked. See the module doc for why presence alone is not enough.
pub(crate) const OWED_RELAY_REVOCATIONS_SQL: &str = "SELECT i.device_id
     FROM relay_revocation_intents i
     WHERE EXISTS (SELECT 1 FROM device_revocations r WHERE r.device_id = i.device_id)
     ORDER BY i.created_at_ms ASC, i.device_id ASC";

impl Core {
    /// Devices this vault has revoked and not yet told the relay about.
    ///
    /// The 16-byte ids are the vault's own, which is the only name a revoking
    /// device holds for a peer and the one
    /// `DELETE /api/v1/devices/by-vault-id/{id}` takes.
    ///
    /// Only the intents the register still agrees with: an intent whose
    /// register row a re-fold unwound stays queued and is not returned until
    /// the register revokes that device again. See the module doc.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn pending_relay_revocations(&self) -> Result<Vec<[u8; 16]>, CoreError> {
        let db = self.db();
        let mut stmt = db.conn().prepare(OWED_RELAY_REVOCATIONS_SQL)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect())
    }

    /// Whether a revocation of `device_id` is still owed to the relay.
    ///
    /// The two halves of a revocation are different guarantees and a user is
    /// entitled to know which they have: the vault half is committed the
    /// moment `RevokeDevice` returns, while the relay half is a queued intent
    /// that needs a session. A client that reported only the first would let
    /// someone with no network believe a stolen laptop had been cut off from
    /// the server, which it has not been. See issue #160.
    ///
    /// Answered from the same set the drain sends, so "queued" here always
    /// means "will be sent": an intent the register no longer agrees with
    /// (#257) is not pending. That is also why a `RevokeDevice` the fold
    /// discarded can never read as pending — its target is current, whatever
    /// an earlier revocation left in the table.
    ///
    /// # Errors
    /// Storage failures.
    pub fn relay_revocation_pending(&self, device_id: &[u8; 16]) -> Result<bool, CoreError> {
        Ok(self.pending_relay_revocations()?.contains(device_id))
    }

    /// Forget an intent the relay has answered.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn clear_relay_revocation(&self, device_id: [u8; 16]) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "DELETE FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
        )?;
        Ok(())
    }

    /// Record that an attempt was made and refused, so a relay that keeps
    /// saying no is visible rather than retried in silence.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn note_relay_revocation_attempt(
        &self,
        device_id: [u8; 16],
        now_ms: u64,
    ) -> Result<u64, CoreError> {
        let db = self.db();
        db.conn().execute(
            "UPDATE relay_revocation_intents
             SET attempts = attempts + 1, last_attempt_ms = ?
             WHERE device_id = ?",
            rusqlite::params![i64::try_from(now_ms).unwrap_or(i64::MAX), &device_id[..]],
        )?;
        let attempts: i64 = db.conn().query_row(
            "SELECT attempts FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(attempts).unwrap_or(0))
    }

    /// Queue a relay revocation without going through `Command::RevokeDevice`.
    ///
    /// Test-only. The command refuses a device this vault has never admitted,
    /// which is the right rule and makes the driver's own tests — which have
    /// no second device to admit — unable to reach the queue at all.
    ///
    /// Writes the register row beside the intent, because an intent is owed
    /// only while the register calls its device revoked (module doc). The row
    /// is a placeholder, not a fold's output: nothing here reads its fields.
    #[cfg(test)]
    pub(crate) fn queue_relay_revocation_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "INSERT OR IGNORE INTO relay_revocation_intents (device_id, created_at_ms)
             VALUES (?, ?)",
            rusqlite::params![&device_id[..], 1_i64],
        )?;
        db.conn().execute(
            "INSERT OR IGNORE INTO device_revocations
             (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
             VALUES (?, 1, 0, X'', 'Lost', 1)",
            rusqlite::params![&device_id[..]],
        )?;
        Ok(())
    }

    /// Drop `device_id`'s register row and nothing else: what a re-fold
    /// unwinding that revocation leaves behind. Test-only.
    #[cfg(test)]
    pub(crate) fn unwind_register_row_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "DELETE FROM device_revocations WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
        )?;
        Ok(())
    }

    /// Whether an intent row exists for `device_id`, owed or not. Test-only.
    #[cfg(test)]
    pub(crate) fn relay_intent_row_exists_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<bool, CoreError> {
        let db = self.db();
        let n: i64 = db.conn().query_row(
            "SELECT count(*) FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// How many times the relay has refused this revocation. Test-only.
    #[cfg(test)]
    pub(crate) fn relay_revocation_attempts_for_test(
        &self,
        device_id: [u8; 16],
    ) -> Result<i64, CoreError> {
        let db = self.db();
        Ok(db.conn().query_row(
            "SELECT attempts FROM relay_revocation_intents WHERE device_id = ?",
            rusqlite::params![&device_id[..]],
            |r| r.get(0),
        )?)
    }
}
