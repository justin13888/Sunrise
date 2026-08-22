//! View enum + view-state.

use crate::input::InputLine;
use crate::keymap::{Keymap, Mode};
use jiff::Timestamp;
use sunrise_core::queries::{ContextRow, DeviceRow, FocusPlanRow, FocusSessionRow, StreamRow};
use sunrise_domain::rrule::RRule;
use sunrise_domain::{
    break_after, materialization_horizon_days, ActivityEvent, DailyReview, Energy, FocusKind,
    FocusStats, ReviewSnapshot, Routine, Segment, SessionLength, Task, TaskTemplate, Trends,
    UnblockCascade, WeeklyReview,
};
use sunrise_id::EntityRef;
use sunrise_sync::SyncState;

/// Compact live-sync indicator rendered on the right of the status line.
///
/// A `None` value on [`ViewState::sync`] hides the indicator entirely — that
/// is the default (unit/render tests and any surface that hasn't wired sync).
/// The binary always sets it: to [`SyncIndicator::off`] when `SUNRISE_SYNC_URL`
/// is unset, otherwise to [`SyncIndicator::live`] with the driver's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncIndicator {
    /// `None` = sync off (no URL configured); `Some(state)` = live driver state.
    pub state: Option<SyncState>,
    /// Persisted outbox depth (unacked local ops).
    pub pending: u32,
}

impl SyncIndicator {
    /// The "off" indicator (no `SUNRISE_SYNC_URL`). Still surfaces the pending
    /// outbox depth so a user knows unsynced local work exists.
    #[must_use]
    pub const fn off(pending: u32) -> Self {
        Self {
            state: None,
            pending,
        }
    }

    /// Live-driver indicator from a [`SyncState`] plus the outbox depth.
    #[must_use]
    pub const fn live(state: SyncState, pending: u32) -> Self {
        Self {
            state: Some(state),
            pending,
        }
    }

    /// Compact label: `live` | `catching-up` | `disconnected` | `off`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self.state {
            None => "off",
            Some(SyncState::Live) => "live",
            Some(SyncState::CatchingUp) => "catching-up",
            Some(SyncState::Disconnected) => "disconnected",
        }
    }

    /// Full status text, e.g. `sync: live (0 pending)`.
    #[must_use]
    pub fn text(self) -> String {
        format!("sync: {} ({} pending)", self.label(), self.pending)
    }
}

/// Rows a list is assumed to show before the runtime has measured the real
/// terminal — the body height of the 80x24 minimum, less the chrome.
pub const DEFAULT_VIEWPORT_ROWS: usize = 20;

/// Primary views per the parity matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Today: scheduled blocks + due-today tasks + manually-pulled tasks.
    Today,
    /// Inbox: unassigned tasks awaiting triage.
    Inbox,
    /// Per-Stream view (target stream id is part of [`ViewState`]).
    Stream,
    /// Free-text search.
    Search,
    /// Focus mode: one-task fullscreen.
    Focus,
    /// Routines: recurring templates, with full CRUD.
    Routines,
    /// Review: the weekly review flow, the daily glance, the trends and the
    /// saved-snapshot history.
    Review,
}

/// Which pane of the Browse view has keyboard focus.
///
/// `docs/07-clients/tui.md` draws the sidebar with **two** lists — Streams
/// above, Contexts below — because those are the two axes the domain has:
/// Streams partition the work and Contexts cut across it. A sidebar with only
/// Streams leaves half the model unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPane {
    /// Sidebar, top: the stream list.
    Streams,
    /// Sidebar, bottom: the context list.
    Contexts,
    /// Right pane: tasks of whichever sidebar row is selected.
    Tasks,
}

impl StreamPane {
    /// The next pane in Tab order.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Streams => Self::Contexts,
            Self::Contexts => Self::Tasks,
            Self::Tasks => Self::Streams,
        }
    }

    /// Whether this pane is one of the two sidebar lists.
    #[must_use]
    pub const fn is_sidebar(self) -> bool {
        matches!(self, Self::Streams | Self::Contexts)
    }
}

/// What the Browse view's task pane is currently listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseTarget {
    /// One Stream's tasks.
    Stream(EntityRef),
    /// Every task carrying one Context, across all Streams.
    Context(EntityRef),
}

/// A pending prompt occupying the shared input line (or, for
/// [`Prompt::ConfirmDelete`], the confirmation gate). Exactly one prompt can
/// be active; [`crate::runtime::apply_action`] consumes it on Submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// Capture a new task (`c`).
    Capture,
    /// Capture a mid-session thought (`a`). Committed through
    /// `Core::capture_aside`, so it lands in the Inbox rather than in the
    /// focused task's stream.
    CaptureAside,
    /// Edit an existing task's title (`e`).
    EditTitle(EntityRef),
    /// Defer one or more tasks by a typed offset such as `2h` / `3d` (`d`).
    /// Carries a list rather than a single id so visual-mode bulk defer and
    /// single-task defer share one code path.
    Defer(Vec<EntityRef>),
    /// Schedule one or more tasks at a typed when-expression (`s`), parsed by
    /// [`sunrise_domain::capture::parse_when`].
    Schedule(Vec<EntityRef>),
    /// Delete something, gated on an explicit `y` (`D`).
    ConfirmDelete {
        /// What to delete once confirmed.
        target: DeleteTarget,
        /// Title (or `"N tasks"`) echoed in the confirmation message.
        title: String,
    },
    /// Rename a Stream or Context in place (`e` in the Browse sidebar).
    Rename(SidebarRow),
    /// Create a stream by name (`S`).
    CreateStream,
    /// Create a context by name (`C`).
    CreateContext,
    /// Create a routine (`R`): a capture line, a `|`, and a recurrence.
    CreateRoutine,
    /// Change an existing routine's recurrence (`e` in the Routines view).
    EditRecurrence(EntityRef),
    /// Rename an existing routine's template title (`E` in the Routines view).
    RenameRoutine(EntityRef),
    /// Free-text search query (`/`).
    Search,
    /// Annotate one or more tasks with the edit grammar (`A`) — see
    /// [`crate::edit`]. Carries a list so a marked or visual set is one
    /// prompt, not one per task.
    Annotate(Vec<EntityRef>),
}

/// One entity's activity feed, as shown by the `L` overlay.
///
/// `docs/08-features/reviews-and-stats.md` §Activity timeline: "Useful for
/// 'what happened?' not for analytics." The core folds it and no client asked.
#[derive(Debug, Clone)]
pub struct ActivityFeed {
    /// What the feed is about, for the overlay title.
    pub title: String,
    /// Events, newest first.
    pub events: Vec<ActivityEvent>,
    /// First visible row — the feed of a long-lived task outgrows an overlay.
    pub scroll: usize,
}

impl ActivityFeed {
    /// Scroll, clamped so the last page stays on screen.
    pub fn scroll_by(&mut self, delta: isize, page: usize) {
        let max = self.events.len().saturating_sub(page);
        let want = isize::try_from(self.scroll)
            .unwrap_or(0)
            .saturating_add(delta);
        self.scroll = usize::try_from(want.max(0)).unwrap_or(0).min(max);
    }
}

/// Which panel of the Review view is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPane {
    /// The five-step weekly review.
    Weekly,
    /// The 60-second daily glance.
    Daily,
    /// Twelve-week completed / deferred / created trends.
    Trends,
    /// Saved review snapshots, newest first.
    History,
}

impl ReviewPane {
    /// Next panel in Tab order.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Weekly => Self::Daily,
            Self::Daily => Self::Trends,
            Self::Trends => Self::History,
            Self::History => Self::Weekly,
        }
    }

    /// Tab label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Weekly => "Weekly",
            Self::Daily => "Daily",
            Self::Trends => "Trends",
            Self::History => "History",
        }
    }
}

/// Everything the Review view reads.
///
/// Every field is a query result held verbatim: the review, the glance, the
/// trend fold and the saved snapshots are all assembled by
/// `sunrise_domain`, so nothing here recomputes a number the core already
/// decided. That is what stops the screen and a snapshot saved from it
/// disagreeing.
#[derive(Debug, Clone, Default)]
pub struct ReviewState {
    /// Which panel is showing.
    pub pane: ReviewPane,
    /// The assembled weekly review for [`Self::week_start_ms`].
    pub weekly: Option<Box<WeeklyReview>>,
    /// The daily glance.
    pub daily: Option<Box<DailyReview>>,
    /// The trend fold.
    pub trends: Option<Box<Trends>>,
    /// Saved snapshots, newest window first.
    pub history: Vec<ReviewSnapshot>,
    /// Which week is under review; `None` is the week containing "now".
    /// Moved by `[` and `]`, which is the whole of "review last week".
    pub week_start_ms: Option<u64>,
    /// First visible row of the current panel.
    pub scroll: usize,
    /// Furthest [`Self::scroll`] may go: total panel rows less one screenful,
    /// refreshed by the runtime from the real terminal height. Clamping to a
    /// guess would either strand the last lines off-screen or let the panel
    /// scroll into empty space.
    pub max_scroll: usize,
}

impl Default for ReviewPane {
    fn default() -> Self {
        Self::Weekly
    }
}

impl ReviewState {
    /// Move to the previous or next week, keeping `None` meaning "this week"
    /// until the user actually steps away from it.
    pub fn shift_week(&mut self, weeks: i64) {
        /// One civil week in milliseconds. The review's own window is the
        /// authority on where a week starts; this only moves between them.
        const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
        let base = self
            .week_start_ms
            .or_else(|| self.weekly.as_ref().map(|w| w.window.start_ms));
        let Some(base) = base else { return };
        let shifted = i64::try_from(base)
            .unwrap_or(0)
            .saturating_add(weeks * WEEK_MS);
        self.week_start_ms = Some(u64::try_from(shifted.max(0)).unwrap_or(0));
        self.scroll = 0;
    }

