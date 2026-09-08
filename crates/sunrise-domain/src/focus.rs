//! Focus sessions per `docs/08-features/focus-mode.md`, represented as
//! [ADR-0013](../../../docs/11-adr/0013-focus-session-op-representation.md)
//! decides: an **append-only record keyed by its own `EntityRef`** (`fcs_`),
//! never a mutable field on a Task.
//!
//! A session is two ops and no mutable register:
//!
//! * a [`FocusStart`] — `(task_id, stream_id, started_at_ms, planned_ms,
//!   energy, kind)`, and
//! * later, a separate [`FocusEnd`] — `(ended_at_ms, actual_focused_ms,
//!   interruptions, completed_task)`.
//!
//! Both address the same session id; the session is otherwise immutable. A
//! `start` with no `end` is a **valid** state — it reads as *still running*
//! ([`FocusSession::is_running`]) — and never a tombstone case.
//!
//! **Nothing ticking is ever persisted.** A live session's elapsed time is
//! derived on read from the caller's injected clock
//! ([`FocusSession::elapsed_ms`]); `actual_focused_ms` is frozen only at the
//! `end` op. That is the determinism rule from
//! `docs/01-architecture/shared-core.md` §determinism applied to a timer.
//!
//! Everything in this module is a **pure function over values the caller has
//! already read** — no storage, no clock, no ambient state — so the planner
//! ranking ([`rank_focus_plan`]), the estimate calibration
//! ([`fold_focus_stats`]), the session sizing ([`plan_session`]) and the
//! unblock cascade ([`unblock_cascade`]) all unit-test in isolation.

use crate::common::Energy;
use crate::deps::DependencyGraph;
use crate::epoch_ms;
use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

// ---------------------------------------------------------------------------
// Timer policy constants (`docs/08-features/focus-mode.md` §Pomodoro)
// ---------------------------------------------------------------------------

/// Default work segment: 25 minutes.
pub const POMODORO_MS: u64 = 25 * 60 * 1000;

/// Default short break: 5 minutes.
pub const SHORT_BREAK_MS: u64 = 5 * 60 * 1000;

/// Default long break, taken after [`CYCLES_BEFORE_LONG_BREAK`] work
/// segments: 15 minutes.
pub const LONG_BREAK_MS: u64 = 15 * 60 * 1000;

/// Work segments between long breaks.
pub const CYCLES_BEFORE_LONG_BREAK: u32 = 4;

// ---------------------------------------------------------------------------
// Scalars
// ---------------------------------------------------------------------------

/// What a session *is*: focused work, or the break that follows it. Breaks are
/// recorded so the timeline reconstructs, but they never count as focused time
/// and never feed estimate calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusKind {
    /// A work segment.
    Work,
    /// A break segment.
    Break,
}

impl FocusKind {
    /// Parse from the wire/storage string. An unrecognised value degrades to
    /// [`FocusKind::Work`] rather than failing.
    ///
    /// Focus sessions default to work; counting an unknown kind as a break would
    /// under-report time actually spent.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "break" => Self::Break,
            // "work" and anything this build has never heard of.
            _ => Self::Work,
        }
    }
}

crate::unknown::lossy_enum!(FocusKind);

impl FocusKind {
    /// Wire/storage tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Break => "break",
        }
    }

    /// Parse a storage tag. `None` for anything else.
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "work" => Some(Self::Work),
            "break" => Some(Self::Break),
            _ => None,
        }
    }
}

/// How long *this* session runs — the per-session choice from
/// `docs/08-features/focus-mode.md` §Adaptive session length.
///
/// `timeboxed to my next Block` is deliberately absent: Block has no command
/// path in v1, so there is no next Block to box against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLength {
    /// One pomodoro (the 25-minute default).
    OnePomodoro,
    /// Sized to the task's `estimated_duration_s`, capped at one pomodoro —
    /// past that the work chunks (see [`plan_session`]).
    SizedToEstimate,
    /// Open-ended: no planned length, the user stops when the task is done.
    UntilDone,
}

/// A one-tap reason for bailing out early or switching task
/// (`docs/08-features/focus-mode.md` §Interruption capture). Data the user
/// opted into; there is no shame UI and no score attached to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionReason {
    /// The user interrupted themselves.
    SelfInterrupt,
    /// A meeting or another person.
    Meeting,
    /// The work turned out to be blocked.
    Blocked,
    /// Anything else.
    Other,
}

impl Interruption {
    /// Interruption instant as epoch milliseconds.
    #[must_use]
    pub fn at_ms(&self) -> u64 {
        epoch_ms::to_u64(self.at)
    }
}

impl InterruptionReason {
    /// Wire/storage tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfInterrupt => "self",
            Self::Meeting => "meeting",
            Self::Blocked => "blocked",
            Self::Other => "other",
        }
    }

    /// Parse a storage tag; unknown values degrade to [`Self::Other`] rather
    /// than failing a read — an interruption reason is never load-bearing.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "self" => Self::SelfInterrupt,
            "meeting" => Self::Meeting,
            "blocked" => Self::Blocked,
            _ => Self::Other,
        }
    }
}

