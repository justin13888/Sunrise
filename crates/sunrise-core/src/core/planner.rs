//! The interactive planner's seam (`docs/08-features/planner.md` §Core seam,
//! ADR-0048 §2): preview a gesture as a diff, remember it, and commit exactly
//! that diff or refuse.

use super::Core;
use crate::engine::PlanError;
use crate::events::DomainEvent;
use std::collections::VecDeque;
use sunrise_domain::planner::{PlanDiff, PlanDiffId, PlanIntent};

/// How many previews the core remembers; the least recently previewed is
/// forgotten first.
pub const PLAN_CACHE_SIZE: usize = 8;

/// What a commit wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCommitResult {
    /// How many tasks and blocks it moved.
    pub moved: u32,
}

/// The most recent previews, newest last, each with the `now` it was solved
/// at.
#[derive(Debug, Default)]
pub(crate) struct PlanCache(VecDeque<(PlanDiff, u64)>);

impl PlanCache {
    fn remember(&mut self, diff: PlanDiff, now_ms: u64) {
        self.0.retain(|(d, _)| d.id != diff.id);
        self.0.push_back((diff, now_ms));
        while self.0.len() > PLAN_CACHE_SIZE {
            self.0.pop_front();
        }
    }

    fn get(&self, id: PlanDiffId) -> Option<(PlanDiff, u64)> {
        self.0.iter().find(|(d, _)| d.id == id).cloned()
    }

    fn forget(&mut self, id: PlanDiffId) {
        self.0.retain(|(d, _)| d.id != id);
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

impl Core {
    /// Solve `intent` against the vault as it stands now, remember the diff
    /// under its id, and return it. Writes nothing.
    ///
    /// # Errors
    /// [`PlanError::Closed`] after close; [`PlanError::Invalid`] for an
    /// intent naming nothing the vault holds; [`PlanError::Engine`] for a
    /// failed read.
    pub fn plan_preview(&self, intent: &PlanIntent) -> Result<PlanDiff, PlanError> {
        if self.is_closed() {
            return Err(PlanError::Closed);
        }
        let now_ms = self.now_ms();
        let diff = {
            let db = self.db.lock();
            self.engine.plan_preview(&db, intent, now_ms)?
        };
        self.plans.lock().remember(diff.clone(), now_ms);
        Ok(diff)
    }

    /// Apply exactly the remembered diff `id`, in one local transaction.
    ///
    /// Never re-solves: the inputs the diff was solved from are read again
    /// and digested, and any difference refuses with nothing written.
    ///
    /// # Errors
    /// [`PlanError::Unknown`] (`PLAN_UNKNOWN`) for a diff that is not
    /// remembered; [`PlanError::Stale`] (`PLAN_STALE`) when an input changed
    /// since the preview; [`PlanError::NotCommittable`] for a diff with a
    /// Blocking violation; [`PlanError::Closed`] after close.
    pub fn plan_commit(&self, id: PlanDiffId) -> Result<PlanCommitResult, PlanError> {
        if self.is_closed() {
            return Err(PlanError::Closed);
        }
        let (diff, now_ms) = self.plans.lock().get(id).ok_or(PlanError::Unknown)?;
        let moved = {
            let mut db = self.db.lock();
            self.engine.plan_apply(&mut db, &diff, now_ms)?
        };
        self.plans.lock().forget(id);
        let items = diff
            .moves
            .iter()
            .map(|m| m.item)
            .chain(diff.unscheduled.iter().map(|u| u.item));
        for item in items {
            let _ = self.changes_tx.send(DomainEvent::Updated(item));
        }
        if moved > 0 && self.sync_shared.is_active() {
            self.sync_shared.poke_submit();
        }
        Ok(PlanCommitResult { moved })
    }

    /// Forget the remembered diff `id`. Optional: the cache evicts on its own.
    pub fn plan_cancel(&self, id: PlanDiffId) {
        self.plans.lock().forget(id);
    }
}