    /// Scroll the current panel, clamped to what it actually rendered.
    pub fn scroll_by(&mut self, delta: isize) {
        let want = isize::try_from(self.scroll)
            .unwrap_or(0)
            .saturating_add(delta);
        self.scroll = usize::try_from(want.max(0))
            .unwrap_or(0)
            .min(self.max_scroll);
    }
}

/// A row of the Browse sidebar, identified by kind so one prompt can serve
/// both lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarRow {
    /// A Stream row.
    Stream(EntityRef),
    /// A Context row.
    Context(EntityRef),
}

/// What a confirmed `D` removes.
///
/// One prompt rather than three: the confirmation gate, its wording and its
/// `y`-only key handling are identical whatever is being deleted, and three
/// copies of a destructive path is three chances for one of them to lose the
/// gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    /// Tasks (one, a visual run, or the marked set).
    Tasks(Vec<EntityRef>),
    /// A whole Stream.
    Stream(EntityRef),
    /// A Context — removed from every Task carrying it, per the spec.
    Context(EntityRef),
    /// A Routine template. Existing generated tasks survive.
    Routine(EntityRef),
}

/// State for the modal move-to-stream picker (`m`).
///
/// Not `PartialEq`: `StreamRow` (from the core's query surface) isn't.
#[derive(Debug, Clone)]
pub struct StreamPicker {
    /// Tasks being moved (one row normally, the whole visual run under `V`).
    pub tasks: Vec<EntityRef>,
    /// Task title (or `"N tasks"`), echoed in the picker header.
    pub task_title: String,
    /// Candidate destinations (`Query::StreamList`, Inbox first).
    pub rows: Vec<StreamRow>,
    /// Highlighted row.
    pub selected: usize,
}

impl StreamPicker {
    /// Move the picker cursor down, wrapping.
    pub fn next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1) % self.rows.len();
        }
    }

    /// Move the picker cursor up, wrapping.
    pub fn prev(&mut self) {
        if !self.rows.is_empty() {
            self.selected = if self.selected == 0 {
                self.rows.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// Move the picker cursor by `delta`, clamping at both ends.
    pub fn nav_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let want = isize::try_from(self.selected)
            .unwrap_or(0)
            .saturating_add(delta);
        self.selected = usize::try_from(want.max(0)).unwrap_or(0).min(last);
    }

    /// The highlighted destination stream, if any.
    #[must_use]
    pub fn selected_row(&self) -> Option<&StreamRow> {
        self.rows.get(self.selected)
    }
}

/// Everything the Focus view reads about focus **sessions**
/// (`docs/08-features/focus-mode.md`).
///
/// Every field here is *read through* from the core's queries. In particular
/// there is no elapsed/remaining field and no tick counter: a running
/// session's numbers are derived on demand from
/// [`sunrise_domain::FocusSession::elapsed_ms`] against
/// [`ViewState::now_ms`], which is what
/// [ADR-0013](../../../docs/11-adr/0013-focus-session-op-representation.md)
/// means by "nothing ticking is ever persisted".
///
/// Not `PartialEq`: the core's query rows aren't.
#[derive(Debug, Clone)]
pub struct FocusState {
    /// The session with a `start` and no `end` (`Query::RunningFocusSessions`).
    /// `None` means no session is running — including right after a crash,
    /// where a dangling start would instead read as *still running*.
    pub running: Option<FocusSessionRow>,
    /// The ranked planner queue (`Query::FocusPlan`), best pick first.
    pub plan: Vec<FocusPlanRow>,
    /// Cursor into [`Self::plan`].
    pub selected: Option<usize>,
    /// Sessions recorded against the focused task
    /// (`Query::TaskFocusSessions`), newest first. Read-only input to
    /// [`Self::work_sessions_done`].
    pub sessions: Vec<FocusSessionRow>,
    /// Folded focus totals + calibration (`:focus stats`), shown as an
    /// overlay. `None` hides it.
    pub stats: Option<Box<FocusStats>>,
    /// What the last mid-session completion released. Informational; it keeps
    /// no score.
    pub cascade: Option<CascadeReport>,
    /// The energy budget the next session declares (`:focus energy`). `None`
    /// means "no signal", which drops energy out of the planner ranking.
    pub energy: Option<Energy>,
    /// How the next session is sized (`:focus length`).
    pub length: SessionLength,
}

impl Default for FocusState {
    fn default() -> Self {
        Self {
            running: None,
            plan: Vec::new(),
            selected: None,
            sessions: Vec::new(),
            stats: None,
            cascade: None,
            energy: None,
            // Sized to the task's own estimate, chunked when it exceeds one
            // sitting — the choice that uses the data the user already has.
            length: SessionLength::SizedToEstimate,
        }
    }
}

impl FocusState {
    /// Whether a session is running right now.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The running session's own id (`fcs_`), for `EndFocus` /
    /// `LogInterruption`.
    #[must_use]
    pub fn running_session(&self) -> Option<EntityRef> {
        self.running.as_ref().map(|r| r.session.start.id)
    }

    /// The task the running session is on.
    #[must_use]
    pub fn running_task(&self) -> Option<EntityRef> {
        self.running.as_ref().map(|r| r.session.start.task_id)
    }

    /// Whether the running session is a break rather than a work segment.
    #[must_use]
    pub fn on_break(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|r| r.session.start.kind == FocusKind::Break)
    }

    /// Wall-clock span of the running session, **derived from `now_ms`** —
    /// never a number this struct carries.
    #[must_use]
    pub fn elapsed_ms(&self, now_ms: u64) -> Option<u64> {
        self.running.as_ref().map(|r| r.session.elapsed_ms(now_ms))
    }

    /// Time left against the plan; the inner `None` is an open-ended
    /// (`until done`) session.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: u64) -> Option<Option<u64>> {
        self.running
            .as_ref()
            .map(|r| r.session.remaining_ms(now_ms))
    }

    /// The highlighted planner row.
    #[must_use]
    pub fn selected_plan(&self) -> Option<&FocusPlanRow> {
        self.selected.and_then(|i| self.plan.get(i))
    }

    /// Work segments already **finished** against the focused task, counted
    /// from the session log. Derived on read; the TUI never increments a
    /// counter of its own, which is what keeps the cycle correct across a
    /// restart or a session started on another device.
    #[must_use]
    pub fn work_sessions_done(&self) -> u32 {
        let n = self
            .sessions
            .iter()
            .filter(|r| !r.running && r.session.start.kind == FocusKind::Work)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// The segment the pomodoro cycle owes once the current one is done —
    /// [`sunrise_domain::break_after`], not arithmetic repeated here. The
    /// work segment currently running counts toward the cycle, so the fourth
    /// one is followed by the long break.
    #[must_use]
    pub fn next_segment(&self) -> Segment {
        let running_work = self
            .running
            .as_ref()
            .is_some_and(|r| r.session.start.kind == FocusKind::Work);
        break_after(
            self.work_sessions_done()
                .saturating_add(u32::from(running_work)),
        )
    }

    /// Point the cursor at `focused`'s row when the queue reloads, so a
    /// refresh under the user's hand does not move the pick.
    pub fn after_plan_loaded(&mut self, focused: Option<EntityRef>) {
        self.selected = match focused.and_then(|id| self.plan.iter().position(|r| r.task.id == id))
        {
            Some(i) => Some(i),
            None => clamp_selection(self.plan.len(), self.selected),
        };
    }
}

/// What a mid-session completion released
/// (`docs/08-features/focus-mode.md` §Unblock cascade), with the released
/// tasks' titles resolved by the runtime.
///
/// Informational by construction: it names what moved and keeps no score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeReport {
    /// The graph frontier the core recomputed.
    pub cascade: UnblockCascade,
    /// Titles of [`UnblockCascade::released`], in the same order. May be
    /// shorter than `released` when the runtime capped the lookups.
    pub released: Vec<String>,
}

impl CascadeReport {
    /// One informational line: what this completion released, and how much is
    /// still waiting on something else.
    #[must_use]
    pub fn line(&self) -> String {
        if self.cascade.is_empty() {
            return "nothing was waiting on it".into();
        }
        let mut parts = vec![if self.released.is_empty() {
            "released nothing yet".to_string()
        } else {
            let names = self
                .released
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("released {names}")
        }];
        if !self.cascade.still_blocked.is_empty() {
            parts.push(format!(
                "{} still waiting on something else",
                self.cascade.still_blocked.len()
            ));
        }
        parts.join(" · ")
    }
}

/// Human label for a pomodoro segment.
#[must_use]
pub const fn segment_label(s: Segment) -> &'static str {
    match s {
        Segment::Work => "work",
        Segment::ShortBreak => "short break",
        Segment::LongBreak => "long break",
    }
}

/// Human label for an energy budget; `None` reads as "any", the value that
/// drops energy out of the planner ranking.
#[must_use]
pub const fn energy_budget_label(e: Option<Energy>) -> &'static str {
    match e {
        None => "any",
        Some(Energy::Low) => "low",
        Some(Energy::Med) => "med",
        Some(Energy::High) => "high",
    }
}

/// Human label for a session-length choice.
#[must_use]
pub const fn length_label(l: SessionLength) -> &'static str {
    match l {
        SessionLength::OnePomodoro => "one pomodoro",
        SessionLength::SizedToEstimate => "sized to estimate",
        SessionLength::UntilDone => "until done",
    }
}

