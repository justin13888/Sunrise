//! The interactive planner's seam (`docs/08-features/planner.md` §Core seam,
//! ADR-0048 §2): preview a gesture as a diff, remember it, commit exactly
//! that diff or refuse, and undo a commit per field.

use super::Core;
use crate::engine::{PlanCommitId, PlanError, PlanUndoSkip};
use crate::events::DomainEvent;
use std::collections::VecDeque;
use sunrise_domain::planner::{PlanDiff, PlanDiffId, PlanIntent};

/// How many previews the core remembers; the least recently previewed is
/// forgotten first.
pub const PLAN_CACHE_SIZE: usize = 8;

/// What a commit wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCommitResult {
    /// Names the commit for [`Core::plan_undo`]. Its inverse is stored with
    /// it.
    pub commit: PlanCommitId,
    /// How many tasks and blocks it moved.
    pub moved: u32,
}

/// What an undo restored, and what it left.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanUndoResult {
    /// How many fields it restored.
    pub restored: u32,
    /// Fields written again since the commit, or of an item deleted since,
    /// left as they are.
    pub skipped: Vec<PlanUndoSkip>,
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

    /// Apply exactly the remembered diff `id`, in one local transaction, and
    /// store its inverse with it for [`Self::plan_undo`].
    ///
    /// Never re-solves: the inputs the diff was solved from are read again
    /// and digested, and any difference refuses with nothing written.
    ///
    /// # Errors
    /// [`PlanError::Unknown`] (`PLAN_UNKNOWN`) for a diff that is not
    /// remembered; [`PlanError::Stale`] (`PLAN_STALE`) when an input changed
    /// since the preview; [`PlanError::NotCommittable`] for a diff with a
    /// Blocking violation; [`PlanError::Invalid`] for a block move that would
    /// leave a recurring block's exception applying to nothing, which the
    /// preview flags unless the block changed since; [`PlanError::Closed`]
    /// after close.
    pub fn plan_commit(&self, id: PlanDiffId) -> Result<PlanCommitResult, PlanError> {
        if self.is_closed() {
            return Err(PlanError::Closed);
        }
        let (diff, now_ms) = self.plans.lock().get(id).ok_or(PlanError::Unknown)?;
        let (moved, commit) = {
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
        Ok(PlanCommitResult { commit, moved })
    }

    /// Undo commit `commit` from its stored inverse, per field, in one local
    /// transaction: each field whose register still holds the commit's write
    /// is restored, and the rest are skipped and reported. Never refuses as
    /// stale, and works after the commit's preview has left the cache.
    ///
    /// # Errors
    /// [`PlanError::Unknown`] (`PLAN_UNKNOWN`) for a commit undone already,
    /// forgotten past [`crate::PLAN_UNDO_HISTORY`], or never made;
    /// [`PlanError::Closed`] after close.
    pub fn plan_undo(&self, commit: PlanCommitId) -> Result<PlanUndoResult, PlanError> {
        if self.is_closed() {
            return Err(PlanError::Closed);
        }
        let undone = {
            let mut db = self.db.lock();
            self.engine.plan_undo(&mut db, commit)?
        };
        for item in &undone.items {
            let _ = self.changes_tx.send(DomainEvent::Updated(*item));
        }
        if !undone.items.is_empty() && self.sync_shared.is_active() {
            self.sync_shared.poke_submit();
        }
        Ok(PlanUndoResult {
            restored: undone.restored,
            skipped: undone.skipped,
        })
    }

    /// Forget the remembered diff `id`. Optional: the cache evicts on its own.
    pub fn plan_cancel(&self, id: PlanDiffId) {
        self.plans.lock().forget(id);
    }
}
