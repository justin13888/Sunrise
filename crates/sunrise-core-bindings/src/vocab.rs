//! The shared vocabulary, exported as functions rather than restated.
//!
//! Everything here is a thin lowering of [`sunrise_domain::phrase`] or
//! [`sunrise_domain::planning`]. None of it computes anything; the point is
//! that a client **cannot** be tempted to compute it.
//!
//! The temptation is real and specific. "Overdue", "tomorrow", "1h30" and
//! "weekdays 09:00–17:00" are each two lines to write in Swift, and each of
//! those two-line versions would disagree with the CLI the first time a
//! deadline landed on a DST boundary or a task was due at 09:00 and read at
//! 17:00. `docs/07-clients/overview.md` says this out loud; these exports are
//! what make following it the path of least resistance.
//!
//! Free functions, not methods: none of them touches the vault, so making them
//! hang off [`crate::SunriseCore`] would imply a dependency on an open vault
//! that does not exist — and a capture preview must render before a vault is
//! even unlocked on a first run.

use sunrise_domain::{
    EnergyFit, ExportDataset, ExportFormat, InterruptionReason, RRule, ScheduleConstraint,
    SessionLength, SnoozeSpan, SunriseTime, TodaySection,
};

use crate::dto::{
    BlockDraftIn, BlockGridRow, Constraint, Recurrence, RoutineItem, SessionRow, TimeValue,
};
use crate::BindingError;

/// See [`sunrise_domain::TodaySection`].
///
/// Declared remotely rather than mirrored so that adding a section upstream
/// fails this crate's build instead of silently producing a value the app
/// cannot name.
#[uniffi::remote(Enum)]
pub enum TodaySection {
    Scheduled,
    Due,
    Overdue,
}

/// `relative_day`'s two answers, as a record.
///
/// `is_past` is not derivable from `text` — "-3d" and "+3d" differ by one
/// character, and a client branching on a string prefix to colour a row is
/// exactly the restatement this module exists to prevent.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RelativeDay {
    /// `yesterday` / `today` / `tomorrow` / `+3d` / `-3d`.
    pub text: String,
    /// Whether the day is behind the caller's `now_ms`.
    pub is_past: bool,
}

/// Which section of Today a row belongs in.
///
/// See [`sunrise_domain::today_section`]: the overdue boundary is
/// `due_at < start_of_today_local`, which is **not** `due_at < now_ms`.
///
/// `tz` is an IANA zone name; an unknown one falls back to UTC, for the same
/// reason [`crate::SunriseCore::capture`] does — showing a row in the wrong
/// section is a smaller failure than showing no rows at all.
#[uniffi::export]
#[must_use]
pub fn today_section(
    scheduled_at: Option<TimeValue>,
    due_at: Option<TimeValue>,
    now_ms: u64,
    tz: String,
) -> TodaySection {
    let zone = zone_or_utc(&tz);
    let scheduled = scheduled_at.map(SunriseTime::from);
    let due = due_at.map(SunriseTime::from);
    sunrise_domain::today_section(scheduled.as_ref(), due.as_ref(), now_ms, &zone)
}

/// `yesterday` / `today` / `tomorrow` / `+3d`, in whole civil days.
///
/// See [`sunrise_domain::relative_day`]. Whole days, not elapsed hours: "due
/// tomorrow" must not read as "today" because it happens to be 23:30 now.
#[uniffi::export]
#[must_use]
pub fn relative_day(at: TimeValue, now_ms: u64, tz: String) -> RelativeDay {
    let zone = zone_or_utc(&tz);
    let at = SunriseTime::from(at).to_instant(&zone);
    let (text, is_past) = sunrise_domain::relative_day(at, now_ms, &zone);
    RelativeDay { text, is_past }
}

/// `1800` → `30m`, `5400` → `1h30`. The compact form, for a list column.
#[uniffi::export]
#[must_use]
pub fn short_duration(secs: u64) -> String {
    sunrise_domain::short_duration(secs)
}

/// `MM:SS`, widening to `H:MM:SS` past an hour. The form for a live timer.
#[uniffi::export]
#[must_use]
pub fn duration_clock(ms: u64) -> String {
    sunrise_domain::fmt_duration_ms(ms)
}