/// `MM:SS`, widening to `H:MM:SS` past an hour. Used for every duration the
/// Focus view shows, so a timer and a total read the same way.
#[must_use]
pub fn fmt_duration_ms(ms: u64) -> String {
    let total_s = ms / 1000;
    let (h, m, s) = (total_s / 3600, (total_s % 3600) / 60, total_s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// One row of the Routines view: the routine's template title, a
/// human-readable RRULE summary, and its next occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineRow {
    /// Routine id.
    pub id: EntityRef,
    /// Template title.
    pub title: String,
    /// Human-readable recurrence summary (e.g. `every 2 weeks on Mo, We`).
    pub rrule: String,
    /// Next occurrence at or after "now", if one exists inside the routine's
    /// materialization horizon.
    pub next: Option<Timestamp>,
    /// Whether the routine is paused (no occurrences are generated).
    pub paused: bool,
    /// The routine's task template, kept so an edit patches the fields the
    /// user changed and leaves the rest exactly as they were —
    /// `RoutinePatch.template` replaces the whole template, so editing a title
    /// without it would silently drop the stream, priority and contexts.
    pub template: TaskTemplate,
    /// The parsed recurrence, kept for the same reason: the summary string is
    /// lossy and cannot be patched back.
    pub rule: RRule,
    /// Current streak counter, so the Routines view can answer "am I keeping
    /// this up?" without a second query per row.
    pub streak: i64,
}

/// Human-readable one-line summary of an [`RRule`], for the Routines view.
#[must_use]
pub fn rrule_summary(r: &RRule) -> String {
    use std::fmt::Write as _;
    use sunrise_domain::rrule::Frequency;
    let unit = match r.freq {
        Frequency::Daily => "day",
        Frequency::Weekly => "week",
        Frequency::Monthly => "month",
        Frequency::Yearly => "year",
    };
    let mut s = if r.interval <= 1 {
        format!("every {unit}")
    } else {
        format!("every {} {unit}s", r.interval)
    };
    if !r.by_day.is_empty() {
        let days: Vec<String> = r.by_day.iter().map(|d| format!("{d:?}")).collect();
        s.push_str(" on ");
        s.push_str(&days.join(", "));
    }
    if !r.by_month_day.is_empty() {
        let days: Vec<String> = r.by_month_day.iter().map(ToString::to_string).collect();
        s.push_str(" day ");
        s.push_str(&days.join(", "));
    }
    if let Some(c) = r.count {
        let _ = write!(s, " ×{c}");
    }
    if let Some(u) = r.until {
        let _ = write!(s, " until {u}");
    }
    s
}

/// Project `Query::Routines` output into [`RoutineRow`]s, resolving each
/// routine's next occurrence at or after `now`.
///
/// The lookahead window is the routine's own per-FREQ materialization horizon
/// (`docs/02-domain/routines-and-recurrence.md`), so a yearly routine still
/// resolves while a daily one stays cheap. Pure over `now` — no wall clock is
/// read here, which keeps the projection unit-testable.
#[must_use]
pub fn routine_rows(routines: &[Routine], now: Timestamp) -> Vec<RoutineRow> {
    routines
        .iter()
        .map(|r| {
            let horizon_h = i64::from(materialization_horizon_days(r.rrule.freq)) * 24;
            let next = now
                .checked_add(jiff::SignedDuration::from_hours(horizon_h))
                .ok()
                .and_then(|end| r.occurrences_in((now, end)).ok())
                .and_then(|occ| occ.first().map(|o| o.at));
            RoutineRow {
                id: r.id,
                title: r.template.title.clone(),
                rrule: rrule_summary(&r.rrule),
                next,
                paused: r.paused,
                template: r.template.clone(),
                rule: r.rrule.clone(),
                streak: r.streak_counter,
            }
        })
        .collect()
}

/// Owning struct for the active view.
///
/// The several `bool` flags are independent toggles over the *same* state
/// (vim-mode is a preference, the `gg` latch is a chord, the help overlay and
/// the triage pass are modes of presentation); folding them into one enum would
/// force combinations that cannot occur to be spelled out, and combinations
/// that can occur — help open *during* a triage pass — to be impossible.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct ViewState {
    /// Current view.
    pub view: View,
    /// Vim-mode toggle. Defaults to true on TUI per spec.
    pub vim_mode: bool,
    /// Current keymap mode.
    pub mode: Mode,
    /// Tasks currently visible in the view (refreshed after each command).
    pub tasks: Vec<Task>,
    /// Index of the selected task in `tasks`. `None` if the list is empty.
    pub selected: Option<usize>,
    /// Stream rows for the Stream view (Inbox first, per `Query::StreamList`).
    pub streams: Vec<StreamRow>,
    /// Context rows (`Query::Contexts`), the candidate set `@name` resolves
    /// against during capture. Refreshed alongside `streams`.
    pub contexts: Vec<ContextRow>,
    /// Index of the selected stream in `streams`. `None` if the list is empty.
    pub selected_stream: Option<usize>,
    /// Index of the selected context in `contexts`. `None` if empty.
    pub selected_context: Option<usize>,
    /// Which sidebar list the Browse task pane is following. Held separately
    /// from the pane focus so moving the cursor into the *other* sidebar list
    /// does not silently retarget the tasks on the right until Enter.
    pub browse: Option<BrowseTarget>,
    /// Focused pane in the Browse view.
    pub pane: StreamPane,
    /// Which of the two sidebar lists focus returns to from the task pane.
    /// Remembered so `l` then `h` is a round trip rather than a reset. Set
    /// through [`ViewState::focus_pane`].
    pub last_sidebar: StreamPane,
    /// Task shown fullscreen in the Focus view.
    pub focused_task: Option<Task>,
    /// View to return to when Focus is closed with Esc.
    pub prev_view: Option<View>,
    /// Active text input (capture buffer / search query), with its caret.
    pub input: InputLine,
    /// One-line status / error displayed at the bottom of every view.
    pub status: String,
    /// Live-sync indicator shown on the right of the status line. `None` hides
    /// it; the binary sets it every frame from `Core::query(SyncStatus)`.
    pub sync: Option<SyncIndicator>,
    /// Rows of the Routines view (`Query::Routines`, projected at refresh time).
    pub routines: Vec<RoutineRow>,
    /// Index of the selected routine. `None` if the list is empty.
    pub selected_routine: Option<usize>,
    /// Prompt currently occupying the input line / confirmation gate.
    pub prompt: Option<Prompt>,
    /// Active move-to-stream picker overlay.
    pub picker: Option<StreamPicker>,
    /// Whether the `?` help overlay is visible.
    pub show_help: bool,
    /// First help row shown, so the overlay can scroll. The Normal-mode
    /// section alone is taller than a minimum-size terminal, and an overlay
    /// that silently clips half the keys documents nothing.
    pub help_scroll: usize,
    /// Paired devices listed by `:devices`, shown as an overlay. `None` hides
    /// it; dismissed by the next keypress like the help overlay.
    pub devices: Option<Vec<DeviceRow>>,
    /// One entity's activity feed (`L`), newest first. `None` hides it.
    pub activity: Option<ActivityFeed>,
    /// Latch for the `gg` chord: set by the first `g`, cleared by anything else.
    pub pending_g: bool,
    /// Visual-mode anchor: the row `V` was pressed on. The selection is the
    /// inclusive run between it and [`Self::selected`].
    pub visual_anchor: Option<usize>,
    /// Rows marked with `Space` — a **non-contiguous** multi-selection, held
    /// by id rather than by index so a refresh that reorders or shortens the
    /// list cannot silently re-target an operator
    /// (`docs/08-features/keyboard.md`: "Multi-select toggle — Space").
    pub marked: Vec<EntityRef>,
    /// Whether Inbox triage mode is running (`t`).
    pub triage: bool,
    /// Live capture preview: the parser's structured reading of
    /// [`Self::input`], recomputed on every keystroke while the capture prompt
    /// is open (`docs/08-features/inbox-and-capture.md`).
    pub capture_preview: Option<String>,
    /// Time zone relative dates are resolved in. Held here rather than read
    /// from the host inside the reducer, so the reducer stays pure and tests
    /// can pin the zone. The binary sets it to `TimeZone::system()`.
    pub tz: jiff::tz::TimeZone,
    /// Active keymap, including any `~/.config/sunrise/keys.toml` overrides.
    pub keymap: Keymap,
    /// Focus-session state: the running session, the planner queue, the
    /// folded stats. See [`FocusState`].
    pub focus: FocusState,
    /// Review-view state: the weekly review, the glance, the trends, the
    /// saved snapshots. See [`ReviewState`].
    pub review: ReviewState,
    /// Visible rows in the focused list, refreshed once per frame by the
    /// binary from the real terminal size. Drives the page-jump keys; a
    /// default is kept so the reducer is usable with no terminal at all.
    pub viewport_rows: usize,
    /// Last reading of the injected clock (`Core::now_ms`), refreshed once per
    /// frame by the binary.
    ///
    /// **Not a tick counter.** Nothing in the TUI increments it; it is the
    /// clock value the render pass hands to
    /// [`sunrise_domain::FocusSession::elapsed_ms`] so a running timer is
    /// derived rather than accumulated.
    pub now_ms: u64,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            view: View::Today,
            vim_mode: true,
            mode: Mode::Normal,
            tasks: Vec::new(),
            selected: None,
            streams: Vec::new(),
            contexts: Vec::new(),
            selected_stream: None,
            selected_context: None,
            browse: None,
            pane: StreamPane::Streams,
            last_sidebar: StreamPane::Streams,
            focused_task: None,
            prev_view: None,
            input: InputLine::new(),
            status: String::new(),
            sync: None,
            routines: Vec::new(),
            selected_routine: None,
            prompt: None,
            picker: None,
            show_help: false,
            help_scroll: 0,
            devices: None,
            activity: None,
            pending_g: false,
            visual_anchor: None,
            marked: Vec::new(),
            triage: false,
            capture_preview: None,
            tz: jiff::tz::TimeZone::UTC,
            keymap: Keymap::default(),
            focus: FocusState::default(),
            review: ReviewState::default(),
            viewport_rows: DEFAULT_VIEWPORT_ROWS,
            now_ms: 0,
        }
    }
}

