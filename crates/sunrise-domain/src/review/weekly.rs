//! The weekly review: its input, its output, and the one fold between them.
//!
//! The parent module's doc is why this fold stays whole: it is the place that
//! must not re-derive a number another fold owns.

use super::{ts_to_ms, ReviewWindow, StreakRow};
use crate::activity::{ActivityEvent, ActivityKind};
use crate::focus::{FocusStats, StreamFocus};
use crate::inbox::inbox_stream_ref;
use crate::routine::Routine;
use crate::stats::{RoutineDrift, Trends, WeekBucket};
use crate::stream::Stream;
use crate::task::{Task, TaskState};
use crate::unknown::Unknowns;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

#[cfg(test)]
pub(super) mod tests;

/// Identity of a Stream as the review needs it. A thin row rather than the
/// whole [`Stream`], so the caller can pass the synthetic Inbox too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStream {
    /// Stream id.
    pub id: EntityRef,
    /// Display name.
    pub name: String,
    /// Archived streams drop out of the review.
    pub archived: bool,
    /// Paused streams drop out of the review.
    pub paused: bool,
}

impl From<&Stream> for ReviewStream {
    fn from(s: &Stream) -> Self {
        Self {
            id: s.id,
            name: s.name.clone(),
            archived: s.archived,
            paused: s.paused,
        }
    }
}

/// Step 1 — one Stream's summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamReview {
    /// Stream id.
    pub stream: EntityRef,
    /// Display name.
    pub name: String,
    /// Tasks completed in the window (spec: "count + list (collapsed)").
    pub completed: Vec<Task>,
    /// Tasks deferred at least once in the window (spec: "list (full)").
    pub deferred: Vec<Task>,
    /// Tasks created in the window that nothing has happened to since
    /// (spec: "created this week and not yet acted on").
    pub created_untouched: Vec<Task>,
    /// Time-in-focus + calibration for this Stream, when it has any.
    pub focus: Option<StreamFocus>,
    /// This Stream's completed / deferred trend line.
    pub trend: Vec<WeekBucket>,
}