crate::unknown::lossy_enum!(InterruptionReason);

/// "Chunk N of M" — the checkpoint marker shown when a task's estimate exceeds
/// one session, so a long task shows visible progress *within* a sitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// 1-based index of this sitting.
    pub index: u32,
    /// Total sittings the estimate implies.
    pub total: u32,
}

// ---------------------------------------------------------------------------
// The two ops
// ---------------------------------------------------------------------------

/// The `start` op: everything known when a session begins. Immutable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusStart {
    /// This session's own id (`fcs_`), minted at start.
    pub id: EntityRef,
    /// Task being focused on.
    pub task_id: EntityRef,
    /// Owning Stream (the op's routing stream, and the stats bucket).
    pub stream_id: EntityRef,
    /// Wall clock at start, from the injected clock.
    ///
    /// A `Timestamp` rather than a bare `u64`, so it cannot be confused with
    /// the DURATIONS beside it. The wire form is unchanged — still an integer
    /// count of milliseconds under the same `started_at_ms` key.
    #[serde(rename = "started_at_ms", with = "crate::epoch_ms")]
    pub started_at: Timestamp,
    /// Planned length, `None` for an open-ended `until done` session.
    #[serde(default)]
    pub planned_ms: Option<u64>,
    /// The session's declared **energy budget** — what the user has in the
    /// tank right now, which the planner matches work against.
    #[serde(default)]
    pub energy: Option<Energy>,
    /// Work or break.
    pub kind: FocusKind,
    /// "Chunk N of M", when the task's estimate exceeds one session.
    #[serde(default)]
    pub chunk: Option<Chunk>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl FocusStart {
    /// Start instant as epoch milliseconds — the form storage columns and the
    /// stats grid still speak.
    #[must_use]
    pub fn started_at_ms(&self) -> u64 {
        epoch_ms::to_u64(self.started_at)
    }
}

/// The `end` op: the frozen tail of a session. Immutable, and addressed to the
/// same session id as its [`FocusStart`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusEnd {
    /// The session being closed.
    pub session_id: EntityRef,
    /// Wall clock at end. Wire form unchanged (integer ms, `ended_at_ms`).
    #[serde(rename = "ended_at_ms", with = "crate::epoch_ms")]
    pub ended_at: Timestamp,
    /// Focused time this session actually contained — **computed and frozen
    /// here**, never stored while the session runs.
    ///
    /// Stays `u64` milliseconds: it is a DURATION, not an instant, and the
    /// distinction is exactly what having one type for both used to hide.
    pub actual_focused_ms: u64,
    /// Interruptions the ending device knows about. Set-union'd with any
    /// [`Interruption`] ops on merge; never a replacement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interruptions: Vec<Interruption>,
    /// Whether the task was completed in this session.
    pub completed_task: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl FocusEnd {
    /// End instant as epoch milliseconds.
    #[must_use]
    pub fn ended_at_ms(&self) -> u64 {
        epoch_ms::to_u64(self.ended_at)
    }
}

/// One logged interruption. Its own append-only record: the `(session, at,
/// reason)` triple is the primary key, so re-delivery is idempotent and two
/// devices logging different interruptions on one session both survive.
///
/// The ONE entity with no `unknown` map. It has no identity apart from its
/// three fields — the whole triple IS the key — so a preserved unknown field
/// would change what the record *is* rather than what it says, and would give
/// two records that a peer considers the same one different identities here.
/// It is `Copy` and totally ordered for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Interruption {
    /// Session interrupted.
    pub session_id: EntityRef,
    /// When it happened. Wire form unchanged (integer ms, `at_ms`).
    #[serde(rename = "at_ms", with = "crate::epoch_ms")]
    pub at: Timestamp,
    /// One-tap reason.
    pub reason: InterruptionReason,
}

// ---------------------------------------------------------------------------
// The read-side view
// ---------------------------------------------------------------------------

/// A session as a reader sees it: its immutable start, its `end` if one has
/// arrived, and the union of its interruptions.
///
/// This is a *view*, assembled from the two append-only records — it is never
/// itself written anywhere, which is what keeps a running timer out of storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusSession {
    /// The `start` op.
    pub start: FocusStart,
    /// The `end` op, if it has arrived. `None` means **still running** — a
    /// valid state, not a broken one.
    #[serde(default)]
    pub end: Option<FocusEnd>,
    /// Every interruption logged against this session, deduped and ordered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interruptions: Vec<Interruption>,
}

