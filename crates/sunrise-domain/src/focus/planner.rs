//! Candidate ranking and the graph frontier: what to work on next, and what
//! finishing it released.
//!
//! The only part of [`crate::focus`] that reads a [`DependencyGraph`], and the
//! only part that sorts. Both halves answer one question from opposite ends —
//! [`rank_focus_plan`] picks an actionable task off the frontier,
//! [`unblock_cascade`] recomputes the frontier once one is finished — so the
//! comparison keys and the frontier recomputation are the same subject.
//!
//! [`energy_rank`] lives here rather than with the calibration fold because
//! ranking is its primary reader. The fold borrows it for its per-energy
//! ordering; the dependency runs that way and must never be inverted.

use crate::common::Energy;
use crate::deps::DependencyGraph;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use sunrise_id::EntityRef;

// ---------------------------------------------------------------------------
// Planner ranking
// ---------------------------------------------------------------------------

/// How well a task's `energy` facet matches the session's declared energy
/// budget. Lower is better; the derive order **is** the ranking order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnergyFit {
    /// The task's energy equals the budget (or no budget was declared, in
    /// which case energy carries no signal and every task ties here).
    Exact,
    /// The task has no energy facet: acceptable, but an exact match beats it.
    Unknown,
    /// The task needs *less* than the budget — doable, but it wastes a window.
    Under,
    /// The task needs *more* than the budget. Ranked last: proposing deep work
    /// to a depleted user is how a planner loses trust.
    Over,
}

/// Rank of an [`Energy`] for comparison purposes.
pub(super) const fn energy_rank(e: Energy) -> u8 {
    match e {
        Energy::Low => 0,
        Energy::Med => 1,
        Energy::High => 2,
    }
}

/// Score a task's energy facet against the session's energy budget.
#[must_use]
pub fn energy_fit(task: Option<Energy>, budget: Option<Energy>) -> EnergyFit {
    let Some(budget) = budget else {
        return EnergyFit::Exact;
    };
    match task {
        None => EnergyFit::Unknown,
        Some(t) => match energy_rank(t).cmp(&energy_rank(budget)) {
            std::cmp::Ordering::Equal => EnergyFit::Exact,
            std::cmp::Ordering::Less => EnergyFit::Under,
            std::cmp::Ordering::Greater => EnergyFit::Over,
        },
    }
}

/// One task offered to the planner, reduced to the facts the ranking uses.
///
/// Built by the caller from whatever it has — the storage projection, a batch
/// of decoded ops, or a literal in a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCandidate {
    /// Task id.
    pub task: EntityRef,
    /// Task's energy facet.
    pub energy: Option<Energy>,
    /// **Leverage**: how many still-open tasks finishing this one releases —
    /// the derived `blocks_others` cardinality.
    pub unblocks: u32,
    /// How many of the task's blockers are still open. Non-zero means the task
    /// is blocked and [`rank_focus_plan`] drops it.
    pub open_blockers: u32,
    /// Priority 1..=5 (1 highest); `None` sorts after any set priority.
    pub priority: Option<u8>,
    /// Deadline, ms since epoch.
    pub due_at_ms: Option<u64>,
    /// Intended start, ms since epoch.
    pub scheduled_at_ms: Option<u64>,
    /// Estimate in seconds, used to size the proposed session.
    pub estimated_duration_s: Option<u64>,
}

/// A ranked planner row: the candidate plus the two facts that put it where it
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRanked {
    /// The candidate.
    pub candidate: PlanCandidate,
    /// Its energy fit against the session budget.
    pub fit: EnergyFit,
}

