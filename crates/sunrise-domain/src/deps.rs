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
pub const fn blocker_is_open(state: &TaskState, deleted: bool) -> bool {
    !deleted && !matches!(state, TaskState::Done | TaskState::Cancelled)
}

/// Widen a user-set state with the derived `blocked` case.
///
/// `open_blockers` is the count of still-open entries in the task's
/// `blocked_by` (see [`blocker_is_open`]; unknown blockers count as open).
/// A `done`/`cancelled` task is never reported blocked — its own state wins.
#[must_use]
pub const fn effective_state(state: &TaskState, open_blockers: u32) -> EffectiveTaskState {
    // An unknown state reads as its fallback, `todo` (ADR-0045 §6).
    match state {
        TaskState::Done => EffectiveTaskState::Done,
        TaskState::Cancelled => EffectiveTaskState::Cancelled,
        TaskState::Todo | TaskState::Unknown(_) if open_blockers == 0 => EffectiveTaskState::Todo,
        TaskState::InProgress if open_blockers == 0 => EffectiveTaskState::InProgress,
        TaskState::Todo | TaskState::InProgress | TaskState::Unknown(_) => {
            EffectiveTaskState::Blocked
        }
    }
}

/// True when a task can be worked on right now: open, and nothing blocking it.
#[must_use]
pub const fn is_actionable(state: &TaskState, open_blockers: u32) -> bool {
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

/// A `blocked_by` edge the cycle rule sets aside (#333,
/// ADR-0065 §5): it stays in the task's merged set, and no read of the
/// dependency graph sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressedEdge<S> {
    /// The task whose `blocked_by` names the edge.
    pub task: EntityRef,
    /// The blocker it names.
    pub blocker: EntityRef,
    /// The stamp the rule ordered it by.
    pub stamp: S,
    /// The cycle it would close, from `task` back to `task`: `task`, then
    /// `blocker`, then the kept edges leading back. A self-block is
    /// `[task, task]`.
    pub cycle: Vec<EntityRef>,
}

/// The acyclic graph the merged `blocked_by` sets read as, and the edges set
/// aside to make it so (#333).
///
/// Concurrent edits on two devices can each pass the local check
/// ([`DependencyGraph::would_cycle`]) and still merge into a cycle, since
/// `blocked_by` is an add-wins set. The rule that breaks it is **oldest edge
/// first**: the edges are taken in ascending `stamp` order (ties by task, then
/// blocker), and an edge that would close a cycle among the edges already
/// kept is suppressed. So of `A→B` and a concurrent `B→A`, the newer one
/// reads as absent, and an edge that closes a longer loop is judged against
/// the kept graph alone.
///
/// The result is a pure function of the set of `(task, blocker, stamp)`
/// triples, whatever order they are given in, so every replica that holds
/// the same merged sets reads the same graph without exchanging anything.
/// Nothing is written: the edge stays in the set, and removing another edge
/// of its cycle brings it back.
///
/// The caller gives each edge the stamp of its oldest surviving add, which
/// is a fact of the merged set, not of this replica. An edge given twice
/// keeps its least stamp.
#[must_use]
pub fn resolve_cycles<S, I>(edges: I) -> (DependencyGraph, Vec<SuppressedEdge<S>>)
where
    S: Ord + Clone,
    I: IntoIterator<Item = (EntityRef, EntityRef, S)>,
{
    let mut ordered: Vec<(S, EntityRef, EntityRef)> =
        edges.into_iter().map(|(t, b, s)| (s, t, b)).collect();
    ordered.sort();
    // One edge per (task, blocker): the oldest, which sorts first.
    let mut seen: BTreeSet<(EntityRef, EntityRef)> = BTreeSet::new();
    ordered.retain(|(_, t, b)| seen.insert((*t, *b)));
    let mut kept = DependencyGraph::new();
    let mut suppressed = Vec::new();
    for (stamp, task, blocker) in ordered {
        if let Some(back) = kept.path(blocker, task) {
            let mut cycle = Vec::with_capacity(back.len() + 1);
            cycle.push(task);
            cycle.extend(back);
            suppressed.push(SuppressedEdge {
                task,
                blocker,
                stamp,
                cycle,
            });
        } else {
            kept.add_edge(task, blocker);
        }
    }
    (kept, suppressed)
}

