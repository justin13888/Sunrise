//! Derived task-dependency state per `docs/02-domain/tasks.md`.
//!
//! `blocked` is **never persisted**: it is recomputed on every read (and on
//! every merge) from a Task's `blocked_by` set plus the *current* state of the
//! referenced blockers. `blocks_others` — the reverse direction — is likewise
//! derived and never rides the wire; it exists so a planner can answer "what
//! does finishing this unblock?" without a full scan.
//!
//! Everything in this module is a pure function over values the caller has
//! already read, so it unit-tests in isolation with no storage and no clock.
//!
//! # Blocking rule
//!
//! A Task is blocked while **any** blocker it names is still open. A blocker is
//! open unless it is `done`, `cancelled`, or tombstoned — and an *unknown*
//! blocker (an id this replica has not materialized yet, because the op that
//! creates it has not arrived) counts as open. That choice is what makes
//! out-of-order op arrival safe: a task that names a blocker it has not seen
//! shows as blocked, and flips to actionable by itself the moment the blocker
//! arrives already-done. Nothing has to be recomputed or repaired.

use crate::task::TaskState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

/// Empty set returned for a task with no recorded edges in either direction.
static NO_EDGES: BTreeSet<EntityRef> = BTreeSet::new();

/// The state a UI actually renders: the user-set [`TaskState`] widened with the
/// derived `blocked` case from `docs/02-domain/tasks.md`'s state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveTaskState {
    /// Pending, nothing in the way.
    Todo,
    /// Started.
    InProgress,
    /// Derived: at least one blocker is still open.
    Blocked,
    /// Done.
    Done,
    /// Cancelled.
    Cancelled,
}

/// Whether a blocker in this state still holds its dependents back.
///
/// `done` and `cancelled` release dependents (a cancelled prerequisite is one
/// nobody is waiting on any more); a tombstoned blocker releases them too,
/// since a deleted task will never reach `done`.
#[must_use]
pub const fn blocker_is_open(state: TaskState, deleted: bool) -> bool {
    !deleted && !matches!(state, TaskState::Done | TaskState::Cancelled)
}

/// Widen a user-set state with the derived `blocked` case.
///
/// `open_blockers` is the count of still-open entries in the task's
/// `blocked_by` (see [`blocker_is_open`]; unknown blockers count as open).
/// A `done`/`cancelled` task is never reported blocked — its own state wins.
#[must_use]
pub const fn effective_state(state: TaskState, open_blockers: u32) -> EffectiveTaskState {
    match state {
        TaskState::Done => EffectiveTaskState::Done,
        TaskState::Cancelled => EffectiveTaskState::Cancelled,
        TaskState::Todo if open_blockers == 0 => EffectiveTaskState::Todo,
        TaskState::InProgress if open_blockers == 0 => EffectiveTaskState::InProgress,
        TaskState::Todo | TaskState::InProgress => EffectiveTaskState::Blocked,
    }
}

/// True when a task can be worked on right now: open, and nothing blocking it.
#[must_use]
pub const fn is_actionable(state: TaskState, open_blockers: u32) -> bool {
    matches!(
        effective_state(state, open_blockers),
        EffectiveTaskState::Todo | EffectiveTaskState::InProgress
    )
}

/// Both directions of the `blocked_by` relation over a set of tasks.
///
/// Built from plain `(task, blocker)` edges, so it can be populated from the
/// storage index, from a batch of freshly-decoded ops, or from a literal in a
/// test. Edges naming tasks the caller never supplied are kept: an edge is a
/// fact about the graph even when one endpoint has not arrived yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyGraph {
    /// task → the tasks it waits on.
    forward: BTreeMap<EntityRef, BTreeSet<EntityRef>>,
    /// blocker → the tasks waiting on it.
    reverse: BTreeMap<EntityRef, BTreeSet<EntityRef>>,
}

impl DependencyGraph {
    /// Empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from `(task, blocker)` edges.
    #[must_use]
    pub fn from_edges<I>(edges: I) -> Self
    where
        I: IntoIterator<Item = (EntityRef, EntityRef)>,
    {
        let mut g = Self::default();
        for (task, blocker) in edges {
            g.add_edge(task, blocker);
        }
        g
    }

    /// Record that `task` waits on `blocker`.
    pub fn add_edge(&mut self, task: EntityRef, blocker: EntityRef) {
        self.forward.entry(task).or_default().insert(blocker);
        self.reverse.entry(blocker).or_default().insert(task);
    }

    /// The tasks `task` waits on (its `blocked_by`).
    #[must_use]
    pub fn blockers_of(&self, task: &EntityRef) -> &BTreeSet<EntityRef> {
        self.forward.get(task).unwrap_or(&NO_EDGES)
    }

    /// The tasks waiting on `task` — the derived `blocks_others` field.
    #[must_use]
    pub fn blocks_others(&self, task: &EntityRef) -> &BTreeSet<EntityRef> {
        self.reverse.get(task).unwrap_or(&NO_EDGES)
    }