impl FocusSession {
    /// A session with no `end` op is still running. The app dying mid-session
    /// leaves exactly this state and it needs no repair: an `end` op may arrive
    /// later from any device.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.end.is_none()
    }

    /// Wall-clock span of the session, **derived, never stored**.
    ///
    /// Running: `now_ms - started_at`. Ended: `ended_at - started_at`. A clock
    /// that reads behind the start (skew, or a stale `now_ms`) saturates to
    /// zero rather than wrapping.
    #[must_use]
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        match &self.end {
            Some(e) => e.ended_at_ms().saturating_sub(self.start.started_at_ms()),
            None => now_ms.saturating_sub(self.start.started_at_ms()),
        }
    }

    /// Focused time: the frozen `actual_focused_ms` once the session has ended,
    /// and the derived elapsed while it runs.
    #[must_use]
    pub fn focused_ms(&self, now_ms: u64) -> u64 {
        match &self.end {
            Some(e) => e.actual_focused_ms,
            None => self.elapsed_ms(now_ms),
        }
    }

    /// Remaining time against the planned length; `None` for an open-ended
    /// (`until done`) session, `Some(0)` once the plan is spent.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: u64) -> Option<u64> {
        self.start
            .planned_ms
            .map(|p| p.saturating_sub(self.elapsed_ms(now_ms)))
    }

    /// Whether a planned session has run past its plan.
    #[must_use]
    pub fn overran(&self, now_ms: u64) -> bool {
        self.remaining_ms(now_ms) == Some(0) && self.start.planned_ms.is_some()
    }
}

// ---------------------------------------------------------------------------
// Adaptive session length + chunking
// ---------------------------------------------------------------------------

/// The next segment the timer should run (`docs/08-features/focus-mode.md`
/// §Pomodoro and timer policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    /// A work segment.
    Work,
    /// The 5-minute break between pomodoros.
    ShortBreak,
    /// The longer break after [`CYCLES_BEFORE_LONG_BREAK`] work segments.
    LongBreak,
}

impl Segment {
    /// Default length of this segment in ms.
    #[must_use]
    pub const fn default_ms(self) -> u64 {
        match self {
            Self::Work => POMODORO_MS,
            Self::ShortBreak => SHORT_BREAK_MS,
            Self::LongBreak => LONG_BREAK_MS,
        }
    }

    /// The [`FocusKind`] a session of this segment records as.
    #[must_use]
    pub const fn kind(self) -> FocusKind {
        match self {
            Self::Work => FocusKind::Work,
            Self::ShortBreak | Self::LongBreak => FocusKind::Break,
        }
    }
}

/// Which segment follows `work_sessions_done` completed work segments.
///
/// `0` → the first work segment. After a work segment the answer is a break,
/// long every [`CYCLES_BEFORE_LONG_BREAK`]. Callers alternate by counting only
/// work segments, so a skipped break never desynchronizes the cycle.
#[must_use]
pub const fn break_after(work_sessions_done: u32) -> Segment {
    if work_sessions_done == 0 {
        return Segment::Work;
    }
    if work_sessions_done.is_multiple_of(CYCLES_BEFORE_LONG_BREAK) {
        Segment::LongBreak
    } else {
        Segment::ShortBreak
    }
}

