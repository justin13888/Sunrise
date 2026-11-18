//! A Task bound to one occurrence of a recurring Block
//! (`docs/02-domain/time-blocks.md` §Recurring blocks, #1162).
//!
//! A recurring Block binds a Task to one occurrence, not to the series: the
//! binding carries the occurrence key. The Block holds those bindings in its
//! `occurrence_tasks` OR-set, beside `tasks`, whose bindings name every
//! occurrence. Pure functions of the Block, with no clock and no storage.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// A Task bound to one occurrence of a recurring Block: the occurrence key,
/// then the Task.
///
/// On the wire it is the two-element array `[occurrence-key, entity-ref]`,
/// with no unknown fields: it is an element of the `occurrence_tasks` OR-set,
/// whose identity is its canonical encoding, so the same binding made on two
/// devices is one element.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OccurrenceBinding(pub String, pub EntityRef);

impl OccurrenceBinding {
    /// The binding of `task` to occurrence `key`.
    #[must_use]
    pub fn new(key: impl Into<String>, task: EntityRef) -> Self {
        Self(key.into(), task)
    }

    /// The occurrence key the binding names.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.0
    }

    /// The bound Task.
    #[must_use]
    pub const fn task(&self) -> EntityRef {
        self.1
    }
}

impl crate::Block {
    /// The Tasks bound to occurrence `key`: those bound to the whole block,
    /// and those bound to that occurrence alone.
    #[must_use]
    pub fn tasks_at(&self, key: &str) -> BTreeSet<EntityRef> {
        let mut tasks = self.tasks.clone();
        tasks.extend(
            self.occurrence_tasks
                .iter()
                .filter(|b| b.key() == key)
                .map(OccurrenceBinding::task),
        );
        tasks
    }

    /// Take the occurrence bindings at or after `key` off the block, for a
    /// split *this and future* at `key`: the ones before stay, and the ones
    /// returned go to the block that continues the series. An occurrence key
    /// is the civil start `YYYY-MM-DDTHH:MM`, so its text order is its time
    /// order, as [`crate::BlockSeries::split_at`] divides the exceptions.
    pub fn take_bindings_from(&mut self, key: &str) -> BTreeSet<OccurrenceBinding> {
        let (from, before) = std::mem::take(&mut self.occurrence_tasks)
            .into_iter()
            .partition(|b| b.key() >= key);
        self.occurrence_tasks = before;
        from
    }
}