impl DependencyGraph {
    /// A shortest path of `blocked_by` edges from `from` to `to`, both
    /// included, or `None` when `to` is not reachable. `from == to` is the
    /// path `[from]`.
    fn path(&self, from: EntityRef, to: EntityRef) -> Option<Vec<EntityRef>> {
        let mut came_from: BTreeMap<EntityRef, EntityRef> = BTreeMap::new();
        let mut queue = std::collections::VecDeque::from([from]);
        let mut seen = BTreeSet::from([from]);
        while let Some(node) = queue.pop_front() {
            if node == to {
                let mut out = vec![to];
                let mut at = to;
                while let Some(prev) = came_from.get(&at) {
                    out.push(*prev);
                    at = *prev;
                }
                out.reverse();
                return Some(out);
            }
            for next in self.blockers_of(&node) {
                if seen.insert(*next) {
                    came_from.insert(*next, node);
                    queue.push_back(*next);
                }
            }
        }
        None
    }

    /// Whether some chain of edges leads back to where it started.
    #[must_use]
    pub fn has_cycle(&self) -> bool {
        // Kahn's algorithm: a graph is acyclic exactly when repeatedly
        // removing nodes with no remaining blockers removes every node.
        let mut nodes: BTreeSet<EntityRef> = self.forward.keys().copied().collect();
        nodes.extend(self.reverse.keys().copied());
        let mut waiting: BTreeMap<EntityRef, usize> = nodes
            .iter()
            .map(|n| (*n, self.blockers_of(n).len()))
            .collect();
        let mut ready: Vec<EntityRef> = waiting
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut removed = 0usize;
        while let Some(node) = ready.pop() {
            removed += 1;
            for dependent in self.blocks_others(&node) {
                if let Some(n) = waiting.get_mut(dependent) {
                    *n -= 1;
                    if *n == 0 {
                        ready.push(*dependent);
                    }
                }
            }
        }
        removed != nodes.len()
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
        assert!(blocker_is_open(&TaskState::Todo, false));
        assert!(blocker_is_open(&TaskState::InProgress, false));
        assert!(!blocker_is_open(&TaskState::Done, false));
        assert!(!blocker_is_open(&TaskState::Cancelled, false));
        // A tombstoned blocker will never reach done; it must not block forever.
        assert!(!blocker_is_open(&TaskState::Todo, true));
    }

    #[test]
    fn an_unknown_state_reads_as_todo() {
        let unknown = TaskState::from_raw("archived");
        // Open, like `todo`: an unknown state never releases a dependent.
        assert!(blocker_is_open(&unknown, false));
        assert_eq!(effective_state(&unknown, 0), EffectiveTaskState::Todo);
        assert_eq!(effective_state(&unknown, 1), EffectiveTaskState::Blocked);
        assert!(is_actionable(&unknown, 0));
    }

    #[test]
    fn effective_state_reports_blocked_only_for_open_tasks() {
        assert_eq!(
            effective_state(&TaskState::Todo, 0),
            EffectiveTaskState::Todo
        );
        assert_eq!(
            effective_state(&TaskState::Todo, 1),
            EffectiveTaskState::Blocked
        );
        assert_eq!(
            effective_state(&TaskState::InProgress, 2),
            EffectiveTaskState::Blocked
        );
        // A finished task is never "blocked", however many blockers it names.
        assert_eq!(
            effective_state(&TaskState::Done, 3),
            EffectiveTaskState::Done
        );
        assert_eq!(
            effective_state(&TaskState::Cancelled, 3),
            EffectiveTaskState::Cancelled
        );
        assert!(is_actionable(&TaskState::Todo, 0));
        assert!(!is_actionable(&TaskState::Todo, 1));
        assert!(!is_actionable(&TaskState::Done, 0));
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
                .filter(|(s, d)| blocker_is_open(s, *d))
                .count(),
        )
        .unwrap();
        assert_eq!(open, 1);
        assert_eq!(
            effective_state(&TaskState::Todo, open),
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

    #[test]
    fn of_two_concurrent_edges_closing_a_loop_the_newer_is_suppressed() {
        let (g, dropped) = resolve_cycles([(t(1), t(2), 5u32), (t(2), t(1), 7)]);
        assert_eq!(g.blockers_of(&t(1)), &BTreeSet::from([t(2)]));
        assert!(g.blockers_of(&t(2)).is_empty());
        assert_eq!(dropped.len(), 1);
        assert_eq!((dropped[0].task, dropped[0].blocker), (t(2), t(1)));
        assert_eq!(dropped[0].stamp, 7);
        assert_eq!(dropped[0].cycle, vec![t(2), t(1), t(2)]);
        assert!(!g.has_cycle());
        // Equal stamps break by task, then blocker.
        let (_, tie) = resolve_cycles([(t(2), t(1), 5u32), (t(1), t(2), 5)]);
        assert_eq!((tie[0].task, tie[0].blocker), (t(2), t(1)));
    }

    #[test]
    fn a_self_block_is_suppressed_and_an_acyclic_graph_is_kept_whole() {
        let (g, dropped) = resolve_cycles([(t(1), t(1), 1u32), (t(2), t(3), 2), (t(3), t(4), 3)]);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].cycle, vec![t(1), t(1)]);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn a_longer_loop_drops_only_its_newest_edge() {
        // 1→2 (old), 2→3, 3→1 (newest), and an unrelated 4→1 after it.
        let (g, dropped) = resolve_cycles([
            (t(3), t(1), 30u32),
            (t(1), t(2), 10),
            (t(2), t(3), 20),
            (t(4), t(1), 40),
        ]);
        assert_eq!(dropped.len(), 1);
        assert_eq!((dropped[0].task, dropped[0].blocker), (t(3), t(1)));
        assert_eq!(dropped[0].cycle, vec![t(3), t(1), t(2), t(3)]);
        assert_eq!(g.edge_count(), 3);
        assert!(!g.has_cycle());
    }