impl ViewState {
    /// Apply selection bookkeeping after `tasks` has changed.
    pub fn after_tasks_loaded(&mut self) {
        self.selected = clamp_selection(self.tasks.len(), self.selected);
        // A visual run over rows that no longer exist would operate on the
        // wrong tasks, and a triage pass over an empty Inbox has nothing left
        // to decide.
        self.visual_anchor = self
            .visual_anchor
            .and_then(|a| clamp_selection(self.tasks.len(), Some(a)));
        if self.triage && self.tasks.is_empty() {
            self.exit_triage();
            self.status = "triage complete".into();
        }
    }

    /// Apply selection bookkeeping after `streams` has changed. Defaults the
    /// selection to the first row (Inbox — `StreamList` returns it first).
    pub fn after_streams_loaded(&mut self) {
        self.selected_stream = clamp_selection(self.streams.len(), self.selected_stream);
    }

    /// Apply selection bookkeeping after `contexts` has changed.
    pub fn after_contexts_loaded(&mut self) {
        self.selected_context = clamp_selection(self.contexts.len(), self.selected_context);
    }

    /// The sidebar row the task pane is following, defaulting to the selected
    /// Stream (Inbox first) before the user has chosen anything.
    #[must_use]
    pub fn browse_target(&self) -> Option<BrowseTarget> {
        self.browse.or_else(|| {
            self.selected_stream_row()
                .map(|r| BrowseTarget::Stream(r.id))
        })
    }

    /// Selected context row, if any.
    #[must_use]
    pub fn selected_context_row(&self) -> Option<&ContextRow> {
        self.selected_context.and_then(|i| self.contexts.get(i))
    }

    /// The Browse sidebar row under the cursor, with its display name.
    ///
    /// `None` outside the Browse sidebar, which is what makes `e`, `D`, `a`
    /// and `p` mean the sidebar row *there* and the task everywhere else,
    /// without needing keys of their own.
    #[must_use]
    pub fn sidebar_row(&self) -> Option<(SidebarRow, String)> {
        if self.view != View::Stream {
            return None;
        }
        match self.pane {
            StreamPane::Streams => self
                .selected_stream_row()
                .map(|r| (SidebarRow::Stream(r.id), r.name.clone())),
            StreamPane::Contexts => self
                .selected_context_row()
                .map(|r| (SidebarRow::Context(r.id), format!("@{}", r.name))),
            StreamPane::Tasks => None,
        }
    }

    /// Whether the selected Stream row is paused.
    #[must_use]
    pub fn paused_stream_selected(&self) -> bool {
        self.selected_stream_row().is_some_and(|r| r.paused)
    }

    /// Whether the selected sidebar row is archived.
    #[must_use]
    pub fn sidebar_row_archived(&self) -> bool {
        match self.pane {
            StreamPane::Streams => self.selected_stream_row().is_some_and(|r| r.archived),
            StreamPane::Contexts => self.selected_context_row().is_some_and(|r| r.archived),
            StreamPane::Tasks => false,
        }
    }

    /// Point the task pane at the sidebar row under the cursor, without
    /// moving focus.
    ///
    /// Called on every sidebar cursor move so the right-hand pane follows the
    /// selection live: a sidebar you have to press Enter in to see anything is
    /// a sidebar you cannot browse with.
    ///
    /// Returns whether the target changed, so the caller only pays for a
    /// re-query when it did.
    pub fn sync_browse_from_cursor(&mut self) -> bool {
        let target = match self.pane {
            StreamPane::Streams => self
                .selected_stream_row()
                .map(|r| BrowseTarget::Stream(r.id)),
            StreamPane::Contexts => self
                .selected_context_row()
                .map(|r| BrowseTarget::Context(r.id)),
            StreamPane::Tasks => None,
        };
        match target {
            Some(t) if self.browse != Some(t) => {
                self.browse = Some(t);
                // The task cursor belonged to the previous list.
                self.selected = None;
                true
            }
            _ => false,
        }
    }

    /// Point the Browse task pane at whichever sidebar row is under the cursor.
    pub fn open_sidebar_row(&mut self) -> bool {
        let target = match self.pane {
            StreamPane::Streams => self
                .selected_stream_row()
                .map(|r| BrowseTarget::Stream(r.id)),
            StreamPane::Contexts => self
                .selected_context_row()
                .map(|r| BrowseTarget::Context(r.id)),
            StreamPane::Tasks => None,
        };
        match target {
            Some(t) => {
                self.browse = Some(t);
                self.pane = StreamPane::Tasks;
                true
            }
            None => false,
        }
    }

    /// Title for the Browse task pane: the Stream's name, or `@context`.
    #[must_use]
    pub fn browse_title(&self) -> String {
        match self.browse_target() {
            Some(BrowseTarget::Stream(id)) => self
                .streams
                .iter()
                .find(|s| s.id == id)
                .map_or_else(|| "Tasks".into(), |s| s.name.clone()),
            Some(BrowseTarget::Context(id)) => self
                .contexts
                .iter()
                .find(|c| c.id == id)
                .map_or_else(|| "Tasks".into(), |c| format!("@{}", c.name)),
            None => "Tasks".into(),
        }
    }

    /// Move selection down by one, wrapping at the end.
    ///
    /// Visual mode clamps instead of wrapping: wrapping past the last row
    /// would flip the run to the other side of the anchor and silently retarget
    /// a bulk operation.
    pub fn select_next(&mut self) {
        self.selected = if self.visual_anchor.is_some() {
            clamp_next(self.tasks.len(), self.selected)
        } else {
            wrap_next(self.tasks.len(), self.selected)
        };
    }

    /// Move selection up by one, wrapping at the start (clamping in visual
    /// mode — see [`Self::select_next`]).
    pub fn select_prev(&mut self) {
        self.selected = if self.visual_anchor.is_some() {
            clamp_prev(self.tasks.len(), self.selected)
        } else {
            wrap_prev(self.tasks.len(), self.selected)
        };
    }

    /// Move the stream selection down by one, wrapping at the end.
    pub fn stream_next(&mut self) {
        self.selected_stream = wrap_next(self.streams.len(), self.selected_stream);
    }

    /// Move the stream selection up by one, wrapping at the start.
    pub fn stream_prev(&mut self) {
        self.selected_stream = wrap_prev(self.streams.len(), self.selected_stream);
    }

    /// Scroll the help overlay by `delta` rows, clamped to `max_scroll`.
    pub fn scroll_help(&mut self, delta: isize, max_scroll: usize) {
        let want = isize::try_from(self.help_scroll)
            .unwrap_or(0)
            .saturating_add(delta);
        self.help_scroll = usize::try_from(want.max(0)).unwrap_or(0).min(max_scroll);
    }

    /// Apply selection bookkeeping after `routines` has changed.
    pub fn after_routines_loaded(&mut self) {
        self.selected_routine = clamp_selection(self.routines.len(), self.selected_routine);
    }

    /// Which list the cursor keys drive right now.
    #[must_use]
    const fn active_list(&self) -> ActiveList {
        match self.view {
            View::Routines => ActiveList::Routines,
            View::Stream if matches!(self.pane, StreamPane::Streams) => ActiveList::Streams,
            View::Stream if matches!(self.pane, StreamPane::Contexts) => ActiveList::Contexts,
            // The Focus view's list is the planner queue: `j`/`k`/`gg`/`G`
            // pick what to work on next without needing keys of their own.
            View::Focus => ActiveList::FocusPlan,
            // The Review view is a page, not a list: the cursor keys scroll it.
            View::Review => ActiveList::Review,
            _ => ActiveList::Tasks,
        }
    }

    /// Cursor-down in whichever list has keyboard focus (Stream view is
    /// pane-aware; every other view navigates the task list).
    pub fn nav_next(&mut self) {
        match self.active_list() {
            ActiveList::Streams => self.stream_next(),
            ActiveList::Contexts => {
                self.selected_context = wrap_next(self.contexts.len(), self.selected_context);
            }
            ActiveList::Review => self.review.scroll_by(1),
            ActiveList::Routines => {
                self.selected_routine = wrap_next(self.routines.len(), self.selected_routine);
            }
            ActiveList::FocusPlan => {
                self.focus.selected = wrap_next(self.focus.plan.len(), self.focus.selected);
                self.sync_focus_pick();
            }
            ActiveList::Tasks => self.select_next(),
        }
    }

    /// Cursor-up counterpart of [`Self::nav_next`].
    pub fn nav_prev(&mut self) {
        match self.active_list() {
            ActiveList::Streams => self.stream_prev(),
            ActiveList::Contexts => {
                self.selected_context = wrap_prev(self.contexts.len(), self.selected_context);
            }
            ActiveList::Review => self.review.scroll_by(-1),
            ActiveList::Routines => {
                self.selected_routine = wrap_prev(self.routines.len(), self.selected_routine);
            }
            ActiveList::FocusPlan => {
                self.focus.selected = wrap_prev(self.focus.plan.len(), self.focus.selected);
                self.sync_focus_pick();
            }
            ActiveList::Tasks => self.select_prev(),
        }
    }

    /// Jump the focused list's cursor to its first row (`gg`).
    pub fn nav_first(&mut self) {
        if matches!(self.active_list(), ActiveList::Review) {
            self.review.scroll = 0;
            return;
        }
        self.nav_to(|_| 0);
    }

    /// Jump the focused list's cursor to its last row (`G`).
    pub fn nav_last(&mut self) {
        if matches!(self.active_list(), ActiveList::Review) {
            self.review.scroll_by(isize::MAX / 2);
            return;
        }
        self.nav_to(|len| len - 1);
    }