    /// Total number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.forward.values().map(BTreeSet::len).sum()
    }

    /// Would replacing `task`'s blockers with `new_blockers` create a cycle
    /// (or a self-block)?
    ///
    /// This is the local submit-time check from `docs/02-domain/tasks.md`
    /// §Validation. It is deliberately *local*: concurrent edits on two devices
    /// can still form a cycle transiently, which the merge tie-breaker resolves.
    #[must_use]
    pub fn would_cycle(&self, task: EntityRef, new_blockers: &BTreeSet<EntityRef>) -> bool {
        if new_blockers.contains(&task) {
            return true;
        }
        // Walk the dependency direction from each proposed blocker. If `task`
        // is reachable, adding the edge closes a loop. `task`'s own existing
        // out-edges are irrelevant — they are the ones being replaced.
        let mut stack: Vec<EntityRef> = new_blockers.iter().copied().collect();
        let mut seen: BTreeSet<EntityRef> = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if node == task {
                return true;
            }
            if !seen.insert(node) {
                continue;
            }
            for next in self.blockers_of(&node) {
                if !seen.contains(next) {
                    stack.push(*next);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn t(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [b; 16])
    }

    #[test]
    fn done_and_cancelled_blockers_release_dependents() {
        assert!(blocker_is_open(TaskState::Todo, false));
        assert!(blocker_is_open(TaskState::InProgress, false));
        assert!(!blocker_is_open(TaskState::Done, false));
        assert!(!blocker_is_open(TaskState::Cancelled, false));
        // A tombstoned blocker will never reach done; it must not block forever.
        assert!(!blocker_is_open(TaskState::Todo, true));
    }

    #[test]
    fn effective_state_reports_blocked_only_for_open_tasks() {
        assert_eq!(
            effective_state(TaskState::Todo, 0),
            EffectiveTaskState::Todo
        );
        assert_eq!(
            effective_state(TaskState::Todo, 1),
            EffectiveTaskState::Blocked
        );
        assert_eq!(
            effective_state(TaskState::InProgress, 2),
            EffectiveTaskState::Blocked
        );
        // A finished task is never "blocked", however many blockers it names.
        assert_eq!(
            effective_state(TaskState::Done, 3),
            EffectiveTaskState::Done
        );
        assert_eq!(
            effective_state(TaskState::Cancelled, 3),
            EffectiveTaskState::Cancelled
        );
        assert!(is_actionable(TaskState::Todo, 0));
        assert!(!is_actionable(TaskState::Todo, 1));
        assert!(!is_actionable(TaskState::Done, 0));
    }

    #[test]
    fn any_open_blocker_blocks() {
        // Three blockers, two finished: still blocked. This is the "any", not
        // "every", reading of the rule — see the module docs.
        let states = [
            (TaskState::Done, false),
            (TaskState::Cancelled, false),
            (TaskState::Todo, false),
        ];
        let open = u32::try_from(
            states
                .iter()
                .filter(|(s, d)| blocker_is_open(*s, *d))
                .count(),
        )
        .unwrap();
        assert_eq!(open, 1);
        assert_eq!(
            effective_state(TaskState::Todo, open),
            EffectiveTaskState::Blocked
        );
    }

    #[test]
    fn graph_exposes_both_directions() {
        // b and c both wait on a.
        let g = DependencyGraph::from_edges([(t(2), t(1)), (t(3), t(1)), (t(3), t(2))]);
        assert_eq!(g.blockers_of(&t(3)), &BTreeSet::from([t(1), t(2)]));
        assert_eq!(g.blocks_others(&t(1)), &BTreeSet::from([t(2), t(3)]));
        assert_eq!(g.blocks_others(&t(3)), &NO_EDGES);
        assert_eq!(g.edge_count(), 3);
    }

    #[test]
    fn graph_keeps_edges_to_tasks_it_has_never_seen() {
        // The ordering hazard: an op naming a blocker whose create has not
        // arrived. The edge is still a fact and must survive.
        let g = DependencyGraph::from_edges([(t(9), t(0xEE))]);
        assert!(g.blockers_of(&t(9)).contains(&t(0xEE)));
        assert!(g.blocks_others(&t(0xEE)).contains(&t(9)));
    }

    #[test]
    fn would_cycle_rejects_self_block() {
        let g = DependencyGraph::new();
        assert!(g.would_cycle(t(1), &BTreeSet::from([t(1)])));
    }

    #[test]
    fn would_cycle_detects_transitive_loop() {
        // a waits on b, b waits on c. Making c wait on a closes the loop.
        let g = DependencyGraph::from_edges([(t(1), t(2)), (t(2), t(3))]);
        assert!(g.would_cycle(t(3), &BTreeSet::from([t(1)])));
        // An unrelated new blocker is fine.
        assert!(!g.would_cycle(t(3), &BTreeSet::from([t(9)])));
        // Adding a second edge that does not close a loop is fine.
        assert!(!g.would_cycle(t(1), &BTreeSet::from([t(2), t(3)])));
    }

    #[test]
    fn would_cycle_terminates_on_an_existing_loop() {
        // A cycle that merged in from two devices must not hang the check.
        let g = DependencyGraph::from_edges([(t(1), t(2)), (t(2), t(1))]);
        assert!(g.would_cycle(t(3), &BTreeSet::from([t(1), t(3)])));
        assert!(!g.would_cycle(t(4), &BTreeSet::from([t(1)])));
    }
}
