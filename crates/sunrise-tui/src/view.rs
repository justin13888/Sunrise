//! View enum + view-state.

use crate::keymap::Mode;
use jiff::Timestamp;
use sunrise_core::queries::StreamRow;
use sunrise_domain::rrule::RRule;
use sunrise_domain::{materialization_horizon_days, Routine, Task};
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
    /// Routines: read-only listing of recurring templates.
    Routines,
}

/// Which pane of the split Stream view has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPane {
    /// Left pane: the stream list.
    Streams,
    /// Right pane: tasks of the selected stream.
    Tasks,
}

impl StreamPane {
    /// The other pane (there are exactly two).
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Streams => Self::Tasks,
            Self::Tasks => Self::Streams,
        }
    }
}

/// A pending prompt occupying the shared input line (or, for
/// [`Prompt::ConfirmDelete`], the confirmation gate). Exactly one prompt can
/// be active; [`crate::runtime::apply_action`] consumes it on Submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// Capture a new task (`c`).
    Capture,
    /// Edit an existing task's title (`e`).
    EditTitle(EntityRef),
    /// Defer a task by a typed offset such as `2h` / `3d` (`d`).
    Defer(EntityRef),
    /// Delete a task, gated on an explicit `y` (`D`).
    ConfirmDelete {
        /// Task to delete once confirmed.
        id: EntityRef,
        /// Title echoed in the confirmation message.
        title: String,
    },
    /// Create a stream by name (`S`).
    CreateStream,
    /// Free-text search query (`/`).
    Search,
}

/// State for the modal move-to-stream picker (`m`).
///
/// Not `PartialEq`: `StreamRow` (from the core's query surface) isn't.
#[derive(Debug, Clone)]
pub struct StreamPicker {
    /// Task being moved.
    pub task: EntityRef,
    /// Task title, echoed in the picker header.
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

    /// The highlighted destination stream, if any.
    #[must_use]
    pub fn selected_row(&self) -> Option<&StreamRow> {
        self.rows.get(self.selected)
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
            }
        })
        .collect()
}

/// Owning struct for the active view.
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
    /// Index of the selected stream in `streams`. `None` if the list is empty.
    pub selected_stream: Option<usize>,
    /// Focused pane in the Stream view.
    pub pane: StreamPane,
    /// Task shown fullscreen in the Focus view.
    pub focused_task: Option<Task>,
    /// View to return to when Focus is closed with Esc.
    pub prev_view: Option<View>,
    /// Active text input (capture buffer / search query).
    pub input: String,
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
    /// Latch for the `gg` chord: set by the first `g`, cleared by anything else.
    pub pending_g: bool,
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
            selected_stream: None,
            pane: StreamPane::Streams,
            focused_task: None,
            prev_view: None,
            input: String::new(),
            status: String::new(),
            sync: None,
            routines: Vec::new(),
            selected_routine: None,
            prompt: None,
            picker: None,
            show_help: false,
            pending_g: false,
        }
    }
}

impl ViewState {
    /// Apply selection bookkeeping after `tasks` has changed.
    pub fn after_tasks_loaded(&mut self) {
        self.selected = clamp_selection(self.tasks.len(), self.selected);
    }

    /// Apply selection bookkeeping after `streams` has changed. Defaults the
    /// selection to the first row (Inbox — `StreamList` returns it first).
    pub fn after_streams_loaded(&mut self) {
        self.selected_stream = clamp_selection(self.streams.len(), self.selected_stream);
    }

    /// Move selection down by one, wrapping at the end.
    pub fn select_next(&mut self) {
        self.selected = wrap_next(self.tasks.len(), self.selected);
    }

    /// Move selection up by one, wrapping at the start.
    pub fn select_prev(&mut self) {
        self.selected = wrap_prev(self.tasks.len(), self.selected);
    }

    /// Move the stream selection down by one, wrapping at the end.
    pub fn stream_next(&mut self) {
        self.selected_stream = wrap_next(self.streams.len(), self.selected_stream);
    }