    #[test]
    fn an_edge_given_twice_keeps_its_oldest_stamp() {
        let (_, dropped) = resolve_cycles([(t(1), t(2), 9u32), (t(2), t(1), 5), (t(1), t(2), 3)]);
        assert_eq!((dropped[0].task, dropped[0].stamp), (t(2), 5));
    }

    #[test]
    fn has_cycle_sees_a_loop_and_nothing_else() {
        assert!(DependencyGraph::from_edges([(t(1), t(2)), (t(2), t(1))]).has_cycle());
        assert!(DependencyGraph::from_edges([(t(1), t(1))]).has_cycle());
        assert!(
            DependencyGraph::from_edges([(t(1), t(2)), (t(2), t(3)), (t(3), t(1))]).has_cycle()
        );
        assert!(!DependencyGraph::from_edges([(t(1), t(2)), (t(3), t(2))]).has_cycle());
        assert!(!DependencyGraph::from_edges([(t(1), t(2)), (t(2), t(3))]).has_cycle());
        assert!(!DependencyGraph::new().has_cycle());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 256,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// #333: any order of the same merged edges yields the same acyclic
        /// graph and the same suppressed edges, and only an edge that would
        /// close a cycle is ever suppressed.
        #[test]
        fn any_merge_order_resolves_to_the_same_dag(
            raw in proptest::collection::vec((0u8..6, 0u8..6, 0u32..40), 0..24),
            seed in proptest::prelude::any::<u64>(),
        ) {
            let edges: Vec<(EntityRef, EntityRef, u32)> =
                raw.iter().map(|(a, b, s)| (t(*a), t(*b), *s)).collect();
            let mut shuffled = edges.clone();
            // A deterministic shuffle from the seed (xorshift).
            let mut x = seed | 1;
            for i in (1..shuffled.len()).rev() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let j = usize::try_from(x % (i as u64 + 1)).unwrap_or(0);
                shuffled.swap(i, j);
            }
            let (g1, d1) = resolve_cycles(edges.clone());
            let (g2, d2) = resolve_cycles(shuffled);
            proptest::prop_assert_eq!(&g1, &g2);
            proptest::prop_assert_eq!(&d1, &d2);
            proptest::prop_assert!(!g1.has_cycle());
            for d in &d1 {
                // Putting the suppressed edge back alone would close its cycle.
                let mut with = g1.blockers_of(&d.task).clone();
                with.insert(d.blocker);
                proptest::prop_assert!(g1.would_cycle(d.task, &with));
            }
            // Every distinct edge is kept or suppressed, never both.
            let distinct: BTreeSet<(EntityRef, EntityRef)> =
                edges.iter().map(|(a, b, _)| (*a, *b)).collect();
            proptest::prop_assert_eq!(distinct.len(), g1.edge_count() + d1.len());
        }
    }
}