/// How many sittings of `session_ms` an estimate of `estimated_ms` implies.
///
/// `None` when the estimate fits inside one session (no chunk marker is shown)
/// or when there is no usable estimate.
#[must_use]
pub fn chunk_count(estimated_ms: u64, session_ms: u64) -> Option<u32> {
    if session_ms == 0 || estimated_ms == 0 || estimated_ms <= session_ms {
        return None;
    }
    let n = estimated_ms.div_ceil(session_ms);
    Some(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The sizing decision for one session: how long to run, and whether to show a
/// "chunk N of M" checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlan {
    /// Planned length, `None` for `until done`.
    pub planned_ms: Option<u64>,
    /// Chunk marker when the estimate exceeds one session.
    pub chunk: Option<Chunk>,
}

/// Size a session from the chosen [`SessionLength`], the task's estimate, and
/// how many work sessions the task has already had.
///
/// * [`SessionLength::OnePomodoro`] — always `pomodoro_ms`.
/// * [`SessionLength::SizedToEstimate`] — the whole estimate when it fits in
///   one pomodoro; otherwise one pomodoro, chunked.
/// * [`SessionLength::UntilDone`] — no planned length at all.
///
/// Chunking is orthogonal to the length choice: whenever the estimate exceeds
/// the session about to run, the caller gets `chunk N of M` with `N` derived
/// from `work_sessions_done` (clamped to `M`, so a task that overruns its
/// estimate reads "chunk 4 of 4" rather than "5 of 4").
#[must_use]
pub fn plan_session(
    length: SessionLength,
    estimated_duration_s: Option<u64>,
    work_sessions_done: u32,
    pomodoro_ms: u64,
) -> SessionPlan {
    let estimated_ms = estimated_duration_s.unwrap_or(0).saturating_mul(1000);
    let planned_ms = match length {
        SessionLength::UntilDone => None,
        SessionLength::OnePomodoro => Some(pomodoro_ms),
        SessionLength::SizedToEstimate => {
            if estimated_ms == 0 {
                // No estimate to size against: fall back to the default.
                Some(pomodoro_ms)
            } else {
                Some(estimated_ms.min(pomodoro_ms))
            }
        }
    };
    // An open-ended session has no sitting to be "N of", so it carries no
    // chunk marker even for a large estimate.
    let chunk = match planned_ms {
        None => None,
        Some(session_ms) => chunk_count(estimated_ms, session_ms).map(|total| Chunk {
            index: work_sessions_done.saturating_add(1).min(total),
            total,
        }),
    };
    SessionPlan { planned_ms, chunk }
}

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
const fn energy_rank(e: Energy) -> u8 {
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

// ---------------------------------------------------------------------------
// Estimate calibration — a pure fold over the immutable session log
// ---------------------------------------------------------------------------

/// One row of the session log as the fold sees it: the session's own facts
/// joined with the *task's* estimate.
///
/// Deliberately flat and owned so [`fold_focus_stats`] needs no database, no
/// graph, and no clock beyond the `now_ms` the caller passes in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Session id.
    pub session: EntityRef,
    /// Task focused on.
    pub task: EntityRef,
    /// Owning Stream — the per-Stream calibration bucket.
    pub stream: EntityRef,
    /// The session's declared energy budget — the per-energy bucket.
    pub energy: Option<Energy>,
    /// Work or break.
    pub kind: FocusKind,
    /// Start, ms since epoch.
    pub started_at_ms: u64,
    /// End, ms since epoch; `None` while the session runs.
    pub ended_at_ms: Option<u64>,
    /// Frozen focused time; `None` while the session runs.
    pub actual_focused_ms: Option<u64>,
    /// The task's `estimated_duration_s` at read time.
    pub estimated_duration_s: Option<u64>,
    /// Whether the task was completed in this session.
    pub completed_task: bool,
    /// Interruptions logged against this session.
    pub interruptions: Vec<Interruption>,
}

impl SessionRecord {
    /// Focused time contributed by this record: frozen once ended, derived
    /// from the clock while running.
    #[must_use]
    pub fn focused_ms(&self, now_ms: u64) -> u64 {
        match self.actual_focused_ms {
            Some(ms) => ms,
            None => now_ms.saturating_sub(self.started_at_ms),
        }
    }

    /// A session still running has no `end` op yet.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.ended_at_ms.is_none()
    }
}

/// The estimate-vs-actual calibration factor for one bucket.
///
/// `factor > 1.0` means work runs **long** against its estimate — "your
/// 30-minute estimates run ~1.7× long" is `factor == 1.7`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// `actual_ms / estimated_ms`.
    pub factor: f64,
    /// Completed, estimated tasks behind the factor.
    pub samples: u32,
    /// Total estimated time across those tasks.
    pub estimated_ms: u64,
    /// Total recorded focused time across those tasks.
    pub actual_ms: u64,
}

/// Focus totals for one Stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamFocus {
    /// Stream id.
    pub stream: EntityRef,
    /// Work sessions recorded against it.
    pub sessions: u32,
    /// Total focused time.
    pub focused_ms: u64,
    /// Calibration for this Stream, when it has at least one completed,
    /// estimated task.
    pub calibration: Option<Calibration>,
}

/// Focus totals for one energy budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergyFocus {
    /// The declared budget the sessions ran under; `None` groups sessions that
    /// declared none.
    pub energy: Option<Energy>,
    /// Work sessions recorded under it.
    pub sessions: u32,
    /// Total focused time.
    pub focused_ms: u64,
    /// Calibration for this energy budget.
    pub calibration: Option<Calibration>,
}

/// How often one interruption reason came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptionTally {
    /// The reason.
    pub reason: InterruptionReason,
    /// Occurrences.
    pub count: u32,
}

/// The whole focus picture: totals, per-Stream and per-energy breakdowns, the
/// calibration factors, and the top focus-breakers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusStats {
    /// Every session in range, work and break.
    pub sessions: u32,
    /// Work sessions only.
    pub work_sessions: u32,
    /// Sessions with a `start` and no `end` — still running.
    pub running: u32,
    /// Total focused (work) time, with running sessions counted from the
    /// caller's clock.
    pub total_focused_ms: u64,
    /// Total interruptions logged.
    pub interruptions: u32,
    /// Per-Stream breakdown, ordered by stream id.
    pub per_stream: Vec<StreamFocus>,
    /// Per-energy breakdown, ordered Low, Med, High, then unset.
    pub per_energy: Vec<EnergyFocus>,
    /// Whole-vault calibration.
    pub overall: Option<Calibration>,
    /// Top focus-breakers, most frequent first.
    pub top_interruptions: Vec<InterruptionTally>,
}

/// Accumulator for one calibration bucket: estimated and actual, per task.
#[derive(Debug, Default)]
struct CalibAcc {
    /// task → (estimate_ms, actual_ms, completed)
    tasks: BTreeMap<EntityRef, (u64, u64, bool)>,
}

