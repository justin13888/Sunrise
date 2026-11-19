//! A Task bound to one occurrence of a recurring Block
//! (`docs/02-domain/time-blocks.md` §Recurring blocks, #1162).
//!
//! A recurring Block binds a Task to one occurrence, not to the series: the
//! binding carries the occurrence key. The Block holds those bindings in its
//! `occurrence_tasks` OR-set, beside `tasks`, whose bindings name every
//! occurrence. Pure functions of the Block, with no clock and no storage.

use crate::routine_gen::{format_key, parse_occurrence_key};
use crate::time::SunriseTime;
use jiff::civil::DateTime;
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

    /// Re-key the occurrence bindings for an anchor moved from `from` to the
    /// block's `starts_at`: each key moves by the civil distance the anchor
    /// moved, so a binding keeps naming the occurrence it named when an edit
    /// moves the series, whole or as a split's continuation (an occurrence
    /// key is the intended civil start). A key, or an anchor, that is not a
    /// civil time is kept as it is.
    pub fn shift_bindings(&mut self, from: &SunriseTime) {
        let (Some(from), Some(to)) = (civil_of(from), civil_of(&self.starts_at)) else {
            return;
        };
        let moved = to.duration_since(from);
        if moved.is_zero() {
            return;
        }
        self.occurrence_tasks = std::mem::take(&mut self.occurrence_tasks)
            .into_iter()
            .map(|b| {
                let shifted = parse_occurrence_key(b.key())
                    .and_then(|at| at.checked_add(moved).ok())
                    .map(|at| format_key(at.date(), at.time()));
                match shifted {
                    Some(key) => OccurrenceBinding::new(key, b.task()),
                    None => b,
                }
            })
            .collect();
    }
}

/// The civil time of a `floating` or `zoned` bound, the kinds a series
/// anchor takes.
fn civil_of(t: &SunriseTime) -> Option<DateTime> {
    match t {
        SunriseTime::Floating { civil } | SunriseTime::Zoned { civil, .. } => Some(*civil),
        _ => None,
    }
}
