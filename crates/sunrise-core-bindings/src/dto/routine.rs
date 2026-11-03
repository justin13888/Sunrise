//! Routines and templates: what generates tasks on a schedule.

use sunrise_client_core::vocab::zone_or_utc;
use sunrise_domain::{
    Energy, NoteBody, RRule, Routine, RoutineCatchupPolicy, ScheduleConstraint, SunriseTime,
    TaskTemplate,
};
use sunrise_id::EntityRef;

use super::{
    extra_in, extra_out, task::patch_field, time::legacy_instant, Constraint, Recurrence, TimeValue,
};

/// See [`sunrise_domain::TaskTemplate`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct Template {
    /// Title of each generated task.
    pub title: String,
    /// Stream each generated task lands in.
    pub stream_id: EntityRef,
    /// Contexts each generated task carries.
    pub contexts: Vec<EntityRef>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// Body.
    pub body: Option<NoteBody>,
    /// Fields this build does not know, opaque; hand it back unchanged. See
    /// the module docs.
    #[uniffi(default = None)]
    pub extra: Option<Vec<u8>>,
}

impl From<&TaskTemplate> for Template {
    fn from(t: &TaskTemplate) -> Self {
        let TaskTemplate {
            title,
            stream_id,
            contexts,
            energy,
            priority,
            estimated_duration_s,
            body,
            unknown,
        } = t;
        Self {
            title: title.clone(),
            stream_id: sunrise_domain::stream_key(*stream_id),
            contexts: contexts.clone(),
            energy: energy.clone(),
            priority: *priority,
            estimated_duration_s: *estimated_duration_s,
            body: body.clone(),
            extra: extra_out(unknown),
        }
    }
}

impl From<Template> for TaskTemplate {
    fn from(t: Template) -> Self {
        Self {
            title: t.title,
            // The legacy Inbox id means no stream (ADR-0046 §5).
            stream_id: sunrise_domain::home_stream(Some(t.stream_id)),
            contexts: t.contexts,
            energy: t.energy,
            priority: t.priority,
            estimated_duration_s: t.estimated_duration_s,
            body: t.body,
            unknown: extra_in(t.extra.as_deref()),
        }
    }
}

/// See [`sunrise_domain::Routine`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineItem {
    /// Routine id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// The task this generates.
    pub template: Template,
    /// The recurrence.
    pub rrule: Recurrence,
    /// IANA zone the routine was set up in, and the zone a pinned anchor is
    /// in.
    pub timezone: String,
    /// The anchor as an instant: [`Self::starts_at_time`] resolved in
    /// [`Self::timezone`]. Kept for clients built before #506.
    pub starts_at: jiff::Timestamp,
    /// The series end as an instant, resolved like `starts_at`.
    pub ends_at: Option<jiff::Timestamp>,
    /// Scheduling constraints applied to generated tasks.
    pub scheduling_constraints: Vec<Constraint>,
    /// Occurrence keys the user skipped.
    pub skipped_keys: Vec<String>,
    /// What to do with occurrences missed while offline.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Current streak.
    pub streak_counter: i64,
    /// Last completion.
    pub last_completed_at: Option<jiff::Timestamp>,
    /// Grace window, in seconds.
    pub grace_window_s: Option<u64>,
    /// Whether a missed occurrence can be forgiven.
    pub forgiveness_enabled: bool,
    /// When the current streak began.
    pub streak_started_at: Option<jiff::Timestamp>,
    /// Forgivenesses used in the current window.
    pub forgivenesses_in_window: u32,
    /// Occurrence keys counted toward the streak.
    pub streak_keys: Vec<String>,
    /// Paused (generates nothing).
    pub paused: bool,
    /// Paused until this instant, resolved like `starts_at`.
    pub paused_until: Option<jiff::Timestamp>,
    /// Archived.
    pub archived: bool,
    /// Tombstoned.
    pub deleted: bool,
    /// The anchor with its kind: `floating` (follows the reader, the
    /// default) or `zoned` (pinned). Always set by the core; when a client
    /// sets it on a routine it hands back, it wins over `starts_at`.
    #[uniffi(default = None)]
    pub starts_at_time: Option<TimeValue>,
    /// The series end with its kind (`floating` or `all_day`).
    #[uniffi(default = None)]
    pub ends_at_time: Option<TimeValue>,
    /// The pause expiry with its kind.
    #[uniffi(default = None)]
    pub paused_until_time: Option<TimeValue>,
    /// The last completion, with its kind (an instant).
    #[uniffi(default = None)]
    pub last_completed_at_time: Option<TimeValue>,
    /// The root of the series this routine continues, when an edit "this
    /// and future" split it off (#331); none for a routine never split off.
    #[uniffi(default = None)]
    pub split_from: Option<EntityRef>,
}