/// The word for an energy facet. `None` reads as "any" — the value that drops
/// energy out of the planner's ranking rather than meaning "no energy".
///
/// Three words, and exported anyway. A client writing its own `switch` here
/// would be the first place "med" quietly became "medium" in one client and
/// not the other.
#[uniffi::export]
#[must_use]
pub fn energy_label(energy: Option<sunrise_domain::Energy>) -> String {
    sunrise_domain::energy_budget_label(energy).to_string()
}

/// One line describing a task's scheduling constraints, empty when it has
/// none.
///
/// The `soft_violations` a [`crate::dto::CommandOutcome`] carries are the same
/// type, so the same call renders "you scheduled this outside its window".
#[uniffi::export]
#[must_use]
pub fn constraint_summary(constraints: Vec<Constraint>) -> String {
    let list: Vec<ScheduleConstraint> = constraints.into_iter().map(Into::into).collect();
    sunrise_domain::constraint_summary(&list)
}

/// Read `every 2 weeks on tue` — or a raw `FREQ=WEEKLY;BYDAY=TU` body — into
/// a [`Recurrence`].
///
/// See [`sunrise_domain::parse_recurrence`]. This is the **only** way a client
/// should turn typed text into a rule. Writing a second parser in Swift would
/// mean `weekends` meaning one thing in the app and another in `sunrise-cli`,
/// and the disagreement would surface as tasks generated on the wrong days —
/// weeks later, silently.
///
/// # Errors
///
/// [`BindingError::BadRecurrence`], carrying the phrase and the domain's own
/// message. Nothing is guessed at: an unreadable cadence is refused rather
/// than rounded to the nearest thing that parses.
#[uniffi::export]
pub fn parse_recurrence(text: String) -> Result<Recurrence, BindingError> {
    sunrise_domain::parse_recurrence(&text)
        .map(|r| Recurrence::from(&r))
        .map_err(|cause| BindingError::BadRecurrence { text, cause })
}

/// Describe a rule in one line: `every 2 weeks on Mo, We`.
///
/// See [`sunrise_domain::rrule_summary`] — the inverse of
/// [`parse_recurrence`], and what [`crate::dto::RoutineListRow::cadence`]
/// already carries for a list row. Exported separately because a *routine
/// editor* has to describe a rule it has parsed but not yet saved, and there
/// is no row to read it off yet.
///
/// Lossy on purpose: it is prose, not a serialization.
#[uniffi::export]
#[must_use]
pub fn recurrence_summary(rule: Recurrence) -> String {
    sunrise_domain::rrule_summary(&RRule::from(rule))
}

/// How a session is sized, in words: `one pomodoro`, `sized to estimate`,
/// `until done`.
///
/// See [`sunrise_domain::length_label`] — and note that `plan_reason` already
/// uses the same words, so a picker that worded them itself would disagree
/// with the explanation printed on the row beside it.
#[uniffi::export]
#[must_use]
pub fn session_length_label(length: SessionLength) -> String {
    sunrise_domain::length_label(length).to_string()
}

/// How a task's energy facet scored against the session budget: `exact`,
/// `unknown`, `under`, `over`. See [`sunrise_domain::energy_fit_label`].
#[uniffi::export]
#[must_use]
pub fn energy_fit_label(fit: EnergyFit) -> String {
    sunrise_domain::energy_fit_label(fit).to_string()
}

/// The word for an interruption reason.
///
/// See [`sunrise_domain::InterruptionReason::as_str`] — which is also the
/// storage tag and the CSV column value, so a client wording these itself
/// would put one word on a button and a different one in the export of the
/// same tap.
#[uniffi::export]
#[must_use]
pub fn interruption_label(reason: InterruptionReason) -> String {
    reason.as_str().to_string()
}

/// Resolve a [`TimeValue`] to an instant, in epoch milliseconds.
///
/// See [`sunrise_domain::SunriseTime::to_instant`]. The four kinds are not
/// interchangeable — "Tuesday morning", "09:00 New York" and "this instant"
/// are three different answers — and collapsing them is exactly the decision
/// the domain owns. A client that needs one number (to seed a date picker,
/// say) asks for it here rather than reading the enum apart.
///
/// `tz` is the zone a floating or all-day value is read in; an unknown one
/// falls back to UTC, as everywhere else in this module.
#[uniffi::export]
#[must_use]
pub fn time_value_ms(value: TimeValue, tz: String) -> i64 {
    SunriseTime::from(value)
        .to_instant(&zone_or_utc(&tz))
        .as_millisecond()
}