impl CalibAcc {
    fn observe(&mut self, r: &SessionRecord) {
        // Running sessions are excluded: their focused time is derived from
        // the clock, and a factor that drifts with wall time is not a
        // calibration. Only frozen `actual_focused_ms` feeds the fold.
        let Some(actual) = r.actual_focused_ms else {
            return;
        };
        let est_ms = r.estimated_duration_s.unwrap_or(0).saturating_mul(1000);
        let e = self.tasks.entry(r.task).or_insert((0, 0, false));
        e.0 = est_ms;
        e.1 = e.1.saturating_add(actual);
        e.2 |= r.completed_task;
    }

    /// Only **completed** tasks with a usable estimate calibrate: a task still
    /// in flight has an actual that is a lower bound, not a measurement.
    #[allow(clippy::cast_precision_loss)]
    fn finish(&self) -> Option<Calibration> {
        let mut estimated_ms: u64 = 0;
        let mut actual_ms: u64 = 0;
        let mut samples: u32 = 0;
        for (est, act, completed) in self.tasks.values() {
            if !*completed || *est == 0 {
                continue;
            }
            estimated_ms = estimated_ms.saturating_add(*est);
            actual_ms = actual_ms.saturating_add(*act);
            samples = samples.saturating_add(1);
        }
        if samples == 0 || estimated_ms == 0 {
            return None;
        }
        Some(Calibration {
            factor: actual_ms as f64 / estimated_ms as f64,
            samples,
            estimated_ms,
            actual_ms,
        })
    }
}

