//! The account's key bundle at the core's seam (ADR-0069): absorbing one, and
//! the three questions the sync driver asks before it publishes one.
//!
//! The bundle is built, judged and absorbed by the engine
//! (`engine::key_bundle`); the transport legs are `key_bundle_sync`'s. This
//! file holds the lock discipline between them and the one piece of memory a
//! publish needs: the manifest this process last saw the relay hold, so an
//! unchanged key set costs nothing on the next session.

use super::{Core, CoreError};
use crate::engine::{KeyBundleApplied, KeyBundlePlan, PinnedKeyBundle};

impl Core {
    /// Absorb the account's key bundle: verify it under the identity in force,
    /// open it with this vault's `ID_D_priv`, and store every key it carries.
    ///
    /// What a device restored from the recovery code does once the relay has
    /// trimmed the identity-addressed `key_envelope` ops: the sync driver
    /// fetches the pinned bundle and calls this when a snapshot record it
    /// needs is sealed under a key it does not hold.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; [`crate::EngineError::Invalid`] for
    /// a bundle that is malformed, of another account, forged or does not
    /// open; [`crate::EngineError::Keychain`] on a vault that holds no
    /// `ID_D_priv`, which is every vault pairing admitted.
    pub fn apply_key_bundle(&self, bytes: &[u8]) -> Result<KeyBundleApplied, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let (outcome, events) = {
            let mut db = self.db.lock();
            self.engine.apply_key_bundle(&mut db, bytes)?
        };
        for ev in events {
            let _ = self.changes_tx.send(ev);
        }
        Ok(outcome)
    }

    /// The bundle this replica would publish, unless this process already saw
    /// the relay hold one with the same manifest.
    pub(crate) fn key_bundle_plan(&self) -> Result<Option<KeyBundlePlan>, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let plan = {
            let mut db = self.db.lock();
            self.engine.key_bundle_plan(&mut db)?
        };
        let seen = *self.published_key_bundle.lock();
        Ok(plan.filter(|p| seen != Some(p.digest)))
    }

    /// Remember that the relay holds a bundle covering the manifest `digest`
    /// names, so the next publish with the same keys asks nothing.
    pub(crate) fn note_key_bundle_current(&self, digest: [u8; 32]) {
        *self.published_key_bundle.lock() = Some(digest);
    }

    /// Seal `plan` into a key bundle signed by this device.
    pub(crate) fn seal_key_bundle(&self, plan: &KeyBundlePlan) -> Result<Vec<u8>, CoreError> {
        Ok(self.engine.seal_key_bundle(plan)?)
    }

    /// What this replica makes of the bundle the relay pins.
    pub(crate) fn judge_pinned_key_bundle(
        &self,
        bytes: &[u8],
    ) -> Result<PinnedKeyBundle, CoreError> {
        let db = self.db.lock();
        Ok(self.engine.judge_pinned_key_bundle(&db, bytes)?)
    }
}