/// A dataset's lowercase wire name: `trends`, `activity`, `focus`, `streaks`.
///
/// See [`sunrise_domain::ExportDataset::as_str`]. This is the name
/// `sunrise-cli export` takes on the command line, so a file saved from a GUI
/// and one saved from the CLI can be named the same thing — which is the only
/// reason it is worth exporting four words.
#[uniffi::export]
#[must_use]
pub fn export_dataset_name(dataset: ExportDataset) -> String {
    dataset.as_str().to_string()
}

/// A format's lowercase wire name, which is also its file extension.
/// See [`sunrise_domain::ExportFormat::as_str`].
#[uniffi::export]
#[must_use]
pub fn export_format_name(format: ExportFormat) -> String {
    format.as_str().to_string()
}

/// When `routine` next fires at or after `now_ms`, or `None` if nothing falls
/// inside its materialization horizon.
///
/// See [`sunrise_domain::routine_rows`], which is where the horizon lives: it
/// is per-frequency (`docs/02-domain/routines-and-recurrence.md`), so a yearly
/// routine still resolves while a daily one stays cheap. A client picking its
/// own lookahead would show "no next occurrence" for a yearly routine and be
/// wrong about it.
///
/// Skips, pauses and the series end are all honoured, because the whole
/// routine is handed over rather than just its rule.
#[uniffi::export]
#[must_use]
pub fn next_occurrence_ms(routine: RoutineItem, now_ms: u64) -> Option<i64> {
    let now = jiff::Timestamp::from_millisecond(i64::try_from(now_ms).ok()?).ok()?;
    let rows = sunrise_domain::routine_rows(&[sunrise_domain::Routine::from(routine)], now);
    rows.first()
        .and_then(|r| r.next)
        .map(jiff::Timestamp::as_millisecond)
}

/// A running session's numbers, as the domain derives them.
///
/// The core stores **no running timer**: a session is a `start` op, and every
/// number below is derived from it and the clock each time it is asked for.
/// That is why this is a function of `now_ms` rather than a field on
/// [`SessionRow`] — a row read a second ago is already wrong.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionProgress {
    /// Focused time: frozen once ended, derived while running.
    pub focused_ms: u64,
    /// `MM:SS`, widening past an hour — the live-timer form.
    pub clock: String,
    /// Time left against the plan; absent for an `until done` session.
    pub remaining_ms: Option<u64>,
    /// `remaining_ms` as a clock, when there is one.
    pub remaining_clock: Option<String>,
    /// Whether a planned session has run past its plan.
    pub overran: bool,
    /// Still running (a `start` with no `end` — a valid state).
    pub running: bool,
}

/// What a live focus timer should show for `session` at `now_ms`.
///
/// See [`sunrise_domain::FocusSession`]. Every number here is one of its
/// methods: `focused_ms`, `remaining_ms`, `overran`. A client ticking a timer
/// on its own would have to decide what "over" means against a plan and what
/// an open-ended session's remaining time is — two decisions the domain has
/// already made, and the second of which is "there isn't one", not zero.
#[uniffi::export]
#[must_use]
pub fn session_progress(session: SessionRow, now_ms: u64) -> SessionProgress {
    let live = sunrise_domain::FocusSession::from(&session);
    let remaining_ms = live.remaining_ms(now_ms);
    SessionProgress {
        focused_ms: live.focused_ms(now_ms),
        clock: sunrise_domain::fmt_duration_ms(live.focused_ms(now_ms)),
        remaining_ms,
        remaining_clock: remaining_ms.map(sunrise_domain::fmt_duration_ms),
        overran: live.overran(now_ms),
        running: live.is_running(),
    }
}

/// The synthetic Inbox stream's id.
///
/// `Query::StreamList` puts the Inbox first as an ordinary-looking row, and a
/// client has to tell it apart: it cannot be renamed, recoloured or deleted,
/// and offering those would produce a rejected command and a confused user.
///
/// Exported rather than hardcoded because the alternative is a client
/// comparing against the literal all-zero id — or worse, against the string
/// "Inbox", which is a display name and translatable.
#[uniffi::export]
#[must_use]
pub fn inbox_stream_id() -> sunrise_id::EntityRef {
    sunrise_domain::inbox_stream_ref()
}