/// Rank candidates for the Focus Planner — the three stacked criteria from
/// `docs/08-features/focus-mode.md` §Focus Planner:
///
/// 1. **Actionable only.** Anything with an open blocker is dropped outright,
///    so the planner is never a dead end.
/// 2. **Energy-matched.** [`EnergyFit`] is the outer key, so high-energy work
///    lands in high-energy windows instead of being proposed to a depleted
///    user just because it unblocks a lot. With no declared budget every task
///    ties at [`EnergyFit::Exact`] and this key vanishes.
/// 3. **Ranked by leverage**, then `due_at`, `priority`, `scheduled_at` —
///    finally by id, so the result is deterministic for equal work.
#[must_use]
pub fn rank_focus_plan(candidates: Vec<PlanCandidate>, budget: Option<Energy>) -> Vec<PlanRanked> {
    let mut out: Vec<PlanRanked> = candidates
        .into_iter()
        .filter(|c| c.open_blockers == 0)
        .map(|c| PlanRanked {
            fit: energy_fit(c.energy, budget),
            candidate: c,
        })
        .collect();
    out.sort_by(|a, b| {
        a.fit
            .cmp(&b.fit)
            // Leverage: more unblocked work first.
            .then_with(|| b.candidate.unblocks.cmp(&a.candidate.unblocks))
            // Earliest deadline; no deadline sorts last.
            .then_with(|| opt_first(a.candidate.due_at_ms, b.candidate.due_at_ms))
            // Priority 1 is highest; unset sorts last.
            .then_with(|| opt_first(a.candidate.priority, b.candidate.priority))
            .then_with(|| opt_first(a.candidate.scheduled_at_ms, b.candidate.scheduled_at_ms))
            .then_with(|| a.candidate.task.cmp(&b.candidate.task))
    });
    out
}

/// Order two optionals ascending with `None` **last** (`Option`'s own `Ord`
/// puts `None` first, which would rank a task with no deadline ahead of one due
/// today).
fn opt_first<T: Ord>(a: Option<T>, b: Option<T>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// Unblock cascade
// ---------------------------------------------------------------------------

/// What completing a task released — the concrete payoff shown mid-session
/// (`docs/08-features/focus-mode.md` §Unblock cascade).
///
/// Informational and opt-out by construction: it names what moved, it keeps no
/// score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnblockCascade {
    /// The task that was just completed.
    pub completed: EntityRef,
    /// Dependents that became actionable because of it.
    pub released: Vec<EntityRef>,
    /// Dependents still waiting on something else.
    pub still_blocked: Vec<EntityRef>,
}

impl UnblockCascade {
    /// Whether anything at all moved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.released.is_empty() && self.still_blocked.is_empty()
    }
}