impl From<&Routine> for RoutineItem {
    fn from(r: &Routine) -> Self {
        let Routine {
            id,
            created_at,
            updated_at,
            template,
            rrule,
            timezone,
            starts_at,
            ends_at,
            scheduling_constraints,
            // Dated for removal; `skipped_keys` is the live representation.
            skip_dates: _,
            skipped_keys,
            catchup_policy,
            streak_counter,
            last_completed_at,
            grace_window_s,
            forgiveness_enabled,
            streak_started_at,
            forgivenesses_in_window,
            streak_keys,
            paused,
            paused_until,
            archived,
            deleted,
            split_from,
            unknown: _,
        } = r;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            template: Template::from(template),
            rrule: Recurrence::from(rrule),
            timezone: timezone.clone(),
            starts_at: legacy_instant(starts_at, timezone),
            ends_at: ends_at.as_ref().map(|t| legacy_instant(t, timezone)),
            scheduling_constraints: scheduling_constraints
                .iter()
                .map(Constraint::from)
                .collect(),
            skipped_keys: skipped_keys.clone(),
            catchup_policy: catchup_policy.clone(),
            streak_counter: *streak_counter,
            last_completed_at: last_completed_at
                .as_ref()
                .map(|t| legacy_instant(t, timezone)),
            grace_window_s: *grace_window_s,
            forgiveness_enabled: *forgiveness_enabled,
            streak_started_at: *streak_started_at,
            forgivenesses_in_window: *forgivenesses_in_window,
            streak_keys: streak_keys.clone(),
            paused: *paused,
            paused_until: paused_until.as_ref().map(|t| legacy_instant(t, timezone)),
            archived: *archived,
            deleted: *deleted,
            starts_at_time: Some(TimeValue::from(starts_at)),
            ends_at_time: ends_at.as_ref().map(TimeValue::from),
            paused_until_time: paused_until.as_ref().map(TimeValue::from),
            last_completed_at_time: last_completed_at.as_ref().map(TimeValue::from),
            split_from: *split_from,
        }
    }
}

/// Back to the domain's own [`Routine`], so the projections that take one can
/// be asked about a routine a client is holding.
///
/// Written out field by field rather than kept as an opaque handle, because
/// the round trip is what makes `next_occurrence_ms` honest: it answers about
/// exactly the routine on screen, including the skips.
impl From<RoutineItem> for Routine {
    fn from(r: RoutineItem) -> Self {
        Self {
            id: r.id,
            created_at: r.created_at,
            updated_at: r.updated_at,
            template: TaskTemplate::from(r.template),
            rrule: RRule::from(r.rrule),
            timezone: r.timezone,
            starts_at: r
                .starts_at_time
                .map_or_else(|| r.starts_at.into(), SunriseTime::from),
            ends_at: r
                .ends_at_time
                .map(SunriseTime::from)
                .or_else(|| r.ends_at.map(Into::into)),
            scheduling_constraints: r
                .scheduling_constraints
                .into_iter()
                .map(ScheduleConstraint::from)
                .collect(),
            // Dated for removal at `DOC_SCHEMA_FLOOR = 3`; nothing writes a
            // new entry, and `skipped_keys` is the live representation.
            skip_dates: Vec::new(),
            skipped_keys: r.skipped_keys,
            catchup_policy: r.catchup_policy,
            streak_counter: r.streak_counter,
            last_completed_at: r
                .last_completed_at_time
                .map(SunriseTime::from)
                .or_else(|| r.last_completed_at.map(Into::into)),
            grace_window_s: r.grace_window_s,
            forgiveness_enabled: r.forgiveness_enabled,
            streak_started_at: r.streak_started_at,
            forgivenesses_in_window: r.forgivenesses_in_window,
            streak_keys: r.streak_keys,
            paused: r.paused,
            paused_until: r
                .paused_until_time
                .map(SunriseTime::from)
                .or_else(|| r.paused_until.map(Into::into)),
            archived: r.archived,
            deleted: r.deleted,
            split_from: r.split_from,
            unknown: sunrise_domain::Unknowns::new(),
        }
        .normalized()
    }
}

