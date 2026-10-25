//! The account's identity chain at the core's seam (ADR-0070): seeding a
//! vault restored from the recovery code, and what the sync driver publishes.

use super::{Core, CoreError};
use crate::engine::ChainSeeded;

impl Core {
    /// Store the account's identity chain record in this vault and walk this
    /// device's identity forward to the one it ends at.
    ///
    /// What a vault restored from the recovery code calls before anything
    /// else reads the relay: `sunrise_client_core::recovery::rejoin_account`
    /// hands in the record the recovery blob's route served beside the blob.
    /// A code made before a rotation that carried it opens the identity it
    /// was made under, and the walk opens each rotation's carry share in turn,
    /// so the vault comes up under the identity in force and certified under
    /// it. A record that names another account, or holds a link that does not
    /// verify, is refused and nothing is written.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; [`crate::EngineError::Invalid`] for
    /// a record that is malformed, of another account, or does not verify.
    pub fn seed_identity_chain(&self, bytes: &[u8]) -> Result<ChainSeeded, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let mut db = self.db.lock();
        Ok(self.engine.seed_identity_chain(&mut db, bytes)?)
    }

    /// The identity chain record this replica would publish, unless this
    /// process already saw the relay hold the same bytes.
    pub(crate) fn identity_chain_plan(&self) -> Result<Option<Vec<u8>>, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let plan = {
            let db = self.db.lock();
            self.engine.identity_chain_plan(&db)?
        };
        let seen = *self.published_identity_chain.lock();
        Ok(plan.filter(|p| seen != Some(*blake3::hash(p).as_bytes())))
    }

    /// Whether this vault should take the account's key bundle before it
    /// replays anything: it was restored from a recovery code in this process,
    /// holds the identity's unwrapping key, and has applied no other device's
    /// op yet.
    ///
    /// Why before (ADR-0070 §4): the vault minted base epochs of its own when
    /// it opened, so an op of the account's sealed under the same epoch with a
    /// key it lacks is refused rather than parked. After a rotation the
    /// identity copies of those keys are sealed to an identity this vault no
    /// longer signs under, and the bundle, sealed to the identity in force,
    /// is where they are.
    pub(crate) fn wants_key_bundle_before_replay(&self) -> bool {
        if !self.restored_from_code || !self.holds_identity_key() {
            return false;
        }
        let me = self.engine.keychain().device_id();
        let db = self.db.lock();
        db.conn()
            .query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM ops WHERE device_id != ?)",
                [&me[..]],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// Whether this vault, restored from a recovery code in this process and
    /// holding the identity's unwrapping key, has ops parked for a key it
    /// does not hold.
    pub(crate) fn wants_key_bundle_for_parked_ops(&self) -> bool {
        if !self.restored_from_code || !self.holds_identity_key() {
            return false;
        }
        let db = self.db.lock();
        db.conn()
            .query_row("SELECT EXISTS (SELECT 1 FROM deferred_ops)", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap_or(false)
    }

    /// Remember that the relay holds the record whose digest is `digest`.
    pub(crate) fn note_identity_chain_current(&self, digest: [u8; 32]) {
        *self.published_identity_chain.lock() = Some(digest);
    }
}
