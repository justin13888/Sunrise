//! The catalog's reads (`crate::alerts`): what a deadline reminder, a
//! security alert and the sync-stalled notice are planned from.

use super::{Core, CoreError};
use crate::alerts::{DeadlineCandidate, SecurityEvent};

impl Core {
    /// Every open task with a hard deadline (`docs/08-features/notifications.md`
    /// #2). Timed and dated alike: the client layer resolves each in the
    /// reader's zone.
    ///
    /// # Errors
    ///
    /// [`CoreError::Closed`] after close, or the failed read.
    pub fn deadline_candidates(&self) -> Result<Vec<DeadlineCandidate>, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok(self.engine.deadline_candidates(&db)?)
    }

    /// The security events this replica holds from after this device joined
    /// (#13–#15): devices joining, devices revoked, rotations that retired
    /// the recovery code. Derived from the rows each op left, so it is the
    /// same on every read and survives a restart.
    ///
    /// # Errors
    ///
    /// [`CoreError::Closed`] after close, or the failed read.
    pub fn security_events(&self) -> Result<Vec<SecurityEvent>, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok(self.engine.security_events(&db)?)
    }

    /// When the oldest op the relay has not acknowledged was queued, epoch
    /// ms, or `None` when nothing waits (#12).
    ///
    /// # Errors
    ///
    /// [`CoreError::Closed`] after close, or the failed read.
    pub fn oldest_unacked_ms(&self) -> Result<Option<u64>, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok(self.engine.oldest_unacked_ms(&db)?)
    }
}
