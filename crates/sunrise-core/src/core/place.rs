//! On-device presence at Places (`docs/02-domain/places.md` §Presence,
//! ADR-0051 §3).
//!
//! The client's region monitor reports each enter, exit and state answer
//! here, and the core answers place requirements from what it was told. None
//! of it is written: no op, no envelope, no row, no file, no log line. A
//! restart, a revoked permission and a closed handle all leave every place
//! unknown until the OS reports it again.

use super::Core;
use crate::CoreError;
use std::collections::BTreeSet;
use sunrise_domain::{
    evaluate_place, select_regions, PlaceEval, PlaceId, Presence, Task, TaskPlaceReq,
};
use sunrise_id::EntityRef;

/// The vault's live places, and every open task that names a place with its
/// requirement.
type PlaceWorld = (Vec<sunrise_domain::Place>, Vec<(Task, TaskPlaceReq)>);

/// One open task's place requirement and how it evaluates now: a row of
/// [`Core::place_requirements`].
///
/// In memory, for the client that asked: it has no `Serialize`, because its
/// evaluation is a reading of where the user is (ADR-0051 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlaceStatus {
    /// The task.
    pub task: EntityRef,
    /// The places its constraints name, any one of which satisfies it.
    pub places: Vec<PlaceId>,
    /// Whether any constraint naming a place is `hard`.
    pub hard: bool,
    /// How the requirement evaluates now, a deleted place being unknown.
    pub eval: PlaceEval,
    /// "Not actionable here": a `hard` requirement that evaluates
    /// `Violated`. A flag for views; it never blocks a write.
    pub not_actionable_here: bool,
}

impl Core {
    /// The OS said the user is (`present`) or is not inside `place`'s
    /// region: a region enter, exit or `requestState(for:)` answer.
    ///
    /// Held in memory for the life of this handle and never written. Ignored
    /// once the handle is closed, so a callback that races `close` cannot
    /// leave a state behind.
    pub fn on_place_presence(&self, place: PlaceId, present: bool) {
        let mut presence = self.place_presence.lock();
        if !self.is_closed() {
            presence.set(place, present);
        }
    }

    /// The OS stopped monitoring `place`, or the Place was deleted: it is
    /// unknown from now on, until it is reported again.
    pub fn forget_place_presence(&self, place: PlaceId) {
        self.place_presence.lock().forget(place);
    }

    /// Location permission was denied or revoked: every place is unknown,
    /// and every place requirement reads as if the task had none.
    pub fn clear_place_presence(&self) {
        self.place_presence.lock().clear();
    }

    /// How a requirement naming `required` (any one satisfies it) evaluates
    /// now, from what the OS last reported. See
    /// [`sunrise_domain::evaluate_place`].
    #[must_use]
    pub fn evaluate_place(&self, required: &[PlaceId]) -> PlaceEval {
        evaluate_place(required, &self.place_presence.lock())
    }

    /// What the device knows, narrowed to the places the vault holds live: a
    /// deleted place, or one this replica does not hold, is unknown
    /// (`docs/02-domain/places.md` §Presence), whatever the OS last said.
    fn live_presence(&self, live: &BTreeSet<PlaceId>) -> Presence {
        let known = self.place_presence.lock();
        let mut out = Presence::new();
        for &place in live {
            if let Some(present) = known.get(place) {
                out.set(place, present);
            }
        }
        out
    }

    /// The vault's live places and every open task's place requirement, read
    /// under one lock.
    fn place_world(&self) -> Result<PlaceWorld, CoreError> {
        self.ensure_open()?;
        let db = self.db.lock();
        Ok((
            self.engine.live_places(&db)?,
            self.engine.place_requirements(&db)?,
        ))
    }

    /// Every open task that names a place, with how its requirement
    /// evaluates now and whether it is "not actionable here"
    /// (`docs/02-domain/places.md` §Views), by task id.
    ///
    /// # Errors
    ///
    /// [`CoreError::Closed`] after close, and a storage read error.
    pub fn place_requirements(&self) -> Result<Vec<TaskPlaceStatus>, CoreError> {
        let (places, reqs) = self.place_world()?;
        let live = places.iter().map(sunrise_domain::Place::place_id).collect();
        let presence = self.live_presence(&live);
        Ok(reqs
            .into_iter()
            .map(|(task, req)| {
                let eval = req.evaluate(&presence);
                TaskPlaceStatus {
                    task: task.id,
                    not_actionable_here: req.not_actionable_here(&presence),
                    places: req.places,
                    hard: req.hard,
                    eval,
                }
            })
            .collect())
    }

    /// "Here now": every open task whose place requirement evaluates
    /// `Satisfied`, by task id (`docs/02-domain/places.md` §Views).
    ///
    /// # Errors
    ///
    /// [`CoreError::Closed`] after close, and a storage read error.
    pub fn here_now(&self) -> Result<Vec<Task>, CoreError> {
        let (places, reqs) = self.place_world()?;
        let live = places.iter().map(sunrise_domain::Place::place_id).collect();
        let presence = self.live_presence(&live);
        Ok(reqs
            .into_iter()
            .filter(|(_, req)| req.evaluate(&presence) == PlaceEval::Satisfied)
            .map(|(task, _)| task)
            .collect())
    }

    /// The places the client should ask the OS to monitor, at most
    /// `cap - 1` of them: [`sunrise_domain::select_regions`] over the vault's
    /// live places and every open task's requirement (ADR-0051 §5).
    ///
    /// `approx_position` (`(lat_e7, lon_e7)`) is used for this call and kept
    /// nowhere.
    ///
    /// # Errors
    ///
    /// [`CoreError::Closed`] after close, and a storage read error.
    pub fn select_regions(
        &self,
        approx_position: Option<(i32, i32)>,
        cap: usize,
    ) -> Result<Vec<PlaceId>, CoreError> {
        let (places, reqs) = self.place_world()?;
        let reqs: Vec<TaskPlaceReq> = reqs.into_iter().map(|(_, r)| r).collect();
        Ok(select_regions(&places, &reqs, approx_position, cap))
    }
}
