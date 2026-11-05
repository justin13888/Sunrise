//! Step 5's saved record, and the draft a finished review turns into.
//!
//! The parent module's doc says why this is the one part of the review that
//! is stored rather than recomputed.

use super::{ReviewTotals, StreakRow, WeeklyReview};
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

impl WeeklyReview {
    /// Turn a completed review into the draft the user saves.
    ///
    /// The per-Stream counts are read straight off the assembled review, so a
    /// snapshot can never disagree with the screen it was saved from.
    #[must_use]
    pub fn to_draft(&self, note: Option<String>) -> ReviewSnapshotDraft {
        ReviewSnapshotDraft {
            window_start_ms: self.window.start_ms,
            window_end_ms: self.window.end_ms,
            totals: self.totals.clone(),
            streams: self
                .streams
                .iter()
                .map(|s| ReviewSnapshotStream {
                    stream: s.stream,
                    name: s.name.clone(),
                    completed: u32::try_from(s.completed.len()).unwrap_or(u32::MAX),
                    deferred: u32::try_from(s.deferred.len()).unwrap_or(u32::MAX),
                    created: u32::try_from(s.created_untouched.len()).unwrap_or(u32::MAX),
                    unknown: Unknowns::new(),
                })
                .collect(),
            streaks: self.streaks.clone(),
            note,
        }
    }
}