    /// Move the focused list's cursor by `delta` rows, clamping at both ends.
    ///
    /// Clamps rather than wraps: a page jump that silently wrapped to the far
    /// end of a long list would lose the user's place entirely, and unlike a
    /// single-step `j` there is no cheap way to tell it happened.
    pub fn nav_by(&mut self, delta: isize) {
        if matches!(self.active_list(), ActiveList::Review) {
            self.review.scroll_by(delta);
            return;
        }
        let current = match self.active_list() {
            ActiveList::Streams => self.selected_stream,
            ActiveList::Contexts => self.selected_context,
            ActiveList::Review => Some(self.review.scroll),
            ActiveList::Routines => self.selected_routine,
            ActiveList::FocusPlan => self.focus.selected,
            ActiveList::Tasks => self.selected,
        }
        .unwrap_or(0);
        self.nav_to(|len| {
            let last = len.saturating_sub(1);
            let want = isize::try_from(current).unwrap_or(0).saturating_add(delta);
            usize::try_from(want.max(0)).unwrap_or(0).min(last)
        });
    }

    /// Rows one page holds — the visible height of the focused list, set by
    /// the runtime from the real terminal size each frame.
    ///
    /// Held on the state rather than passed to the reducer because a page is a
    /// property of the *viewport*, and the reducer must stay a pure function of
    /// state: a test that wants a five-row page sets one.
    #[must_use]
    pub const fn page_rows(&self) -> usize {
        self.viewport_rows
    }

    /// Half a page, at least one row (`^D` / `^U`).
    #[must_use]
    pub const fn half_page_rows(&self) -> usize {
        let half = self.viewport_rows / 2;
        if half == 0 {
            1
        } else {
            half
        }
    }

    /// Shared body of [`Self::nav_first`] / [`Self::nav_last`]: resolve the
    /// focused list's length, then set its cursor (no-op when empty).
    fn nav_to(&mut self, pick: impl Fn(usize) -> usize) {
        let list = self.active_list();
        let len = match list {
            ActiveList::Streams => self.streams.len(),
            ActiveList::Contexts => self.contexts.len(),
            // Handled by `scroll_by`, which clamps to the rendered row count.
            ActiveList::Review => 0,
            ActiveList::Routines => self.routines.len(),
            ActiveList::FocusPlan => self.focus.plan.len(),
            ActiveList::Tasks => self.tasks.len(),
        };
        if len == 0 {
            return;
        }
        let idx = Some(pick(len));
        match list {
            ActiveList::Streams => self.selected_stream = idx,
            ActiveList::Contexts => self.selected_context = idx,
            ActiveList::Review => {}
            ActiveList::Routines => self.selected_routine = idx,
            ActiveList::FocusPlan => {
                self.focus.selected = idx;
                self.sync_focus_pick();
            }
            ActiveList::Tasks => self.selected = idx,
        }
    }

    /// Selected routine row, if any.
    #[must_use]
    pub fn selected_routine_row(&self) -> Option<&RoutineRow> {
        self.selected_routine.and_then(|i| self.routines.get(i))
    }

    /// Open the move-to-stream picker over `rows` for `task`, entering
    /// [`Mode::Picker`]. The cursor starts on the task's current stream so
    /// Enter without navigating is a no-op move rather than a surprise.
    pub fn open_stream_picker(
        &mut self,
        tasks: Vec<EntityRef>,
        title: String,
        rows: Vec<StreamRow>,
    ) {
        let current = tasks
            .first()
            .and_then(|first| self.tasks.iter().find(|t| t.id == *first))
            .map(|t| t.stream_id);
        let selected = current
            .and_then(|s| rows.iter().position(|r| r.id == s))
            .unwrap_or(0);
        self.status = format!("move \"{title}\" to stream — Enter to choose, Esc to cancel");
        self.picker = Some(StreamPicker {
            tasks,
            task_title: title,
            rows,
            selected,
        });
        self.mode = Mode::Picker;
    }

    /// Clear any prompt/picker/help overlay and return to Normal mode.
    ///
    /// Triage mode survives this: a prompt opened *from* triage (schedule,
    /// defer, delete) must hand control back to the triage card, not dump the
    /// user into the plain Inbox mid-pass. [`Self::exit_triage`] is the only
    /// way out.
    pub fn reset_to_normal(&mut self) {
        self.mode = self.resting_mode();
        self.prompt = None;
        self.picker = None;
        self.pending_g = false;
        self.visual_anchor = None;
        self.capture_preview = None;
        self.input.clear();
        self.status.clear();
    }

    /// The inclusive `(first, last)` row range visual mode has selected, if
    /// visual mode is active and the cursor is on a row.
    #[must_use]
    pub fn visual_range(&self) -> Option<(usize, usize)> {
        let (a, b) = (self.visual_anchor?, self.selected?);
        Some((a.min(b), a.max(b)))
    }

    /// Rows currently marked *and* still visible, in list order.
    ///
    /// Filtered against `tasks` on every read rather than pruned on refresh: a
    /// mark on a row that a sync just moved out of this view must not silently
    /// become part of the next bulk operation, but it also must not be
    /// destroyed — coming back to the view restores it.
    #[must_use]
    pub fn marked_ids(&self) -> Vec<EntityRef> {
        self.tasks
            .iter()
            .filter(|t| self.marked.contains(&t.id))
            .map(|t| t.id)
            .collect()
    }

    /// Whether the task at row `i` is marked.
    #[must_use]
    pub fn is_marked(&self, i: usize) -> bool {
        self.tasks
            .get(i)
            .is_some_and(|t| self.marked.contains(&t.id))
    }

    /// Toggle the mark on the selected row and step down, so marking a set is
    /// one key held rather than a key and a motion alternated.
    pub fn toggle_mark(&mut self) -> bool {
        let Some(id) = self.selected_task().map(|t| t.id) else {
            return false;
        };
        match self.marked.iter().position(|m| *m == id) {
            Some(i) => {
                self.marked.remove(i);
            }
            None => self.marked.push(id),
        }
        self.select_next();
        let n = self.marked_ids().len();
        self.status = match n {
            0 => "no rows marked".to_string(),
            1 => "1 marked · operators apply to marked rows".to_string(),
            n => format!("{n} marked · operators apply to marked rows"),
        };
        true
    }

    /// Drop every mark.
    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// Ids the next operator applies to, in precedence order: the marked set,
    /// then the visual run, then the row under the cursor.
    ///
    /// Marks win over the cursor because they are the *explicit* statement —
    /// a user who has ticked four rows and then moves the cursor has not
    /// changed their mind about the four.
    #[must_use]
    pub fn operand_ids(&self) -> Vec<EntityRef> {
        // While a session owns the keyboard the operand is the task being
        // focused on, not whatever row the underlying list cursor sits on.
        if self.mode == Mode::Focus {
            return self.focus.running_task().into_iter().collect();
        }
        let marked = self.marked_ids();
        if !marked.is_empty() {
            return marked;
        }
        match self.visual_range() {
            // Sliced with `get` rather than indexed: a stale range against a
            // list that shrank under a refresh must yield fewer tasks, never a
            // panic in the middle of the user's session.
            Some((lo, hi)) => self
                .tasks
                .get(lo..=hi)
                .or_else(|| self.tasks.get(lo..))
                .unwrap_or_default()
                .iter()
                .map(|t| t.id)
                .collect(),
            None => self.selected_task().map(|t| t.id).into_iter().collect(),
        }
    }

    /// Label for the current operand set: the task's title, or `"N tasks"`.
    #[must_use]
    pub fn operand_label(&self) -> String {
        if self.mode == Mode::Focus {
            return self.focused_title();
        }
        match self.marked_ids().len() {
            0 => {}
            1 => {
                let id = self.marked_ids()[0];
                return self
                    .tasks
                    .iter()
                    .find(|t| t.id == id)
                    .map_or_else(String::new, |t| t.title.clone());
            }
            n => return format!("{n} tasks"),
        }
        match self.visual_range() {
            Some((lo, hi)) if hi > lo => format!("{} tasks", hi - lo + 1),
            _ => self
                .selected_task()
                .map_or_else(String::new, |t| t.title.clone()),
        }
    }

    /// Enter visual mode, anchoring on the current row. No-op with no
    /// selection — there would be nothing to anchor to.
    pub fn enter_visual(&mut self) -> bool {
        // Visual mode selects *tasks*; in the Browse sidebar the cursor keys
        // drive a stream or context list instead, so there is nothing to
        // extend.
        if self.view == View::Stream && self.pane.is_sidebar() {
            return false;
        }
        let Some(i) = self.selected else { return false };
        self.visual_anchor = Some(i);
        self.mode = Mode::Visual;
        self.status = "visual: j/k extend · x done · d defer · D delete · m move".into();
        true
    }

    /// Leave visual mode, dropping the selection.
    pub fn exit_visual(&mut self) {
        self.visual_anchor = None;
        self.mode = Mode::Normal;
        self.status.clear();
    }

    /// Switch to `view`, dropping marks made in the one being left.
    ///
    /// Marks are per-list: carrying them across would let an operator run in
    /// the Inbox on rows the user ticked in Today, with nothing on screen
    /// saying so.
    pub fn switch_view(&mut self, view: View) {
        if view != self.view {
            self.clear_marks();
        }
        self.view = view;
    }

    /// Enter Inbox triage: switch to the Inbox and present its first task.
    pub fn enter_triage(&mut self) {
        self.view = View::Inbox;
        self.triage = true;
        self.visual_anchor = None;
        self.mode = Mode::Triage;
        self.selected = if self.tasks.is_empty() { None } else { Some(0) };
    }

    /// Leave triage mode.
    pub fn exit_triage(&mut self) {
        self.triage = false;
        self.mode = Mode::Normal;
        self.visual_anchor = None;
    }