    /// Move the stream selection up by one, wrapping at the start.
    pub fn stream_prev(&mut self) {
        self.selected_stream = wrap_prev(self.streams.len(), self.selected_stream);
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
            _ => ActiveList::Tasks,
        }
    }

    /// Cursor-down in whichever list has keyboard focus (Stream view is
    /// pane-aware; every other view navigates the task list).
    pub fn nav_next(&mut self) {
        match self.active_list() {
            ActiveList::Streams => self.stream_next(),
            ActiveList::Routines => {
                self.selected_routine = wrap_next(self.routines.len(), self.selected_routine);
            }
            ActiveList::Tasks => self.select_next(),
        }
    }

    /// Cursor-up counterpart of [`Self::nav_next`].
    pub fn nav_prev(&mut self) {
        match self.active_list() {
            ActiveList::Streams => self.stream_prev(),
            ActiveList::Routines => {
                self.selected_routine = wrap_prev(self.routines.len(), self.selected_routine);
            }
            ActiveList::Tasks => self.select_prev(),
        }
    }

    /// Jump the focused list's cursor to its first row (`gg`).
    pub fn nav_first(&mut self) {
        self.nav_to(|_| 0);
    }

    /// Jump the focused list's cursor to its last row (`G`).
    pub fn nav_last(&mut self) {
        self.nav_to(|len| len - 1);
    }

    /// Shared body of [`Self::nav_first`] / [`Self::nav_last`]: resolve the
    /// focused list's length, then set its cursor (no-op when empty).
    fn nav_to(&mut self, pick: impl Fn(usize) -> usize) {
        let list = self.active_list();
        let len = match list {
            ActiveList::Streams => self.streams.len(),
            ActiveList::Routines => self.routines.len(),
            ActiveList::Tasks => self.tasks.len(),
        };
        if len == 0 {
            return;
        }
        let idx = Some(pick(len));
        match list {
            ActiveList::Streams => self.selected_stream = idx,
            ActiveList::Routines => self.selected_routine = idx,
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
    pub fn open_stream_picker(&mut self, task: EntityRef, title: String, rows: Vec<StreamRow>) {
        let current = self
            .tasks
            .iter()
            .find(|t| t.id == task)
            .map(|t| t.stream_id);
        let selected = current
            .and_then(|s| rows.iter().position(|r| r.id == s))
            .unwrap_or(0);
        self.status = format!("move \"{title}\" to stream — Enter to choose, Esc to cancel");
        self.picker = Some(StreamPicker {
            task,
            task_title: title,
            rows,
            selected,
        });
        self.mode = Mode::Picker;
    }

    /// Clear any prompt/picker/help overlay and return to Normal mode.
    pub fn reset_to_normal(&mut self) {
        self.mode = Mode::Normal;
        self.prompt = None;
        self.picker = None;
        self.pending_g = false;
        self.input.clear();
        self.status.clear();
    }

    /// Toggle which Stream-view pane has focus (Tab).
    pub fn toggle_pane(&mut self) {
        self.pane = self.pane.other();
    }

    /// Focus a specific Stream-view pane (`h` → Streams, `l` → Tasks).
    pub fn focus_pane(&mut self, pane: StreamPane) {
        self.pane = pane;
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
}

/// Which of the three selectable lists the cursor keys currently drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveList {
    /// The task list (Today / Inbox / Search / Stream's right pane).
    Tasks,
    /// The Stream view's left pane.
    Streams,
    /// The Routines view.
    Routines,
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
    use sunrise_core::queries::StreamRow;
    use sunrise_domain::rrule::RRule;
    use sunrise_domain::{
        inbox_stream_ref, Routine, RoutineCatchupPolicy, StreamColor, Task, TaskState, TaskTemplate,
    };
    use sunrise_id::{EntityKind, EntityRef};

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
        }
    }

    /// A named user stream row; `idx` seeds the id.
    pub(crate) fn stream_row(idx: u8, name: &str, open: u64) -> StreamRow {
        StreamRow {
            id: EntityRef::new(EntityKind::Stream, [idx; 16]),
            name: name.into(),
            color: StreamColor::Sky,
            open_task_count: open,
            archived: false,
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
    fn pane_toggle_and_focus() {
        let mut s = ViewState::default();
        assert_eq!(s.pane, StreamPane::Streams);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Tasks);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Streams);
        s.focus_pane(StreamPane::Tasks);
        assert_eq!(s.pane, StreamPane::Tasks);
        s.focus_pane(StreamPane::Tasks);
        assert_eq!(s.pane, StreamPane::Tasks);
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
