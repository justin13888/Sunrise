//! A stream's snapshot record (ADR-0068): reading and applying one, and
//! choosing which this replica publishes to the relay.

use super::{Core, CoreError};
#[cfg(doc)]
use crate::engine::{Engine, EngineError};

impl Core {
    /// The latest snapshot record of `stream_id` this vault wrote or applied,
    /// verbatim: what a new device bootstraps that stream from.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub fn stream_snapshot(&self, stream_id: [u8; 16]) -> Result<Option<Vec<u8>>, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok(self.engine.stream_snapshot(&db, &stream_id)?)
    }

    /// Apply a snapshot record another device wrote, and broadcast what it
    /// changed on `changes()`. See [`Engine::apply_snapshot`] for the checks
    /// and for what each outcome means.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; [`EngineError::Invalid`] for a
    /// record that is malformed, forged or of another history; storage
    /// failures.
    pub fn apply_stream_snapshot(
        &self,
        record: &[u8],
    ) -> Result<crate::engine::SnapshotApplied, CoreError> {
        self.ensure_open()?;
        let (outcome, events) = {
            let mut db = self.db.lock();
            self.engine.apply_snapshot(&mut db, record)?
        };
        for ev in events {
            let _ = self.changes_tx.send(ev);
        }
        Ok(outcome)
    }

    /// The snapshot records this replica should publish now (ADR-0068): each
    /// stream's latest record held here that this replica wrote, while it is
    /// that stream's compactor, unless this process already saw the relay pin
    /// it.
    ///
    /// # Errors
    /// [`CoreError::Closed`] after close; storage failures.
    pub(crate) fn snapshots_to_publish(
        &self,
    ) -> Result<Vec<crate::snapshot_sync::Publishable>, CoreError> {
        self.ensure_open()?;
        let policy = *self.compaction_policy.lock();
        let mut db = self.db.lock();
        let own = self.engine.own_stream_snapshots(&db)?;
        let published = self.published_snapshots.lock().clone();
        let mut out = Vec::new();
        for (stream, record) in own {
            let blob_id = crate::snapshot_sync::blob_id_of(&record);
            if published.get(&stream) == Some(&blob_id) {
                continue;
            }
            if self.engine.is_stream_compactor(&mut db, &stream, &policy)? {
                out.push(crate::snapshot_sync::Publishable {
                    stream,
                    blob_id,
                    record,
                });
            }
        }
        Ok(out)
    }

    /// Remember that the relay pins `blob_id` as `stream`'s snapshot record.
    pub(crate) fn note_snapshot_published(&self, stream: [u8; 16], blob_id: [u8; 16]) {
        self.published_snapshots.lock().insert(stream, blob_id);
    }
}