/// An IANA zone, or UTC when the name is not one.
fn zone_or_utc(tz: &str) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(tz).unwrap_or(jiff::tz::TimeZone::UTC)
}

// ---------------------------------------------------------------------------
// Calendar conflicts
// ---------------------------------------------------------------------------

/// One shaded region on the calendar grid: two Blocks and the time they share.
///
/// See [`sunrise_domain::BlockOverlap`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockConflict {
    /// The earlier-starting Block.
    pub a: sunrise_id::EntityRef,
    /// The later-starting Block.
    pub b: sunrise_id::EntityRef,
    /// Start of the shared region (epoch ms).
    pub from_ms: i64,
    /// End of the shared region (epoch ms), exclusive.
    pub to_ms: i64,
}

/// Every overlapping pair among the rows a calendar grid is showing.
///
/// See [`sunrise_domain::overlaps`]. Exported rather than computed in the
/// client because "overlap" is a decision, not arithmetic: it is measured on
/// [`sunrise_domain::SunriseTime::index_ms`] so that the four time kinds
/// compare the way storage orders them, and back-to-back blocks are
/// deliberately *not* a conflict. A client comparing its own two numbers would
/// get the second of those wrong on its first well-planned day.
#[uniffi::export]
#[must_use]
pub fn block_conflicts(rows: Vec<BlockGridRow>) -> Vec<BlockConflict> {
    let blocks: Vec<sunrise_domain::Block> = rows.iter().map(|r| r.block.to_domain()).collect();
    sunrise_domain::overlaps(&blocks)
        .into_iter()
        .map(|o| BlockConflict {
            a: o.a,
            b: o.b,
            from_ms: o.from_ms,
            to_ms: o.to_ms,
        })
        .collect()
}

/// The draft the Resolve menu's **Merge** action creates.
///
/// `docs/02-domain/time-blocks.md` §Conflicts defines merge as "tombstone the
/// two original Blocks and create a new one with the union time range and
/// concatenated tasks". This is the create half; the caller submits the two
/// deletes alongside it.
///
/// The interesting part is not the union but **which time kind survives it**,
/// and that is a domain decision: two bounds of the same kind stay that kind,
/// two different kinds resolve to an instant rather than letting one silently
/// re-anchor the other's meaning on the next flight. See
/// [`sunrise_domain::merge_blocks`].
///
/// `tz` is the zone a floating or all-day bound resolves in when the kinds
/// disagree; an unknown one falls back to UTC, as everywhere else here.
///
/// # Errors
///
/// [`BindingError::Core`] when the union does not validate — in practice only
/// a merge that would bind two or more tasks with no title on either side,
/// since a multi-task Block has no single task title to shadow.
#[uniffi::export]
pub fn merged_block_draft(
    a: BlockGridRow,
    b: BlockGridRow,
    tz: String,
) -> Result<BlockDraftIn, BindingError> {
    let zone = zone_or_utc(&tz);
    let draft = sunrise_domain::merge_blocks(&a.block.to_domain(), &b.block.to_domain(), &zone)
        .map_err(|e| BindingError::Core(e.to_string()))?;
    Ok(BlockDraftIn {
        stream_id: draft.stream_id,
        starts_at: TimeValue::from(&draft.starts_at),
        ends_at: TimeValue::from(&draft.ends_at),
        title: draft.title,
        title_track_task: draft.title_track_task,
        tasks: draft.tasks,
    })
}

/// When a "defer an hour" / "until tomorrow" / "next week" lands, epoch ms.
///
/// See [`sunrise_domain::snooze_target`]. The three answers behind the action
/// buttons `docs/08-features/notifications.md` puts on every reminder, and the
/// reason they are not client arithmetic: "tomorrow" is a **date**, so adding
/// 86,400,000 ms is wrong twice a year — an hour early or an hour late across
/// a DST boundary, on exactly the reminders someone was relying on.
///
/// `tz` is the zone the civil arithmetic happens in; an unknown one falls back
/// to UTC, as everywhere else here.
#[uniffi::export]
#[must_use]
pub fn snooze_target_ms(from_ms: u64, span: SnoozeSpan, tz: String) -> i64 {
    let from = jiff::Timestamp::from_millisecond(i64::try_from(from_ms).unwrap_or(i64::MAX))
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
    sunrise_domain::snooze_target(from, span, &zone_or_utc(&tz)).as_millisecond()
}
