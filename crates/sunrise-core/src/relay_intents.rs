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

use crate::core::{Core, CoreError};

impl Core {
    /// Devices this vault has revoked and not yet told the relay about.
    ///
    /// The 16-byte ids are the vault's own, which is the only name a revoking
    /// device holds for a peer and the one
    /// `DELETE /api/v1/devices/by-vault-id/{id}` takes.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn pending_relay_revocations(&self) -> Result<Vec<[u8; 16]>, CoreError> {
        let db = self.db();
        let mut stmt = db
            .conn()
            .prepare("SELECT device_id FROM relay_revocation_intents ORDER BY created_at_ms ASC")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect())
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
        Ok(())
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