/// Recompute the graph frontier around a freshly-completed task.
///
/// `open_after` is each dependent's open-blocker count **as of after** the
/// completion — the caller recomputes it the same way every other read does
/// (`blocked` is never stored), and a dependent missing from the map counts as
/// having no remaining blockers.
#[must_use]
pub fn unblock_cascade(
    graph: &DependencyGraph,
    completed: EntityRef,
    open_after: &BTreeMap<EntityRef, u32>,
) -> UnblockCascade {
    let mut released = Vec::new();
    let mut still_blocked = Vec::new();
    for dep in graph.blocks_others(&completed) {
        if open_after.get(dep).copied().unwrap_or(0) == 0 {
            released.push(*dep);
        } else {
            still_blocked.push(*dep);
        }
    }
    UnblockCascade {
        completed,
        released,
        still_blocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn tsk(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [b; 16])
    }

    // ---- planner ----

    fn cand(task: u8, unblocks: u32, energy: Option<Energy>) -> PlanCandidate {
        PlanCandidate {
            task: tsk(task),
            energy,
            unblocks,
            open_blockers: 0,
            priority: None,
            due_at_ms: None,
            scheduled_at_ms: None,
            estimated_duration_s: None,
        }
    }

    #[test]
    fn planner_prefers_high_leverage_at_matching_energy() {
        // Two tasks the user can do right now, both a match for the declared
        // budget. The one that releases more downstream work wins.
        let ranked = rank_focus_plan(
            vec![
                cand(1, 0, Some(Energy::High)),
                cand(2, 3, Some(Energy::High)),
            ],
            Some(Energy::High),
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].candidate.task, tsk(2));
        assert_eq!(ranked[0].candidate.unblocks, 3);
        assert_eq!(ranked[0].fit, EnergyFit::Exact);
        assert_eq!(ranked[1].candidate.task, tsk(1));
    }

    #[test]
    fn planner_never_proposes_a_blocked_task() {
        let mut blocked = cand(1, 99, Some(Energy::High));
        blocked.open_blockers = 1;
        let ranked = rank_focus_plan(vec![blocked, cand(2, 0, Some(Energy::High))], None);
        assert_eq!(ranked.len(), 1, "the blocked task is dropped, not demoted");
        assert_eq!(ranked[0].candidate.task, tsk(2));
    }

    #[test]
    fn energy_budget_outranks_leverage() {
        // A depleted user is not handed deep work just because it unblocks a
        // lot: the low-energy match comes first.
        let ranked = rank_focus_plan(
            vec![
                cand(1, 9, Some(Energy::High)),
                cand(2, 0, Some(Energy::Low)),
            ],
            Some(Energy::Low),
        );
        assert_eq!(ranked[0].candidate.task, tsk(2));
        assert_eq!(ranked[0].fit, EnergyFit::Exact);
        assert_eq!(ranked[1].fit, EnergyFit::Over);
    }

    #[test]
    fn energy_fit_ordering() {
        assert_eq!(
            energy_fit(Some(Energy::High), Some(Energy::High)),
            EnergyFit::Exact
        );
        assert_eq!(energy_fit(None, Some(Energy::High)), EnergyFit::Unknown);
        assert_eq!(
            energy_fit(Some(Energy::Low), Some(Energy::High)),
            EnergyFit::Under
        );
        assert_eq!(
            energy_fit(Some(Energy::High), Some(Energy::Low)),
            EnergyFit::Over
        );
        // No declared budget: energy carries no signal, everything ties.
        assert_eq!(energy_fit(Some(Energy::High), None), EnergyFit::Exact);
        assert_eq!(energy_fit(None, None), EnergyFit::Exact);
        assert!(EnergyFit::Exact < EnergyFit::Unknown);
        assert!(EnergyFit::Unknown < EnergyFit::Under);
        assert!(EnergyFit::Under < EnergyFit::Over);
    }

    #[test]
    fn ties_break_on_due_then_priority_then_id() {
        let mut a = cand(1, 1, None);
        a.due_at_ms = Some(2_000);
        let mut b = cand(2, 1, None);
        b.due_at_ms = Some(1_000);
        let mut c = cand(3, 1, None);
        c.due_at_ms = None; // no deadline sorts last, not first
        let ranked = rank_focus_plan(vec![a, c, b], None);
        assert_eq!(
            ranked.iter().map(|r| r.candidate.task).collect::<Vec<_>>(),
            vec![tsk(2), tsk(1), tsk(3)]
        );
    }

    // ---- unblock cascade ----

    #[test]
    fn cascade_separates_released_from_still_blocked() {
        // `deploy` and `qa` both wait on `build`; `qa` also waits on `docs`.
        let g = DependencyGraph::from_edges([(tsk(2), tsk(1)), (tsk(3), tsk(1)), (tsk(3), tsk(4))]);
        let open_after = BTreeMap::from([(tsk(2), 0), (tsk(3), 1)]);
        let c = unblock_cascade(&g, tsk(1), &open_after);
        assert_eq!(c.completed, tsk(1));
        assert_eq!(c.released, vec![tsk(2)]);
        assert_eq!(c.still_blocked, vec![tsk(3)]);
        assert!(!c.is_empty());
    }

    #[test]
    fn cascade_of_a_leaf_is_empty() {
        let g = DependencyGraph::new();
        let c = unblock_cascade(&g, tsk(1), &BTreeMap::new());
        assert!(c.is_empty());
    }
}