    /// Move to the next task in a triage pass, ending the pass when the Inbox
    /// runs out.
    ///
    /// `consumed` says whether the decision removed the task from the Inbox
    /// (promote / delete / complete): the list shrinks under the cursor, so
    /// holding the index *is* advancing. Decisions that leave the task in
    /// place (keep / schedule / defer) step forward instead, or the same card
    /// would come back forever.
    pub fn triage_advance(&mut self, consumed: bool) {
        if !consumed {
            self.selected = self.selected.map(|i| i + 1);
        }
        let remaining = self.tasks.len().saturating_sub(usize::from(consumed));
        if self.selected.is_none_or(|i| i >= remaining) {
            self.exit_triage();
            self.status = "triage complete".into();
        }
    }

    /// Show the activity-feed overlay for `title`.
    pub fn show_activity(&mut self, title: String, events: Vec<ActivityEvent>) {
        self.status = match events.len() {
            0 => format!("{title}: nothing has happened yet — any key to close"),
            1 => format!("{title}: 1 event — j/k scroll, any other key closes"),
            n => format!("{title}: {n} events — j/k scroll, any other key closes"),
        };
        self.activity = Some(ActivityFeed {
            title,
            events,
            scroll: 0,
        });
    }

    /// Show the `:devices` overlay.
    pub fn show_devices(&mut self, rows: Vec<DeviceRow>) {
        self.status = format!("{} paired device(s) — any key to close", rows.len());
        self.devices = Some(rows);
    }

    /// Cycle the Browse pane focus (Tab): Streams → Contexts → Tasks.
    pub fn toggle_pane(&mut self) {
        self.focus_pane(self.pane.next());
    }

    /// Move focus into the sidebar (`h`), returning to whichever of its two
    /// lists was last used rather than always snapping to Streams — otherwise
    /// the context list is unreachable with one hand on `h`/`l`.
    pub fn focus_sidebar(&mut self) {
        if !self.pane.is_sidebar() {
            self.pane = self.last_sidebar;
        }
    }

    /// Move focus to the task pane (`l`).
    pub fn focus_tasks(&mut self) {
        self.pane = StreamPane::Tasks;
    }

    /// Focus a specific Browse pane, remembering the sidebar list.
    pub fn focus_pane(&mut self, pane: StreamPane) {
        self.pane = pane;
        if pane.is_sidebar() {
            self.last_sidebar = pane;
        }
    }

    /// Selected task ref, if any.
    #[must_use]
    pub fn selected_task(&self) -> Option<&Task> {
        self.selected.and_then(|i| self.tasks.get(i))
    }

    /// Selected stream row, if any.
    #[must_use]
    pub fn selected_stream_row(&self) -> Option<&StreamRow> {
        self.selected_stream.and_then(|i| self.streams.get(i))
    }

    /// Open the Focus view on the currently selected task, remembering the
    /// view to return to. No-op selection-wise when nothing is selected (the
    /// Focus view then renders its empty placeholder).
    pub fn open_focus(&mut self) {
        if self.view != View::Focus {
            self.prev_view = Some(self.view);
        }
        self.focused_task = self.selected_task().cloned();
        self.view = View::Focus;
    }

    /// Close the Focus view and return to the previous view (Today if the
    /// Focus view was entered directly). No-op outside the Focus view.
    pub fn close_focus(&mut self) {
        if self.view != View::Focus {
            return;
        }
        self.view = self.prev_view.take().unwrap_or(View::Today);
        self.focused_task = None;
    }

    /// The mode a transient mode (prompt, command line, picker, confirmation)
    /// hands control back to.
    ///
    /// A running focus session outranks a triage pass, which outranks Normal:
    /// capturing an aside mid-session must return to the session, exactly as a
    /// prompt opened from triage returns to the triage card.
    #[must_use]
    pub fn resting_mode(&self) -> Mode {
        if self.focus.is_running() {
            Mode::Focus
        } else if self.triage {
            Mode::Triage
        } else {
            Mode::Normal
        }
    }

    /// Title of the task the Focus view is on.
    ///
    /// Falls back to the running session's task id: a session recovered from
    /// another device can be live before its task has been read back, and a
    /// timer over a blank line reads like a bug.
    #[must_use]
    pub fn focused_title(&self) -> String {
        if let Some(t) = self.focused_task.as_ref() {
            return t.title.clone();
        }
        self.focus
            .running_task()
            .map_or_else(String::new, |id| id.to_str())
    }

    /// Mirror the planner cursor into [`Self::focused_task`], so the detail
    /// the view shows and the task `F` / Enter would start on are the same
    /// row the user is looking at.
    fn sync_focus_pick(&mut self) {
        if let Some(row) = self.focus.selected_plan() {
            self.focused_task = Some(row.task.clone());
        }
    }

    /// Reconcile the keyboard with the session log after a refresh.
    ///
    /// This is the *read* side: a session that appeared — started here,
    /// recovered after a crash, or merged in from another device — takes over
    /// the Focus view, and one that ended hands the keyboard back. Only
    /// Normal mode is taken over, so an open prompt is never yanked away.
    pub fn after_focus_loaded(&mut self) {
        let focused = self.focused_task.as_ref().map(|t| t.id);
        self.focus.after_plan_loaded(focused);
        if self.focus.is_running() {
            if self.mode == Mode::Normal {
                self.mode = Mode::Focus;
            }
            if self.view != View::Focus {
                self.prev_view = Some(self.view);
                self.view = View::Focus;
            }
        } else if self.mode == Mode::Focus {
            self.mode = Mode::Normal;
        }
    }

    /// Show the `:focus stats` overlay.
    pub fn show_focus_stats(&mut self, stats: Box<FocusStats>) {
        self.status = "focus stats — any key to close".into();
        self.focus.stats = Some(stats);
    }
}

/// Which of the selectable lists the cursor keys currently drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveList {
    /// The Browse sidebar's context list.
    Contexts,
    /// The Review view's scrolling panel.
    Review,
    /// The task list (Today / Inbox / Search / Stream's right pane).
    Tasks,
    /// The Stream view's left pane.
    Streams,
    /// The Routines view.
    Routines,
    /// The Focus view's planner queue.
    FocusPlan,
}

/// Clamp an optional selection index to a list of `len` items (first item
/// when unset, last when past the end, `None` when empty).
fn clamp_selection(len: usize, selected: Option<usize>) -> Option<usize> {
    match (len, selected) {
        (0, _) => None,
        (_, None) => Some(0),
        (n, Some(i)) if i >= n => Some(n - 1),
        (_, s) => s,
    }
}

/// Next index with wraparound; `None` when empty.
fn wrap_next(len: usize, selected: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.map_or(0, |i| (i + 1) % len))
}

/// Next index, stopping at the last row; `None` when empty.
fn clamp_next(len: usize, selected: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.map_or(0, |i| (i + 1).min(len - 1)))
}

/// Previous index, stopping at the first row; `None` when empty.
fn clamp_prev(len: usize, selected: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.map_or(0, |i| i.saturating_sub(1)))
}

/// Previous index with wraparound; `None` when empty.
fn wrap_prev(len: usize, selected: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.map_or(0, |i| if i == 0 { len - 1 } else { i - 1 }))
}

/// Deterministic fixtures shared by the view/render test modules.
#[cfg(test)]
pub(crate) mod fixtures {
    use jiff::Timestamp;
    use std::collections::BTreeSet;
    use sunrise_core::queries::{ContextRow, FocusPlanRow, FocusSessionRow, StreamRow};
    use sunrise_domain::rrule::RRule;
    use sunrise_domain::{
        inbox_stream_ref, Chunk, Energy, EnergyFit, FocusKind, FocusSession, FocusStart, Routine,
        RoutineCatchupPolicy, SessionPlan, StreamColor, Task, TaskState, TaskTemplate, POMODORO_MS,
    };
    use sunrise_id::{EntityKind, EntityRef};

    /// A **running** work session on `task`: a `start` op with no `end`.
    ///
    /// `focused_ms` is seeded with a deliberately absurd value. It is the
    /// snapshot the query took when it ran, and nothing that renders a live
    /// timer may use it — the timer derives from the clock instead. A test
    /// that starts showing `9:59:59` has caught exactly the bug
    /// `docs/11-adr/0013-…` exists to prevent.
    pub(crate) fn running_session(
        task: EntityRef,
        started_at_ms: u64,
        planned_ms: Option<u64>,
    ) -> FocusSessionRow {
        FocusSessionRow {
            session: FocusSession {
                start: FocusStart {
                    id: EntityRef::new(EntityKind::FocusSession, [42u8; 16]),
                    task_id: task,
                    stream_id: inbox_stream_ref(),
                    started_at_ms,
                    planned_ms,
                    energy: Some(Energy::High),
                    kind: FocusKind::Work,
                    chunk: Some(Chunk { index: 2, total: 4 }),
                },
                end: None,
                interruptions: Vec::new(),
            },
            running: true,
            focused_ms: 35_999_000,
        }
    }

    /// An **ended** work session on `task` — what `break_after` counts.
    pub(crate) fn ended_work_session(task: EntityRef, idx: u8) -> FocusSessionRow {
        let mut row = running_session(task, 1_000, Some(POMODORO_MS));
        row.session.start.id = EntityRef::new(EntityKind::FocusSession, [idx; 16]);
        row.session.end = Some(sunrise_domain::FocusEnd {
            session_id: row.session.start.id,
            ended_at_ms: 1_000 + POMODORO_MS,
            actual_focused_ms: POMODORO_MS,
            interruptions: Vec::new(),
            completed_task: false,
        });
        row.running = false;
        row
    }

    /// One planner row: a task, its leverage, and its energy fit.
    pub(crate) fn plan_row(idx: u8, unblocks: u32, energy_fit: EnergyFit) -> FocusPlanRow {
        let mut task = fake_task(idx);
        task.title = format!("plan task {idx}");
        FocusPlanRow {
            task,
            unblocks,
            energy_fit,
            suggested: SessionPlan {
                planned_ms: Some(POMODORO_MS),
                chunk: Some(Chunk { index: 1, total: 4 }),
            },
            prior_sessions: 0,
        }
    }