/// Step 5 — the counts on the summary screen.
///
/// Not `Copy`: it carries the unknown-field map every wire struct does
/// (ADR-0045 §6), because it crosses the wire inside
/// [`ReviewSnapshot`](super::ReviewSnapshot).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTotals {
    /// Tasks completed in the window.
    pub completed: u32,
    /// Defer events in the window.
    pub deferred: u32,
    /// Tasks cancelled or deleted in the window — the spec's "dropped".
    pub dropped: u32,
    /// Tasks created in the window.
    pub created: u32,
    /// Tasks re-opened in the window. Not in the spec's list, but a review
    /// that reports 8 completions while hiding 3 resurrections is misleading.
    pub reopened: u32,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`].
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// The assembled weekly review — one value per spec step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeeklyReview {
    /// The window under review.
    pub window: ReviewWindow,
    /// Step 1 — per-Stream summaries, ordered by stream id. The work that
    /// belongs to no stream is one more, unnamed, under the legacy Inbox
    /// reference ([`crate::inbox::stream_key`]), which sorts first.
    pub streams: Vec<StreamReview>,
    /// Step 2 — untriaged inbox items, oldest first.
    pub inbox: Vec<Task>,
    /// Step 3 — routines with non-trivial drift, worst first.
    pub drifting_routines: Vec<RoutineDrift>,
    /// Step 3 — every live Routine's streak, longest first.
    pub streaks: Vec<StreakRow>,
    /// Step 4 — commitments that came due on or before the window's end and
    /// are still open.
    ///
    /// Deliberately **not** bounded below by the window. Step 4 asks what the
    /// user has broken their word about, and a commitment that slipped three
    /// weeks ago is still slipped; bounding it to the window would hide
    /// exactly the items most in need of a decision. One consequence worth
    /// knowing: this list grows over a vault's lifetime, so the first weekly
    /// review of an old vault can be long.
    pub slipped: Vec<Task>,
    /// Step 5 — the counts.
    pub totals: ReviewTotals,
    /// Time-in-focus and the estimate calibration for the window.
    pub focus: FocusStats,
    /// 12-week trends.
    pub trends: Trends,
}

/// Everything [`build_weekly_review`] needs, fetched by the caller.
#[derive(Debug, Clone)]
pub struct WeeklyReviewInput {
    /// The window under review.
    pub window: ReviewWindow,
    /// Candidate Streams (including the synthetic Inbox row, if wanted).
    pub streams: Vec<ReviewStream>,
    /// Every live Task. Filtering to the window is this module's job.
    pub tasks: Vec<Task>,
    /// The activity feed covering the window, ascending.
    pub activity: Vec<ActivityEvent>,
    /// Every live Routine.
    pub routines: Vec<Routine>,
    /// Per-Routine drift over the tuning window.
    pub drift: Vec<RoutineDrift>,
    /// Focus fold for the window.
    pub focus: FocusStats,
    /// Trend fold.
    pub trends: Trends,
    /// The reader's zone, which a task's floating or all-day times resolve in
    /// before they are compared with the window
    /// (`docs/10-cross-cutting/time.md` §2).
    pub zone: jiff::tz::TimeZone,
}

/// Assemble the weekly review.
///
/// The window filter is applied to **events**, not to task timestamps: "tasks
/// completed this week" means "tasks whose completion event landed this week",
/// which stays right for a task completed on another device and merged in
/// late, and for a task completed twice with a re-open in between.
#[must_use]
pub fn build_weekly_review(input: WeeklyReviewInput) -> WeeklyReview {
    let WeeklyReviewInput {
        window,
        streams,
        tasks,
        activity,
        routines,
        drift,
        focus,
        trends,
        zone,
    } = input;

    let mut totals = ReviewTotals::default();
    let mut completed_ids: BTreeSet<EntityRef> = BTreeSet::new();
    let mut deferred_ids: BTreeSet<EntityRef> = BTreeSet::new();
    let mut created_ids: BTreeSet<EntityRef> = BTreeSet::new();
    // "Acted on" = any event other than the creation itself, including focus
    // sessions: opening a timer on a task is unmistakably acting on it.
    let mut touched_ids: BTreeSet<EntityRef> = BTreeSet::new();

    for e in activity.iter().filter(|e| window.contains(e.at_ms)) {
        match &e.kind {
            ActivityKind::TaskCreated => {
                totals.created = totals.created.saturating_add(1);
                created_ids.insert(e.entity);
            }
            ActivityKind::TaskCompleted => {
                totals.completed = totals.completed.saturating_add(1);
                completed_ids.insert(e.entity);
                touched_ids.insert(e.entity);
            }
            ActivityKind::TaskReopened => {
                totals.reopened = totals.reopened.saturating_add(1);
                // A re-open un-does the completion for the purposes of this
                // review's list: the task is open again and belongs in
                // "slipped", not in "completed".
                completed_ids.remove(&e.entity);
                touched_ids.insert(e.entity);
            }
            ActivityKind::TaskDeferred { .. } => {
                totals.deferred = totals.deferred.saturating_add(1);
                deferred_ids.insert(e.entity);
                touched_ids.insert(e.entity);
            }
            ActivityKind::TaskCancelled | ActivityKind::TaskDeleted => {
                totals.dropped = totals.dropped.saturating_add(1);
                completed_ids.remove(&e.entity);
                touched_ids.insert(e.entity);
            }
            _ => {
                touched_ids.insert(e.entity);
            }
        }
    }

    let by_id: BTreeMap<EntityRef, &Task> = tasks.iter().map(|t| (t.id, t)).collect();
    let focus_by_stream: BTreeMap<EntityRef, &StreamFocus> =
        focus.per_stream.iter().map(|f| (f.stream, f)).collect();

    // Each live stream, and the work that belongs to no stream as a bucket
    // of its own (ADR-0046 §Consequences), keyed by the legacy Inbox
    // reference and unnamed: a client labels it.
    let buckets = streams
        .iter()
        .filter(|s| !s.archived && !s.paused)
        .map(|s| (Some(s.id), s.id, s.name.clone()))
        .chain(std::iter::once((None, inbox_stream_ref(), String::new())));
    let mut stream_reviews: Vec<StreamReview> = buckets
        .map(|(home, key, name)| {
            let pick = |ids: &BTreeSet<EntityRef>| -> Vec<Task> {
                let mut v: Vec<Task> = ids
                    .iter()
                    .filter_map(|id| by_id.get(id).copied())
                    .filter(|t| t.stream_id == home && !t.deleted)
                    .cloned()
                    .collect();
                v.sort_by_key(|t| (t.created_at, t.id));
                v
            };
            let created_untouched: BTreeSet<EntityRef> =
                created_ids.difference(&touched_ids).copied().collect();
            StreamReview {
                stream: key,
                name,
                completed: pick(&completed_ids),
                // "Deferred ≥ 1 time" — the event says it happened this week,
                // the counter confirms the task still carries the deferral.
                deferred: pick(&deferred_ids)
                    .into_iter()
                    .filter(|t| t.deferred_count >= 1)
                    .collect(),
                created_untouched: pick(&created_untouched),
                focus: focus_by_stream.get(&key).map(|f| (*f).clone()),
                trend: trends.for_stream(key).unwrap_or_default().to_vec(),
            }
        })
        // The no-stream bucket is reviewed when it holds something to say.
        .filter(|r| {
            r.stream != inbox_stream_ref()
                || !(r.completed.is_empty()
                    && r.deferred.is_empty()
                    && r.created_untouched.is_empty()
                    && r.focus.is_none()
                    && r.trend.is_empty())
        })
        .collect();
    stream_reviews.sort_by_key(|s| s.stream);

    // The Inbox view (ADR-0046 §3), less what is already under way.
    let mut inbox: Vec<Task> = tasks
        .iter()
        .filter(|t| t.in_inbox() && t.state.effective() == TaskState::Todo)
        .cloned()
        .collect();
    inbox.sort_by_key(|t| (t.created_at, t.id));

    let mut slipped: Vec<Task> = tasks
        .iter()
        .filter(|t| !t.deleted && !t.archived)
        .filter(|t| !matches!(t.state, TaskState::Done | TaskState::Cancelled))
        .filter(|t| commitment_at(t, &zone).is_some_and(|m| m < window.end_ms))
        .cloned()
        .collect();
    // Earliest commitment first: the thing that slipped furthest is the first
    // decision the user is asked to make.
    slipped.sort_by_key(|t| (commitment_at(t, &zone), t.id));

    let mut drifting_routines: Vec<RoutineDrift> =
        drift.into_iter().filter(|d| d.over_threshold).collect();
    drifting_routines.sort_by(|a, b| {
        b.drift
            .partial_cmp(&a.drift)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.routine.cmp(&b.routine))
    });

    let mut streaks: Vec<StreakRow> = routines
        .iter()
        .filter(|r| !r.deleted && !r.archived)
        .map(|r| StreakRow {
            routine: r.id,
            title: r.template.title.clone(),
            streak: r.streak_counter,
            last_completed_at_ms: r
                .last_completed_at
                .as_ref()
                .and_then(|t| t.resolve_in(&zone))
                .and_then(ts_to_ms),
            unknown: Unknowns::new(),
        })
        .collect();
    streaks.sort_by(|a, b| {
        b.streak
            .cmp(&a.streak)
            .then_with(|| a.routine.cmp(&b.routine))
    });

    WeeklyReview {
        window,
        streams: stream_reviews,
        inbox,
        drifting_routines,
        streaks,
        slipped,
        totals,
        focus,
        trends,
    }
}

/// When a Task was committed to, for a reader in `tz`: the earlier of its
/// deadline (missed at the end of an all-day date, ADR-0047 §Due instant) and
/// its planned slot. `None` means the Task carries no commitment this build
/// can place.
fn commitment_at(t: &Task, tz: &jiff::tz::TimeZone) -> Option<u64> {
    let ms = |v: Option<jiff::Timestamp>| v.and_then(|v| u64::try_from(v.as_millisecond()).ok());
    let due = t.hard_due_at.as_ref().and_then(|v| ms(v.due_in(tz)));
    let sched = t.planned_at.as_ref().and_then(|v| ms(v.resolve_in(tz)));
    match (due, sched) {
        (Some(d), Some(s)) => Some(d.min(s)),
        (d, s) => d.or(s),
    }
}
