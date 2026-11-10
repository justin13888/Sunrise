//! Weekly and daily review per `docs/08-features/reviews-and-stats.md`.
//!
//! > *Periodic review is the most important habit Sunrise wants to enable.
//! > Stats support reviews; they are not a leaderboard.*
//!
//! The weekly review is a five-step guided flow. Steps 2 and 4 are
//! *interactions* — the UI walks the user through triage and slipped-commitment
//! decisions one item at a time — but every step is driven by a **list this
//! module computes**, and the shape of those lists is what makes the flow
//! reviewable rather than a wall of text.
//!
//! # Everything here is a pure fold
//!
//! [`build_weekly_review`] takes values the caller already read — streams,
//! tasks, the activity feed for the window, routines, the focus fold, the
//! trend fold — and assembles the review. It never queries, never reads a
//! clock, and never re-derives a number another fold already owns:
//!
//! * "completed / deferred / dropped this week" comes from the **activity
//!   feed** ([`crate::activity::fold_activity`]), so the review and the
//!   timeline can never disagree about what happened;
//! * time-in-focus and the estimate calibration come from
//!   [`crate::focus::fold_focus_stats`] — the review is where the spec says a
//!   user finally *sees* "your 30-minute estimates run ~1.7× long";
//! * the trends come from [`crate::stats::fold_trends`];
//! * streaks and drift come from [`crate::streak`] / [`crate::stats`].
//!
//! # The snapshot
//!
//! Step 5 says the review is *"saved … as an opaque entity (queryable in
//! History)"*. [`ReviewSnapshot`] is that entity. It is the one part of this
//! feature that is **not** derivable: everything else can be recomputed from
//! the op log for any past week, but "the user actually sat down and reviewed
//! week W on device D" is a new fact. It is therefore an append-only record
//! keyed by its own `rvw_` id — the same representation ADR-0013 chose for
//! focus sessions, and for the same reason: two devices can each record a
//! review without contending over a register, and re-delivery is a no-op.
//!
//! # Layout
//!
//! The weekly fold is one function over one input struct and stays whole in
//! `weekly`. The daily review (`daily`) and the snapshot record (`snapshot`)
//! are separate types the fold does not read, so they live beside it.

mod daily;
mod snapshot;
mod weekly;

pub use daily::{build_daily_review, DailyReview};
pub use snapshot::{ReviewSnapshot, ReviewSnapshotDraft, ReviewSnapshotStream, StreamTally};
pub use weekly::{
    build_weekly_review, ReviewStream, ReviewTotals, StreamReview, WeeklyReview, WeeklyReviewInput,
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A Routine's streak, for step 3 and the snapshot.
///
/// Defined in [`crate::streak`], which owns the streak mechanics; re-exported
/// here because the review is where a streak is read.
pub use crate::streak::StreakRow;

/// A half-open review window `[start_ms, end_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWindow {
    /// Inclusive start (ms since epoch).
    pub start_ms: u64,
    /// Exclusive end (ms since epoch).
    pub end_ms: u64,
}

impl ReviewWindow {
    /// Construct, normalizing an inverted range to an empty one.
    #[must_use]
    pub fn new(start_ms: u64, end_ms: u64) -> Self {
        Self {
            start_ms,
            end_ms: end_ms.max(start_ms),
        }
    }

    /// Whether `at_ms` falls in the window.
    #[must_use]
    pub const fn contains(&self, at_ms: u64) -> bool {
        at_ms >= self.start_ms && at_ms < self.end_ms
    }
}

/// jiff [`Timestamp`] → epoch milliseconds, dropping pre-epoch instants.
fn ts_to_ms(t: Timestamp) -> Option<u64> {
    u64::try_from(t.as_millisecond()).ok()
}

#[cfg(test)]
mod tests {
    use super::weekly::tests::{MON, WEEK};
    use super::*;

    #[test]
    fn review_window_normalizes_an_inverted_range() {
        let w = ReviewWindow::new(MON + WEEK, MON);
        assert_eq!(w.start_ms, MON + WEEK);
        assert_eq!(w.end_ms, MON + WEEK);
        assert!(!w.contains(MON + WEEK), "an empty window contains nothing");
    }
}