/// See [`sunrise_domain::RoutineDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineDraftIn {
    /// The task to generate.
    pub template: Template,
    /// The recurrence.
    pub rrule: Recurrence,
    /// IANA zone the routine is set up in, and the zone a pinned anchor is
    /// pinned to.
    pub timezone: String,
    /// Anchor as an instant. When [`Self::anchor`] is not set, the routine
    /// is `floating` at this instant's wall-clock time in [`Self::timezone`]:
    /// a routine whose client named no zone follows the traveller
    /// (ADR-0066 §1).
    pub starts_at: jiff::Timestamp,
    /// Series end as an instant, read as `floating` at its wall clock in
    /// `timezone` when [`Self::ends_at_time`] is not set.
    pub ends_at: Option<jiff::Timestamp>,
    /// Scheduling constraints for generated tasks.
    pub scheduling_constraints: Vec<Constraint>,
    /// What to do with occurrences missed while offline.
    pub catchup_policy: RoutineCatchupPolicy,
    /// The anchor with its kind: `floating`, or `zoned` in `timezone` to pin
    /// it. Wins over `starts_at`.
    #[uniffi(default = None)]
    pub anchor: Option<TimeValue>,
    /// The series end with its kind (`floating` or `all_day`). Wins over
    /// `ends_at`.
    #[uniffi(default = None)]
    pub ends_at_time: Option<TimeValue>,
}

/// An instant's wall-clock time in `tz`, as a floating value: what a client
/// that sends an instant and a zone and no kind means by a routine time.
fn floating_in(at: jiff::Timestamp, tz: &str) -> SunriseTime {
    SunriseTime::floating(at.to_zoned(zone_or_utc(tz)).datetime())
}

impl From<RoutineDraftIn> for sunrise_domain::RoutineDraft {
    fn from(d: RoutineDraftIn) -> Self {
        let starts_at = d
            .anchor
            .map_or_else(|| floating_in(d.starts_at, &d.timezone), SunriseTime::from);
        let ends_at = d
            .ends_at_time
            .map(SunriseTime::from)
            .or_else(|| d.ends_at.map(|e| floating_in(e, &d.timezone)));
        Self {
            template: d.template.into(),
            rrule: d.rrule.into(),
            timezone: d.timezone,
            starts_at,
            ends_at,
            scheduling_constraints: d
                .scheduling_constraints
                .into_iter()
                .map(ScheduleConstraint::from)
                .collect(),
            catchup_policy: d.catchup_policy,
        }
    }
}

