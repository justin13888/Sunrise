//! On-device presence at Places (`docs/02-domain/places.md` §Presence,
//! ADR-0051 §3).
//!
//! The client's region monitor reports each enter, exit and state answer
//! here, and the core answers place requirements from what it was told. None
//! of it is written: no op, no envelope, no row, no file, no log line. A
//! restart, a revoked permission and a closed handle all leave every place
//! unknown until the OS reports it again.

use super::Core;
use sunrise_domain::{evaluate_place, PlaceEval, PlaceId};

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
}
