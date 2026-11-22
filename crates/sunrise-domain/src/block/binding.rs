//! A Task bound to one occurrence of a recurring Block
//! (`docs/02-domain/time-blocks.md` §Recurring blocks, #1162).
//!
//! A recurring Block binds a Task to one occurrence, not to the series: the
//! binding carries the occurrence key. The Block holds those bindings in its
//! `occurrence_tasks` OR-set, beside `tasks`, whose bindings name every
//! occurrence. Pure functions of the Block, with no clock and no storage.

use super::series::{civil_distance, shift_key};
use crate::time::SunriseTime;
use crate::BlockSeries;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;
use thiserror::Error;

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

    /// Re-key the occurrence bindings for an anchor moved from `from` to the
    /// block's `starts_at`: each key moves by the civil distance the anchor
    /// moved, so a binding keeps naming the occurrence it named when an edit
    /// moves the series, whole or as a split's continuation (an occurrence
    /// key is the intended civil start). A key, or an anchor, that is not a
    /// civil time is kept as it is.
    pub fn shift_bindings(&mut self, from: &SunriseTime) {
        let Some(moved) = civil_distance(from, &self.starts_at) else {
            return;
        };
        if moved.is_zero() {
            return;
        }
        self.occurrence_tasks = std::mem::take(&mut self.occurrence_tasks)
            .into_iter()
            .map(|b| match shift_key(b.key(), moved) {
                Some(key) => OccurrenceBinding::new(key, b.task()),
                None => b,
            })
            .collect();
    }

    /// Re-key the occurrence bindings for an edit that moved the anchor from
    /// series `was`'s to the block's `starts_at`, as
    /// [`Self::shift_bindings`] does, refusing a move that would leave a
    /// binding naming no occurrence, as [`crate::BlockSeries::follow_anchor`]
    /// refuses one that would strand an exception. A binding `was` did not
    /// produce moves with the rest and still names nothing; a block whose
    /// edit dropped its rule is only re-keyed.
    ///
    /// # Errors
    /// [`StrandedBinding`] when a binding to an occurrence `was` produced
    /// would name one the moved rule does not produce (a rule whose `BYDAY`
    /// holds the weekday while the anchor's date moves). The bindings are
    /// left as they were.
    pub fn follow_bindings(&mut self, was: &BlockSeries) -> Result<(), StrandedBinding> {
        let moved = civil_distance(&was.starts_at, &self.starts_at).filter(|m| !m.is_zero());
        if let (Some(moved), Some(now)) = (moved, self.series()) {
            for b in &self.occurrence_tasks {
                let follows = shift_key(b.key(), moved).is_some_and(|k| now.produces_key(&k));
                if was.produces_key(b.key()) && !follows {
                    return Err(StrandedBinding {
                        key: b.key().to_owned(),
                        task: b.task(),
                    });
                }
            }
        }
        self.shift_bindings(&was.starts_at);
        Ok(())
    }
}

/// Why an edit that moves a series' anchor cannot carry its occurrence
/// bindings along ([`crate::Block::follow_bindings`]).
#[derive(Debug, Error, PartialEq, Eq)]
#[error("the binding of {task} at {key} names no occurrence once the series moves")]
pub struct StrandedBinding {
    /// The key, before the move, of the binding that would name nothing.
    pub key: String,
    /// The bound Task.
    pub task: EntityRef,
}