/// An edit to a routine. Split-optional, like [`TaskEdit`](super::TaskEdit).
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct RoutineEdit {
    /// Replace the whole template.
    #[uniffi(default = None)]
    pub template: Option<Template>,
    /// Replace the recurrence.
    #[uniffi(default = None)]
    pub rrule: Option<Recurrence>,
    /// New IANA zone.
    #[uniffi(default = None)]
    pub timezone: Option<String>,
    /// New anchor, as an instant: read as `zoned` in the routine's zone at
    /// its wall clock there. Prefer [`Self::set_starts_at_time`].
    #[uniffi(default = None)]
    pub starts_at: Option<jiff::Timestamp>,
    /// New series end.
    #[uniffi(default = None)]
    pub set_ends_at: Option<jiff::Timestamp>,
    /// Remove the series end.
    #[uniffi(default = false)]
    pub clear_ends_at: bool,
    /// Replace the constraint list.
    #[uniffi(default = None)]
    pub scheduling_constraints: Option<Vec<Constraint>>,
    /// New catch-up policy.
    #[uniffi(default = None)]
    pub catchup_policy: Option<RoutineCatchupPolicy>,
    /// New grace window, in seconds.
    #[uniffi(default = None)]
    pub set_grace_window_s: Option<u64>,
    /// Clear the grace window.
    #[uniffi(default = false)]
    pub clear_grace_window: bool,
    /// Enable or disable forgiveness.
    #[uniffi(default = None)]
    pub forgiveness_enabled: Option<bool>,
    /// Pause or resume.
    #[uniffi(default = None)]
    pub paused: Option<bool>,
    /// Pause until this instant.
    #[uniffi(default = None)]
    pub set_paused_until: Option<jiff::Timestamp>,
    /// Clear the pause deadline.
    #[uniffi(default = false)]
    pub clear_paused_until: bool,
    /// Archive or unarchive.
    #[uniffi(default = None)]
    pub archived: Option<bool>,
    /// New anchor with its kind: `floating` unpins the routine, `zoned` in
    /// its zone pins it. Wins over `starts_at`.
    #[uniffi(default = None)]
    pub set_starts_at_time: Option<TimeValue>,
    /// New series end with its kind. Wins over `set_ends_at`.
    #[uniffi(default = None)]
    pub set_ends_at_time: Option<TimeValue>,
    /// New pause expiry with its kind (`all_day` for "until Monday"). Wins
    /// over `set_paused_until`.
    #[uniffi(default = None)]
    pub set_paused_until_time: Option<TimeValue>,
    /// How far the edit reaches (#331); the whole series when absent.
    #[uniffi(default = None)]
    pub scope: Option<EditScopeIn>,
}

/// How far a routine edit reaches; see [`sunrise_domain::EditScope`]. Each
/// occurrence is named by its key, `YYYY-MM-DDTHH:MM`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum EditScopeIn {
    /// That occurrence's task only.
    This {
        /// The occurrence.
        key: String,
    },
    /// The occurrence and every one after it: the series is split there.
    ThisAndFuture {
        /// The first occurrence the edit applies to.
        key: String,
    },
    /// The whole series.
    All,
}

impl From<EditScopeIn> for sunrise_domain::EditScope {
    fn from(s: EditScopeIn) -> Self {
        match s {
            EditScopeIn::This { key } => Self::This { key },
            EditScopeIn::ThisAndFuture { key } => Self::ThisAndFuture { key },
            EditScopeIn::All => Self::All,
        }
    }
}

impl From<RoutineEdit> for sunrise_domain::RoutinePatch {
    fn from(e: RoutineEdit) -> Self {
        let set_ends = e
            .set_ends_at_time
            .map(SunriseTime::from)
            .or_else(|| e.set_ends_at.map(Into::into));
        let set_pause = e
            .set_paused_until_time
            .map(SunriseTime::from)
            .or_else(|| e.set_paused_until.map(Into::into));
        Self {
            template: e.template.map(TaskTemplate::from),
            rrule: e.rrule.map(RRule::from),
            timezone: e.timezone,
            starts_at: e
                .set_starts_at_time
                .map(SunriseTime::from)
                .or_else(|| e.starts_at.map(Into::into)),
            ends_at: patch_field(set_ends, e.clear_ends_at),
            scheduling_constraints: e
                .scheduling_constraints
                .map(|l| l.into_iter().map(ScheduleConstraint::from).collect()),
            catchup_policy: e.catchup_policy,
            grace_window_s: patch_field(e.set_grace_window_s, e.clear_grace_window),
            forgiveness_enabled: e.forgiveness_enabled,
            paused: e.paused,
            paused_until: patch_field(set_pause, e.clear_paused_until),
            archived: e.archived,
        }
    }
}