    /// A minimal task; `idx` seeds the id and title.
    pub(crate) fn fake_task(idx: u8) -> Task {
        Task {
            id: EntityRef::new(EntityKind::Task, [idx; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            title: format!("task {idx}"),
            body: None,
            stream_id: EntityRef::new(EntityKind::Stream, [0u8; 16]),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            archived: false,
            deleted: false,
        }
    }

    /// A minimal live routine: `title`, `rrule` body, anchored at `starts_at`
    /// in UTC.
    pub(crate) fn fake_routine(idx: u8, title: &str, rrule: &str, starts_at: &str) -> Routine {
        Routine {
            id: EntityRef::new(EntityKind::Routine, [idx; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            template: TaskTemplate {
                title: title.into(),
                stream_id: inbox_stream_ref(),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse(rrule).expect("valid rrule"),
            timezone: "UTC".into(),
            starts_at: starts_at.parse().expect("valid timestamp"),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 0,
            last_completed_at: None,
            // Streak bookkeeping (migration 0009). Defaults here: this helper
            // exists to render a routine row, and no view test asserts streak
            // behaviour — that lives in sunrise-domain's streak tests.
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
        }
    }

    /// The synthetic Inbox stream row (`StreamList` returns it first).
    pub(crate) fn inbox_row(open: u64) -> StreamRow {
        StreamRow {
            id: inbox_stream_ref(),
            name: "Inbox".into(),
            color: StreamColor::Slate,
            open_task_count: open,
            archived: false,
            paused: false,
        }
    }

    /// A named context row; `idx` seeds the id.
    pub(crate) fn context_row(idx: u8, name: &str) -> ContextRow {
        ContextRow {
            id: EntityRef::new(EntityKind::Context, [idx; 16]),
            name: name.into(),
            description: None,
            archived: false,
            task_count: 0,
        }
    }

    /// A named user stream row; `idx` seeds the id.
    /// A projected Routines-view row, with a plausible template behind it.
    pub(crate) fn routine_row(
        idx: u8,
        title: &str,
        rrule: &str,
        paused: bool,
    ) -> super::RoutineRow {
        super::RoutineRow {
            id: EntityRef::new(EntityKind::Routine, [idx; 16]),
            title: title.into(),
            rrule: rrule.into(),
            next: None,
            paused,
            template: TaskTemplate {
                title: title.into(),
                stream_id: inbox_stream_ref(),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rule: RRule {
                freq: sunrise_domain::rrule::Frequency::Daily,
                interval: 1,
                by_day: Vec::new(),
                by_month_day: Vec::new(),
                by_month: Vec::new(),
                by_set_pos: Vec::new(),
                count: None,
                until: None,
                wkst: None,
            },
            streak: 0,
        }
    }

    pub(crate) fn stream_row(idx: u8, name: &str, open: u64) -> StreamRow {
        StreamRow {
            id: EntityRef::new(EntityKind::Stream, [idx; 16]),
            name: name.into(),
            color: StreamColor::Sky,
            open_task_count: open,
            archived: false,
            paused: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{fake_routine, fake_task, inbox_row, stream_row};
    use super::*;

    /// 2026-01-01T00:00:00Z — the fixed "now" for routine projection tests.
    fn now() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().expect("valid timestamp")
    }

    #[test]
    fn rrule_summary_reads_as_english() {
        let daily = fake_routine(1, "t", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        assert_eq!(rrule_summary(&daily.rrule), "every day");

        let biweekly = fake_routine(
            2,
            "t",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE",
            "2026-01-05T09:00:00Z",
        );
        assert_eq!(rrule_summary(&biweekly.rrule), "every 2 weeks on Mo, We");

        let monthly = fake_routine(
            3,
            "t",
            "FREQ=MONTHLY;BYMONTHDAY=1;COUNT=6",
            "2026-01-01T09:00:00Z",
        );
        assert_eq!(rrule_summary(&monthly.rrule), "every month day 1 ×6");
    }

    #[test]
    fn routine_rows_resolve_the_next_occurrence() {
        let daily = fake_routine(1, "Water plants", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        let rows = routine_rows(&[daily], now());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Water plants");
        assert_eq!(rows[0].rrule, "every day");
        assert_eq!(
            rows[0].next,
            Some("2026-01-01T09:00:00Z".parse().expect("valid timestamp"))
        );
    }

    #[test]
    fn routine_rows_project_a_paused_routine_with_no_next() {
        let mut r = fake_routine(1, "Water plants", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        r.paused = true;
        let rows = routine_rows(&[r], now());
        assert!(rows[0].paused);
        assert_eq!(rows[0].next, None);
    }

    #[test]
    fn routine_rows_report_no_next_past_the_series_end() {
        // A daily series that stopped in 2025 has nothing left to schedule.
        let r = fake_routine(
            1,
            "Old habit",
            "FREQ=DAILY;UNTIL=20250601T000000Z",
            "2025-01-01T09:00:00Z",
        );
        let rows = routine_rows(&[r], now());
        assert_eq!(rows[0].next, None);
    }

    #[test]
    fn routine_navigation_is_independent_of_the_task_cursor() {
        let mut s = ViewState::default();
        s.view = View::Routines;
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.routines = routine_rows(
            &[
                fake_routine(1, "a", "FREQ=DAILY", "2026-01-01T09:00:00Z"),
                fake_routine(2, "b", "FREQ=DAILY", "2026-01-01T10:00:00Z"),
            ],
            now(),
        );
        s.after_routines_loaded();
        assert_eq!(s.selected_routine, Some(0));
        s.nav_next();
        assert_eq!(s.selected_routine, Some(1));
        assert_eq!(s.selected, Some(0), "task cursor must not move");
        s.nav_next();
        assert_eq!(s.selected_routine, Some(0), "wraps");
        assert_eq!(
            s.selected_routine_row().map(|r| r.title.clone()),
            Some("a".into())
        );
    }

    #[test]
    fn nav_first_and_last_are_no_ops_on_empty_lists() {
        let mut s = ViewState::default();
        s.nav_first();
        s.nav_last();
        assert_eq!(s.selected, None);
    }

    #[test]
    fn select_next_wraps() {
        let mut s = ViewState::default();
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        assert_eq!(s.selected, Some(0));
        s.select_next();
        assert_eq!(s.selected, Some(1));
        s.select_next();
        s.select_next();
        // wraps after 3
        assert_eq!(s.selected, Some(0));
    }

    #[test]
    fn select_prev_wraps() {
        let mut s = ViewState::default();
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.select_prev();
        assert_eq!(s.selected, Some(2));
    }

    #[test]
    fn empty_clears_selection() {
        let mut s = ViewState::default();
        s.after_tasks_loaded();
        assert_eq!(s.selected, None);
        s.select_next();
        assert_eq!(s.selected, None);
    }

    #[test]
    fn streams_default_to_first_row() {
        let mut s = ViewState::default();
        s.streams = vec![inbox_row(2), stream_row(1, "Work", 3)];
        s.after_streams_loaded();
        assert_eq!(s.selected_stream, Some(0));
        assert_eq!(s.selected_stream_row().unwrap().name, "Inbox");
    }

    #[test]
    fn stream_selection_wraps_both_ways() {
        let mut s = ViewState::default();
        s.streams = vec![inbox_row(0), stream_row(1, "Work", 1)];
        s.after_streams_loaded();
        s.stream_next();
        assert_eq!(s.selected_stream, Some(1));
        s.stream_next();
        assert_eq!(s.selected_stream, Some(0));
        s.stream_prev();
        assert_eq!(s.selected_stream, Some(1));
    }

    #[test]
    fn stream_selection_clamps_after_reload() {
        let mut s = ViewState::default();
        s.streams = vec![inbox_row(0), stream_row(1, "Work", 1)];
        s.selected_stream = Some(5);
        s.after_streams_loaded();
        assert_eq!(s.selected_stream, Some(1));
        s.streams.clear();
        s.after_streams_loaded();
        assert_eq!(s.selected_stream, None);
    }

    #[test]
    fn pane_toggle_cycles_all_three_browse_panes() {
        let mut s = ViewState::default();
        assert_eq!(s.pane, StreamPane::Streams);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Contexts);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Tasks);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Streams);
    }

    #[test]
    fn h_returns_to_whichever_sidebar_list_was_last_used() {
        // Always snapping back to Streams would make the context list
        // unreachable with one hand on `h`/`l`.
        let mut s = ViewState::default();
        s.focus_pane(StreamPane::Contexts);
        s.focus_tasks();
        assert_eq!(s.pane, StreamPane::Tasks);
        s.focus_sidebar();
        assert_eq!(s.pane, StreamPane::Contexts);
        s.focus_sidebar();
        assert_eq!(s.pane, StreamPane::Contexts);
    }

    #[test]
    fn nav_is_pane_aware_in_stream_view() {
        let mut s = ViewState::default();
        s.view = View::Stream;
        s.streams = vec![inbox_row(0), stream_row(1, "Work", 1)];
        s.tasks = (0u8..2).map(fake_task).collect();
        s.after_streams_loaded();
        s.after_tasks_loaded();

        // Streams pane: j/k move the stream cursor, not the task cursor.
        s.pane = StreamPane::Streams;
        s.nav_next();
        assert_eq!(s.selected_stream, Some(1));
        assert_eq!(s.selected, Some(0));

        // Tasks pane: j/k move the task cursor.
        s.pane = StreamPane::Tasks;
        s.nav_next();
        assert_eq!(s.selected, Some(1));
        assert_eq!(s.selected_stream, Some(1));
    }

    #[test]
    fn nav_ignores_pane_outside_stream_view() {
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.pane = StreamPane::Streams;
        s.tasks = (0u8..2).map(fake_task).collect();
        s.after_tasks_loaded();
        s.nav_next();
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn focus_state_machine_roundtrip() {
        // Select in Inbox → open Focus shows that task → Esc returns.
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.select_next();
        let expected = s.selected_task().unwrap().id;

        s.open_focus();
        assert_eq!(s.view, View::Focus);
        assert_eq!(s.focused_task.as_ref().unwrap().id, expected);
        assert_eq!(s.prev_view, Some(View::Inbox));

        s.close_focus();
        assert_eq!(s.view, View::Inbox);
        assert_eq!(s.focused_task, None);
        assert_eq!(s.prev_view, None);
        // Selection in the originating view is untouched.
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn close_focus_defaults_to_today_and_noops_elsewhere() {
        let mut s = ViewState::default();
        s.view = View::Focus; // entered without open_focus (e.g. `:view focus`)
        s.close_focus();
        assert_eq!(s.view, View::Today);

        s.view = View::Inbox;
        s.close_focus();
        assert_eq!(s.view, View::Inbox);
    }

    #[test]
    fn reopening_focus_from_focus_keeps_original_return_view() {
        let mut s = ViewState::default();
        s.view = View::Search;
        s.tasks = vec![fake_task(1)];
        s.after_tasks_loaded();
        s.open_focus();
        // A second open_focus while already in Focus must not clobber
        // prev_view with View::Focus.
        s.open_focus();
        assert_eq!(s.prev_view, Some(View::Search));
        s.close_focus();
        assert_eq!(s.view, View::Search);
    }
}

#[cfg(test)]
mod focus_tests {
    use super::fixtures::{ended_work_session, fake_task, plan_row, running_session};
    use super::*;
    use sunrise_domain::{EnergyFit, InterruptionReason, POMODORO_MS};
    use sunrise_id::{EntityKind, EntityRef};

    fn running_state() -> ViewState {
        let task = fake_task(1);
        let mut s = ViewState::default();
        s.view = View::Focus;
        s.focused_task = Some(task.clone());
        s.focus.running = Some(running_session(task.id, 1_000, Some(POMODORO_MS)));
        s.mode = Mode::Focus;
        s
    }

    #[test]
    fn elapsed_and_remaining_derive_from_the_clock_and_are_never_stored() {
        let s = running_state();
        assert_eq!(s.focus.elapsed_ms(1_000), Some(0));
        assert_eq!(s.focus.elapsed_ms(1_000 + 300_000), Some(300_000));
        assert_eq!(
            s.focus.remaining_ms(1_000 + 300_000),
            Some(Some(POMODORO_MS - 300_000))
        );
        // Same value, three clocks, three answers: nothing about the running
        // timer lives in `FocusState`.
        assert_eq!(s.focus.elapsed_ms(u64::MAX), Some(u64::MAX - 1_000));
        assert_eq!(s.focus.remaining_ms(u64::MAX), Some(Some(0)));
    }

    #[test]
    fn an_open_ended_session_has_no_remaining() {
        let task = fake_task(1);
        let mut s = ViewState::default();
        s.focus.running = Some(running_session(task.id, 0, None));
        assert_eq!(s.focus.remaining_ms(999_999), Some(None));
    }

    #[test]
    fn work_sessions_done_is_counted_from_the_log_not_a_counter() {
        let task = fake_task(1);
        let mut s = running_state();
        // Only *ended* work segments count; the one currently running has not
        // happened yet.
        s.focus.sessions = vec![
            running_session(task.id, 1_000, Some(POMODORO_MS)),
            ended_work_session(task.id, 7),
            ended_work_session(task.id, 8),
        ];
        assert_eq!(s.focus.work_sessions_done(), 2);
        // Two done plus the one running is the third cycle → still a short
        // break. This is `break_after`, not arithmetic repeated here.
        assert_eq!(s.focus.next_segment(), Segment::ShortBreak);
        s.focus.sessions.push(ended_work_session(task.id, 9));
        assert_eq!(s.focus.work_sessions_done(), 3);
        // The fourth cycle earns the long break.
        assert_eq!(s.focus.next_segment(), Segment::LongBreak);
        assert_eq!(
            Segment::LongBreak.default_ms(),
            sunrise_domain::LONG_BREAK_MS
        );
    }

    #[test]
    fn a_running_session_owns_the_resting_mode() {
        let mut s = running_state();
        assert_eq!(s.resting_mode(), Mode::Focus);
        // A prompt opened mid-session hands control back to the session, not
        // to Normal — the same contract triage has.
        s.mode = Mode::Insert;
        s.prompt = Some(Prompt::CaptureAside);
        s.reset_to_normal();
        assert_eq!(s.mode, Mode::Focus);
        assert!(s.prompt.is_none());
        s.focus.running = None;
        assert_eq!(s.resting_mode(), Mode::Normal);
    }

    #[test]
    fn after_focus_loaded_takes_the_keyboard_and_gives_it_back() {
        let task = fake_task(1);
        let mut s = ViewState::default();
        s.view = View::Inbox;
        // A session appears — started here, recovered after a crash, or merged
        // in from another device; the read side cannot tell and need not.
        s.focus.running = Some(running_session(task.id, 0, Some(POMODORO_MS)));
        s.after_focus_loaded();
        assert_eq!(s.mode, Mode::Focus);
        assert_eq!(s.view, View::Focus);
        assert_eq!(s.prev_view, Some(View::Inbox));
        // ... and when it ends, the keyboard comes back.
        s.focus.running = None;
        s.after_focus_loaded();
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn after_focus_loaded_never_yanks_an_open_prompt() {
        let task = fake_task(1);
        let mut s = ViewState::default();
        s.mode = Mode::Insert;
        s.prompt = Some(Prompt::Capture);
        s.focus.running = Some(running_session(task.id, 0, Some(POMODORO_MS)));
        s.after_focus_loaded();
        assert_eq!(s.mode, Mode::Insert, "a half-typed capture survives");
    }

    #[test]
    fn the_planner_cursor_is_the_focus_pick() {
        let mut s = ViewState::default();
        s.view = View::Focus;
        s.focus.plan = vec![
            plan_row(1, 3, EnergyFit::Exact),
            plan_row(2, 0, EnergyFit::Under),
        ];
        s.after_focus_loaded();
        assert_eq!(s.focus.selected, Some(0));
        s.nav_next();
        assert_eq!(s.focus.selected, Some(1));
        // The detail pane and the task `F` would start on are the same row.
        assert_eq!(
            s.focused_task.as_ref().map(|t| t.id),
            Some(s.focus.plan[1].task.id)
        );
        s.nav_first();
        assert_eq!(s.focus.selected, Some(0));
    }

    #[test]
    fn a_reload_keeps_the_cursor_on_the_task_the_user_picked() {
        let mut s = ViewState::default();
        s.view = View::Focus;
        s.focus.plan = vec![
            plan_row(1, 3, EnergyFit::Exact),
            plan_row(2, 1, EnergyFit::Exact),
        ];
        s.after_focus_loaded();
        s.nav_next();
        let picked = s.focus.plan[1].task.id;
        // The queue re-ranks under the user's hand: the pick follows the task.
        s.focus.plan = vec![
            plan_row(2, 9, EnergyFit::Exact),
            plan_row(1, 3, EnergyFit::Exact),
        ];
        s.after_focus_loaded();
        assert_eq!(s.focus.selected_plan().map(|r| r.task.id), Some(picked));
    }

    #[test]
    fn the_focus_operand_is_the_session_task_not_the_list_cursor() {
        let mut s = running_state();
        // The underlying list still has a cursor on something else entirely.
        s.tasks = vec![fake_task(9)];
        s.selected = Some(0);
        assert_eq!(s.operand_ids(), vec![s.focus.running_task().unwrap()]);
        assert_eq!(s.operand_label(), s.focused_title());
    }

    #[test]
    fn the_cascade_names_what_moved_and_keeps_no_score() {
        let t = |b: u8| EntityRef::new(EntityKind::Task, [b; 16]);
        let report = CascadeReport {
            cascade: UnblockCascade {
                completed: t(1),
                released: vec![t(2), t(3)],
                still_blocked: vec![t(4)],
            },
            released: vec!["Deploy".into(), "QA".into()],
        };
        let line = report.line();
        assert!(line.contains("\"Deploy\""), "{line}");
        assert!(line.contains("\"QA\""), "{line}");
        assert!(line.contains("1 still waiting"), "{line}");
        // No score, no streak, no congratulation.
        for banned in ["streak", "score", "congrat", "well done", "point"] {
            assert!(!line.to_lowercase().contains(banned), "{line}");
        }
        let empty = CascadeReport {
            cascade: UnblockCascade {
                completed: t(1),
                released: Vec::new(),
                still_blocked: Vec::new(),
            },
            released: Vec::new(),
        };
        assert_eq!(empty.line(), "nothing was waiting on it");
    }

    #[test]
    fn durations_read_as_a_timer() {
        assert_eq!(fmt_duration_ms(0), "00:00");
        assert_eq!(fmt_duration_ms(59_999), "00:59");
        assert_eq!(fmt_duration_ms(POMODORO_MS), "25:00");
        assert_eq!(
            fmt_duration_ms(3 * 3_600_000 + 4 * 60_000 + 5_000),
            "3:04:05"
        );
    }

    #[test]
    fn labels_cover_every_choice() {
        assert_eq!(energy_budget_label(None), "any");
        assert_eq!(energy_budget_label(Some(Energy::High)), "high");
        assert_eq!(length_label(SessionLength::OnePomodoro), "one pomodoro");
        assert_eq!(length_label(SessionLength::UntilDone), "until done");
        assert_eq!(segment_label(Segment::Work), "work");
        assert_eq!(segment_label(Segment::LongBreak), "long break");
        // The reason set is the domain's, not one invented here.
        assert_eq!(InterruptionReason::Meeting.as_str(), "meeting");
    }
}
