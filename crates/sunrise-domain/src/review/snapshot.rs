//! Step 5's saved record, and the draft a finished review turns into.
//!
//! The parent module's doc says why this is the one part of the review that
//! is stored rather than recomputed.

use super::{ReviewTotals, ReviewWindow, StreakRow, WeeklyReview};
use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// One Stream's line in a saved snapshot: counts only, never task lists.
///
/// A snapshot is a record that a review happened and what it concluded, not a
/// second copy of the vault. The lists are recomputable from the op log for the
/// same window; the counts are what the History view shows at a glance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshotStream {
    /// Stream id.
    pub stream: EntityRef,
    /// Its name at review time.
    pub name: String,
    /// Completed in the window.
    pub completed: u32,
    /// Deferred in the window.
    pub deferred: u32,
    /// Created in the window.
    pub created: u32,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`].
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Step 5's saved artifact: *"a review snapshot stored as an opaque entity
/// (queryable in History)"*.
///
/// **Append-only.** A snapshot is written once, keyed by its own `rvw_` id, and
/// never updated — so two devices that both record a review of the same week
/// produce two rows rather than a lost write, and a re-delivered op is an
/// ignored duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshot {
    /// Snapshot id (`rvw_`).
    pub id: EntityRef,
    /// When the review was completed.
    pub created_at: Timestamp,
    /// Window the review covered.
    ///
    /// A `Timestamp`, like `created_at` two fields up. This struct used to
    /// hold both representations at once, which is how the mixture became
    /// visible enough to fix. Wire form unchanged (integer ms).
    #[serde(rename = "window_start_ms", with = "crate::epoch_ms")]
    pub window_start: Timestamp,
    /// Exclusive end of that window.
    #[serde(rename = "window_end_ms", with = "crate::epoch_ms")]
    pub window_end: Timestamp,
    /// The step-5 counts.
    pub totals: ReviewTotals,
    /// Per-Stream counts, ordered by stream id.
    #[serde(default)]
    pub streams: Vec<ReviewSnapshotStream>,
    /// Streaks as they stood at review time.
    #[serde(default)]
    pub streaks: Vec<StreakRow>,
    /// Optional free-text reflection the user typed.
    #[serde(default)]
    pub note: Option<String>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl ReviewSnapshot {
    /// Window start as epoch milliseconds.
    #[must_use]
    pub fn window_start_ms(&self) -> u64 {
        crate::epoch_ms::to_u64(self.window_start)
    }

    /// Window end as epoch milliseconds (exclusive).
    #[must_use]
    pub fn window_end_ms(&self) -> u64 {
        crate::epoch_ms::to_u64(self.window_end)
    }
}

/// Draft submitted to save a snapshot; the core fills the id and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewSnapshotDraft {
    /// Window start (ms since epoch).
    pub window_start_ms: u64,
    /// Window end (ms since epoch, exclusive).
    pub window_end_ms: u64,
    /// Counts.
    pub totals: ReviewTotals,
    /// Per-Stream counts.
    pub streams: Vec<ReviewSnapshotStream>,
    /// Streaks.
    pub streaks: Vec<StreakRow>,
    /// Optional note.
    pub note: Option<String>,
}

/// One Stream's counts as a review screen shows them: the input to
/// [`ReviewSnapshotDraft::from_counts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTally {
    /// The stream.
    pub stream: EntityRef,
    /// Its name on screen.
    pub name: String,
    /// Completed in the window.
    pub completed: usize,
    /// Deferred in the window.
    pub deferred: usize,
    /// Created in the window and untouched since.
    pub created: usize,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl ReviewSnapshotDraft {
    /// The draft of a review, from the counts it showed.
    ///
    /// A snapshot is the one fact about a review the op log cannot re-derive
    /// — that someone did it, and what they saw — so it carries the screen's
    /// counts rather than being recomputed when read. A count past `u32` is
    /// held at the maximum rather than trapping. `note` is stored as given.
    #[must_use]
    pub fn from_counts(
        window: &ReviewWindow,
        totals: ReviewTotals,
        tallies: Vec<StreamTally>,
        streaks: Vec<StreakRow>,
        note: Option<String>,
    ) -> Self {
        Self {
            window_start_ms: window.start_ms,
            window_end_ms: window.end_ms,
            totals,
            streams: tallies
                .into_iter()
                .map(|s| ReviewSnapshotStream {
                    stream: s.stream,
                    name: s.name,
                    completed: count(s.completed),
                    deferred: count(s.deferred),
                    created: count(s.created),
                    unknown: Unknowns::new(),
                })
                .collect(),
            streaks,
            note,
        }
    }
}

impl WeeklyReview {
    /// Turn a completed review into the draft the user saves.
    ///
    /// The per-Stream counts are read straight off the assembled review, so a
    /// snapshot can never disagree with the screen it was saved from.
    #[must_use]
    pub fn to_draft(&self, note: Option<String>) -> ReviewSnapshotDraft {
        let tallies = self
            .streams
            .iter()
            .map(|s| StreamTally {
                stream: s.stream,
                name: s.name.clone(),
                completed: s.completed.len(),
                deferred: s.deferred.len(),
                created: s.created_untouched.len(),
            })
            .collect();
        ReviewSnapshotDraft::from_counts(
            &self.window,
            self.totals.clone(),
            tallies,
            self.streaks.clone(),
            note,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::weekly::tests::eref;
    use sunrise_id::EntityKind;

    /// A count past `u32` holds at the maximum, and the note is stored as
    /// given: trimming it is the client's call, not the record's.
    #[test]
    fn from_counts_clamps_a_count_and_keeps_the_note_as_given() {
        let stream = eref(EntityKind::Stream, 1);
        let d = ReviewSnapshotDraft::from_counts(
            &ReviewWindow::new(1_000, 2_000),
            ReviewTotals::default(),
            vec![StreamTally {
                stream,
                name: "Work".into(),
                completed: usize::MAX,
                deferred: 3,
                created: 0,
            }],
            vec![],
            Some(" quiet ".into()),
        );
        assert_eq!((d.window_start_ms, d.window_end_ms), (1_000, 2_000));
        assert_eq!(d.streams.len(), 1);
        assert_eq!(d.streams[0].completed, u32::MAX);
        assert_eq!(d.streams[0].deferred, 3);
        assert_eq!(d.streams[0].unknown, Unknowns::new());
        assert_eq!(d.note.as_deref(), Some(" quiet "));
    }
}
