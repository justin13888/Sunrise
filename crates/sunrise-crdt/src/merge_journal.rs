//! Merge journal entries — record losing/winning ops for review.
//!
//! Per `spec/05-sync/conflict-resolution.md`. Each scalar LWW resolution
//! and each task-move resolution writes a journal entry; UI exposes the
//! recent entries in the review surface.

use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// One merge-journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeEvent {
    /// Stable id (ULID) for the journal record itself.
    pub journal_id: [u8; 16],
    /// Wall-clock at the merge.
    pub created_at_ms: u64,
    /// Entity affected.
    pub entity: EntityRef,
    /// Field path (dotted; e.g., `"task.title"`).
    pub field: String,
    /// `op_id` of the losing op.
    pub losing_op_id: [u8; 16],
    /// `op_id` of the winning op.
    pub winning_op_id: [u8; 16],
    /// Short human summary.
    pub summary: String,
}

/// Append-only merge journal; in-memory.
#[derive(Debug, Default)]
pub struct MergeJournal {
    events: Vec<MergeEvent>,
}

impl MergeJournal {
    /// Empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one event.
    pub fn push(&mut self, event: MergeEvent) {
        self.events.push(event);
    }

    /// All events in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &MergeEvent> + '_ {
        self.events.iter()
    }

    /// Count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::{EntityKind, EntityRef};

    #[test]
    fn push_and_iter() {
        let mut j = MergeJournal::new();
        let e = MergeEvent {
            journal_id: [1u8; 16],
            created_at_ms: 1,
            entity: EntityRef::new(EntityKind::Task, [0u8; 16]),
            field: "task.title".into(),
            losing_op_id: [2u8; 16],
            winning_op_id: [3u8; 16],
            summary: "LWW".into(),
        };
        j.push(e.clone());
        assert_eq!(j.len(), 1);
        let collected: Vec<_> = j.iter().collect();
        assert_eq!(collected[0], &e);
    }
}