/// Fold the immutable session log into [`FocusStats`].
///
/// Pure: same input, same output, with `now_ms` the only time that enters —
/// and it only affects the *running* sessions' contribution to
/// `total_focused_ms`, never a calibration factor. A running session has no
/// frozen `actual_focused_ms`, and a factor derived from a duration that grows
/// with the wall clock would not be a calibration, so those sessions are left
/// out of the calibration fold entirely.
///
/// Breaks are counted in `sessions` but contribute no focused time and no
/// calibration: a break is not work.
#[must_use]
pub fn fold_focus_stats(records: &[SessionRecord], now_ms: u64) -> FocusStats {
    let mut sessions: u32 = 0;
    let mut work_sessions: u32 = 0;
    let mut running: u32 = 0;
    let mut total_focused_ms: u64 = 0;
    let mut interruptions: u32 = 0;

    let mut stream_sessions: BTreeMap<EntityRef, (u32, u64)> = BTreeMap::new();
    let mut energy_sessions: BTreeMap<Option<Energy>, (u32, u64)> = BTreeMap::new();
    let mut stream_calib: BTreeMap<EntityRef, CalibAcc> = BTreeMap::new();
    let mut energy_calib: BTreeMap<Option<Energy>, CalibAcc> = BTreeMap::new();
    let mut overall = CalibAcc::default();
    let mut reasons: BTreeMap<InterruptionReason, u32> = BTreeMap::new();
    // Interruptions are set-semantics: the same triple may reach us both as
    // its own op and inside an `end` op's list.
    let mut seen_interruptions: BTreeSet<Interruption> = BTreeSet::new();

    for r in records {
        sessions = sessions.saturating_add(1);
        if r.is_running() {
            running = running.saturating_add(1);
        }
        for i in &r.interruptions {
            if seen_interruptions.insert(*i) {
                interruptions = interruptions.saturating_add(1);
                *reasons.entry(i.reason).or_insert(0) += 1;
            }
        }
        if r.kind != FocusKind::Work {
            continue;
        }
        work_sessions = work_sessions.saturating_add(1);
        let focused = r.focused_ms(now_ms);
        total_focused_ms = total_focused_ms.saturating_add(focused);

        let s = stream_sessions.entry(r.stream).or_insert((0, 0));
        s.0 = s.0.saturating_add(1);
        s.1 = s.1.saturating_add(focused);

        let e = energy_sessions.entry(r.energy).or_insert((0, 0));
        e.0 = e.0.saturating_add(1);
        e.1 = e.1.saturating_add(focused);

        overall.observe(r);
        stream_calib.entry(r.stream).or_default().observe(r);
        energy_calib.entry(r.energy).or_default().observe(r);
    }

    let per_stream = stream_sessions
        .into_iter()
        .map(|(stream, (n, ms))| StreamFocus {
            stream,
            sessions: n,
            focused_ms: ms,
            calibration: stream_calib.get(&stream).and_then(CalibAcc::finish),
        })
        .collect();

    let mut per_energy: Vec<EnergyFocus> = energy_sessions
        .into_iter()
        .map(|(energy, (n, ms))| EnergyFocus {
            energy,
            sessions: n,
            focused_ms: ms,
            calibration: energy_calib.get(&energy).and_then(CalibAcc::finish),
        })
        .collect();
    // `Option<Energy>` sorts None first; Low → Med → High → unset reads better.
    per_energy.sort_by_key(|e| e.energy.map_or(u8::MAX, energy_rank));

    let mut top_interruptions: Vec<InterruptionTally> = reasons
        .into_iter()
        .map(|(reason, count)| InterruptionTally { reason, count })
        .collect();
    top_interruptions.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));

    FocusStats {
        sessions,
        work_sessions,
        running,
        total_focused_ms,
        interruptions,
        per_stream,
        per_energy,
        overall: overall.finish(),
        top_interruptions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn fcs(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::FocusSession, [b; 16])
    }
    fn tsk(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [b; 16])
    }
    fn strm(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [b; 16])
    }

    fn start(id: u8, task: u8, at: u64) -> FocusStart {
        FocusStart {
            id: fcs(id),
            task_id: tsk(task),
            stream_id: strm(1),
            started_at: epoch_ms::from_u64(at),
            planned_ms: Some(POMODORO_MS),
            energy: Some(Energy::High),
            kind: FocusKind::Work,
            chunk: None,
            unknown: Unknowns::new(),
        }
    }

    fn record(
        id: u8,
        task: u8,
        est_s: Option<u64>,
        actual: Option<u64>,
        done: bool,
    ) -> SessionRecord {
        SessionRecord {
            session: fcs(id),
            task: tsk(task),
            stream: strm(1),
            energy: Some(Energy::High),
            kind: FocusKind::Work,
            started_at_ms: 1_000,
            ended_at_ms: actual.map(|a| 1_000 + a),
            actual_focused_ms: actual,
            estimated_duration_s: est_s,
            completed_task: done,
            interruptions: Vec::new(),
        }
    }

    // ---- the ADR's two load-bearing properties ----

    #[test]
    fn a_dangling_start_reads_as_running() {
        // The app died mid-session: a `start` op with no `end`. That is a
        // valid state, not a tombstone case.
        let s = FocusSession {
            start: start(1, 1, 10_000),
            end: None,
            interruptions: Vec::new(),
        };
        assert!(s.is_running());
        assert_eq!(s.focused_ms(10_000 + 90_000), 90_000);
        // An `end` arriving later — from any device — closes it, and nothing
        // had to be repaired in between.
        let closed = FocusSession {
            end: Some(FocusEnd {
                session_id: fcs(1),
                ended_at: epoch_ms::from_u64(10_000 + 120_000),
                actual_focused_ms: 100_000,
                interruptions: Vec::new(),
                completed_task: true,
                unknown: Unknowns::new(),
            }),
            ..s
        };
        assert!(!closed.is_running());
    }

    #[test]
    fn elapsed_derives_from_the_clock_and_is_never_stored() {
        let s = FocusSession {
            start: start(1, 1, 1_000),
            end: None,
            interruptions: Vec::new(),
        };
        // Same session value, three different clocks, three different answers.
        assert_eq!(s.elapsed_ms(1_000), 0);
        assert_eq!(s.elapsed_ms(61_000), 60_000);
        assert_eq!(s.elapsed_ms(1_000 + POMODORO_MS), POMODORO_MS);
        // A clock reading behind the start saturates instead of wrapping.
        assert_eq!(s.elapsed_ms(0), 0);
        // Remaining is likewise derived, and bottoms out at zero.
        assert_eq!(s.remaining_ms(1_000), Some(POMODORO_MS));
        assert_eq!(s.remaining_ms(1_000 + 60_000), Some(POMODORO_MS - 60_000));
        assert_eq!(s.remaining_ms(u64::MAX), Some(0));
        assert!(s.overran(u64::MAX));
        assert!(!s.overran(1_000));

        // Once ended, the frozen value wins and the clock stops mattering.
        let ended = FocusSession {
            start: start(1, 1, 1_000),
            end: Some(FocusEnd {
                session_id: fcs(1),
                ended_at: epoch_ms::from_u64(1_000 + 300_000),
                actual_focused_ms: 240_000,
                interruptions: Vec::new(),
                completed_task: false,
                unknown: Unknowns::new(),
            }),
            interruptions: Vec::new(),
        };
        assert_eq!(ended.elapsed_ms(u64::MAX), 300_000);
        assert_eq!(ended.focused_ms(u64::MAX), 240_000);
    }

    #[test]
    fn open_ended_sessions_have_no_remaining() {
        let s = FocusSession {
            start: FocusStart {
                planned_ms: None,
                ..start(1, 1, 0)
            },
            end: None,
            interruptions: Vec::new(),
        };
        assert_eq!(s.remaining_ms(999_999), None);
        assert!(!s.overran(999_999));
    }

    // ---- adaptive session length + chunking ----

    #[test]
    fn one_pomodoro_is_always_a_pomodoro() {
        let p = plan_session(SessionLength::OnePomodoro, Some(300), 0, POMODORO_MS);
        assert_eq!(p.planned_ms, Some(POMODORO_MS));
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn sized_to_estimate_shrinks_to_a_short_task() {
        // 10 minutes of work does not get a 25-minute timer.
        let p = plan_session(SessionLength::SizedToEstimate, Some(600), 0, POMODORO_MS);
        assert_eq!(p.planned_ms, Some(600_000));
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn sized_to_estimate_without_an_estimate_falls_back_to_a_pomodoro() {
        let p = plan_session(SessionLength::SizedToEstimate, None, 0, POMODORO_MS);
        assert_eq!(p.planned_ms, Some(POMODORO_MS));
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn an_estimate_over_one_session_chunks_n_of_m() {
        // 90 minutes against a 25-minute session: 4 sittings.
        let p = plan_session(
            SessionLength::SizedToEstimate,
            Some(90 * 60),
            0,
            POMODORO_MS,
        );
        assert_eq!(p.planned_ms, Some(POMODORO_MS));
        assert_eq!(p.chunk, Some(Chunk { index: 1, total: 4 }));
        // Third sitting of the same task.
        let p3 = plan_session(
            SessionLength::SizedToEstimate,
            Some(90 * 60),
            2,
            POMODORO_MS,
        );
        assert_eq!(p3.chunk, Some(Chunk { index: 3, total: 4 }));
        // Overrunning the estimate clamps rather than reading "5 of 4".
        let p9 = plan_session(
            SessionLength::SizedToEstimate,
            Some(90 * 60),
            8,
            POMODORO_MS,
        );
        assert_eq!(p9.chunk, Some(Chunk { index: 4, total: 4 }));
    }

    #[test]
    fn until_done_has_no_plan_and_no_chunks() {
        let p = plan_session(SessionLength::UntilDone, Some(90 * 60), 3, POMODORO_MS);
        assert_eq!(p.planned_ms, None);
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn chunk_count_edges() {
        assert_eq!(chunk_count(0, POMODORO_MS), None);
        assert_eq!(chunk_count(POMODORO_MS, POMODORO_MS), None);
        assert_eq!(chunk_count(POMODORO_MS + 1, POMODORO_MS), Some(2));
        assert_eq!(chunk_count(1_000, 0), None);
    }

    #[test]
    fn long_break_lands_every_fourth_cycle() {
        assert_eq!(break_after(0), Segment::Work);
        assert_eq!(break_after(1), Segment::ShortBreak);
        assert_eq!(break_after(3), Segment::ShortBreak);
        assert_eq!(break_after(4), Segment::LongBreak);
        assert_eq!(break_after(8), Segment::LongBreak);
        assert_eq!(Segment::Work.kind(), FocusKind::Work);
        assert_eq!(Segment::LongBreak.kind(), FocusKind::Break);
        assert_eq!(Segment::ShortBreak.default_ms(), SHORT_BREAK_MS);
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

    // ---- calibration ----

    #[test]
    fn calibration_produces_the_factor_from_known_pairs() {
        // "Your 30-minute estimates run ~1.7x long": estimate 30 min, actual
        // 51 min, task completed.
        let recs = vec![record(1, 1, Some(30 * 60), Some(51 * 60 * 1000), true)];
        let s = fold_focus_stats(&recs, 0);
        let c = s.overall.expect("one completed, estimated task calibrates");
        assert!((c.factor - 1.7).abs() < 1e-9, "factor was {}", c.factor);
        assert_eq!(c.samples, 1);
        assert_eq!(c.estimated_ms, 30 * 60 * 1000);
        assert_eq!(c.actual_ms, 51 * 60 * 1000);
    }

    #[test]
    fn calibration_aggregates_sessions_of_one_task_before_dividing() {
        // A 60-minute estimate finished across three 30-minute sittings is a
        // 1.5x factor, not three separate ones.
        let recs = vec![
            record(1, 1, Some(60 * 60), Some(30 * 60 * 1000), false),
            record(2, 1, Some(60 * 60), Some(30 * 60 * 1000), false),
            record(3, 1, Some(60 * 60), Some(30 * 60 * 1000), true),
        ];
        let c = fold_focus_stats(&recs, 0).overall.expect("calibrates");
        assert!((c.factor - 1.5).abs() < 1e-9, "factor was {}", c.factor);
        assert_eq!(c.samples, 1, "one task, not three");
    }

    #[test]
    fn calibration_ignores_incomplete_and_unestimated_work() {
        let recs = vec![
            // Estimated but never finished: its actual is a lower bound.
            record(1, 1, Some(600), Some(1_200_000), false),
            // Finished but never estimated: nothing to calibrate against.
            record(2, 2, None, Some(600_000), true),
        ];
        let s = fold_focus_stats(&recs, 0);
        assert_eq!(s.overall, None);
        // The time is still counted, it just does not calibrate.
        assert_eq!(s.total_focused_ms, 1_800_000);
        assert_eq!(s.work_sessions, 2);
    }

    #[test]
    fn a_running_session_counts_time_but_never_calibrates() {
        let mut running = record(1, 1, Some(600), None, false);
        running.started_at_ms = 1_000;
        let done = record(2, 2, Some(600), Some(900_000), true);
        let s = fold_focus_stats(&[running, done], 1_000 + 300_000);
        assert_eq!(s.running, 1);
        // 300 s derived from the clock + 900 s frozen.
        assert_eq!(s.total_focused_ms, 300_000 + 900_000);
        // The factor comes from the frozen session alone: 900 s actual against
        // a 600 s estimate.
        let c = s.overall.expect("calibrates");
        assert!((c.factor - 1.5).abs() < 1e-9);
        assert_eq!(c.samples, 1);
    }

    #[test]
    fn breaks_are_recorded_but_never_counted_as_focus() {
        let mut brk = record(2, 1, Some(600), Some(300_000), false);
        brk.kind = FocusKind::Break;
        let s = fold_focus_stats(&[record(1, 1, Some(600), Some(600_000), true), brk], 0);
        assert_eq!(s.sessions, 2);
        assert_eq!(s.work_sessions, 1);
        assert_eq!(s.total_focused_ms, 600_000);
        let c = s.overall.expect("calibrates");
        assert!(
            (c.factor - 1.0).abs() < 1e-9,
            "the break did not inflate it"
        );
    }

    #[test]
    fn stats_bucket_per_stream_and_per_energy() {
        let mut a = record(1, 1, Some(600), Some(1_200_000), true); // 2.0x
        a.stream = strm(1);
        a.energy = Some(Energy::High);
        let mut b = record(2, 2, Some(600), Some(300_000), true); // 0.5x
        b.stream = strm(2);
        b.energy = Some(Energy::Low);
        let s = fold_focus_stats(&[a, b], 0);
        assert_eq!(s.per_stream.len(), 2);
        let s1 = s.per_stream.iter().find(|r| r.stream == strm(1)).unwrap();
        assert!((s1.calibration.unwrap().factor - 2.0).abs() < 1e-9);
        let s2 = s.per_stream.iter().find(|r| r.stream == strm(2)).unwrap();
        assert!((s2.calibration.unwrap().factor - 0.5).abs() < 1e-9);
        // Low before High, per the ordering contract.
        assert_eq!(
            s.per_energy.iter().map(|e| e.energy).collect::<Vec<_>>(),
            vec![Some(Energy::Low), Some(Energy::High)]
        );
        // Overall pools both: 1500 s actual against 1200 s estimated.
        let c = s.overall.unwrap();
        assert_eq!(c.samples, 2);
        assert!((c.factor - 1.25).abs() < 1e-9);
    }

    #[test]
    fn interruptions_dedupe_and_rank() {
        let dup = Interruption {
            session_id: fcs(1),
            at: crate::epoch_ms::from_u64(5_000),
            reason: InterruptionReason::Meeting,
        };
        let mut r1 = record(1, 1, None, Some(600_000), true);
        // The same triple reaching us twice — once as its own op, once inside
        // the `end` op — is one interruption.
        r1.interruptions = vec![
            dup,
            dup,
            Interruption {
                session_id: fcs(1),
                at: crate::epoch_ms::from_u64(9_000),
                reason: InterruptionReason::SelfInterrupt,
            },
        ];
        let mut r2 = record(2, 2, None, Some(600_000), true);
        r2.interruptions = vec![Interruption {
            session_id: fcs(2),
            at: crate::epoch_ms::from_u64(1_000),
            reason: InterruptionReason::Meeting,
        }];
        let s = fold_focus_stats(&[r1, r2], 0);
        assert_eq!(s.interruptions, 3);
        assert_eq!(
            s.top_interruptions,
            vec![
                InterruptionTally {
                    reason: InterruptionReason::Meeting,
                    count: 2
                },
                InterruptionTally {
                    reason: InterruptionReason::SelfInterrupt,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn empty_log_folds_to_zero() {
        let s = fold_focus_stats(&[], 12_345);
        assert_eq!(s.sessions, 0);
        assert_eq!(s.total_focused_ms, 0);
        assert_eq!(s.overall, None);
        assert!(s.per_stream.is_empty());
        assert!(s.top_interruptions.is_empty());
    }

    #[test]
    fn scalar_tags_round_trip() {
        for k in [FocusKind::Work, FocusKind::Break] {
            assert_eq!(FocusKind::from_str_opt(k.as_str()), Some(k));
        }
        assert_eq!(FocusKind::from_str_opt("nope"), None);
        for r in [
            InterruptionReason::SelfInterrupt,
            InterruptionReason::Meeting,
            InterruptionReason::Blocked,
            InterruptionReason::Other,
        ] {
            assert_eq!(InterruptionReason::from_str_lossy(r.as_str()), r);
        }
        // Unknown reasons degrade rather than fail a read.
        assert_eq!(
            InterruptionReason::from_str_lossy("garbage"),
            InterruptionReason::Other
        );
    }
}
