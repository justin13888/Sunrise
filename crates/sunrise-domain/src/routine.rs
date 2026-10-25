//! Routine entity per `docs/02-domain/routines-and-recurrence.md`.

use crate::common::{Energy, NoteBody};
use crate::constraint::{validate_list as validate_constraint_list, ScheduleConstraint};
use crate::rrule::RRule;
use crate::task::TaskDraft;
use crate::time::SunriseTime;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{validate_title, ValidationError, MAX_TASK_TITLE_LEN};
use jiff::civil;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// What to do when an occurrence is missed.
///
/// An unrecognised policy reads as [`RoutineCatchupPolicy::Skip`] and is
/// written back verbatim. The least surprising reading: an unknown policy must
/// not materialize a backlog of missed occurrences the user never asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutineCatchupPolicy {
    /// Skip missed occurrences silently.
    Skip,
    /// Merge missed occurrences into a single "catch up" task.
    Merge,
    /// Queue every missed occurrence as a separate task.
    Queue,
    /// A policy this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(RoutineCatchupPolicy, fallback = Skip, {
    Skip => "skip",
    Merge => "merge",
    Queue => "queue",
});

/// Review cadence for routines (mirrors Stream cadences).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineReviewCadence {
    /// Review weekly.
    Weekly,
    /// Review monthly.
    Monthly,
    /// No reviews.
    None,
}

/// Task template embedded in a Routine. Used to materialize occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTemplate {
    /// Title (required).
    pub title: String,
    /// The Stream each occurrence is created in, or `None` for occurrences
    /// that belong to no stream (ADR-0046 §1). The routine itself lives in
    /// the vault-meta domain whatever this says, so re-pointing it is an
    /// ordinary write. On the wire `None` is the legacy Inbox reference
    /// ([`crate::inbox::wire`]).
    #[serde(default, with = "crate::inbox::wire")]
    pub stream_id: Option<EntityRef>,
    /// Optional contexts.
    #[serde(default)]
    pub contexts: Vec<EntityRef>,
    /// Optional energy facet.
    #[serde(default)]
    pub energy: Option<Energy>,
    /// Optional priority 1..=5.
    #[serde(default)]
    pub priority: Option<u8>,
    /// Optional duration in seconds.
    #[serde(default)]
    pub estimated_duration_s: Option<u64>,
    /// Optional body.
    #[serde(default)]
    pub body: Option<NoteBody>,
    /// Fields this build does not know, re-emitted verbatim. See
    /// [`crate::unknown`]. They stay on the template; an occurrence is a new
    /// Task this build writes, and it cannot say which of them apply to one.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl TaskTemplate {
    /// Build a `TaskDraft` for this occurrence.
    ///
    /// `planned_at` is the occurrence's planned time, of the anchor's kind
    /// ([`Routine::occurrence_time`]): a floating routine's occurrence is
    /// floating at its civil key, a zoned one's zoned in the anchor's zone.
    /// An occurrence is never an instant
    /// (`docs/11-adr/0066-times-survive-travel.md` §1).
    #[must_use]
    pub fn to_draft(&self, planned_at: Option<SunriseTime>) -> TaskDraft {
        TaskDraft {
            title: self.title.clone(),
            body: self.body.clone(),
            stream_id: self.stream_id,
            contexts: self.contexts.clone(),
            priority: self.priority,
            energy: self.energy.clone(),
            estimated_duration_s: self.estimated_duration_s,
            planned_at,
            target_at: None,
            hard_due_at: None,
            scheduling_constraints: Vec::new(),
            assignee: None,
            // Routine occurrences inherit the Stream default, like any other
            // task; a per-occurrence lead time would have to be re-set on
            // every generated instance.
            reminder_lead_s: None,
        }
    }
}

/// serde `default` for [`Routine::forgiveness_enabled`] (the rule is on by
/// default, per `docs/02-domain/routines-and-recurrence.md`).
const fn default_true() -> bool {
    true
}

/// serde `skip_serializing_if` companion to [`default_true`].
///
/// serde hands `skip_serializing_if` a reference, so the by-reference signature
/// is forced here rather than chosen.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(b: &bool) -> bool {
    *b
}

/// serde `skip_serializing_if` for counters that default to zero. Same
/// by-reference signature requirement as [`is_true`].
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Persisted Routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    /// Routine id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Task template materialized per occurrence.
    pub template: TaskTemplate,
    /// Recurrence rule.
    pub rrule: RRule,
    /// IANA timezone string (e.g., `"America/Los_Angeles"`): the zone the
    /// routine was set up in, and the zone a zoned anchor is pinned to.
    ///
    /// A zoned anchor names its own zone, which is kept equal to this one
    /// ([`Routine::rezone`]). A floating anchor has no zone; for it this is
    /// only where the routine was created, and the zone an instant written
    /// before `DOC_SCHEMA_V` 15 is read in ([`Routine::normalize_legacy_times`]).
    pub timezone: String,
    /// The anchor: the first occurrence's wall-clock time.
    ///
    /// `floating` (the default: "daily at 08:00" wherever the user is) or
    /// `zoned` (the user pinned a zone, or an import carried a `TZID`),
    /// `docs/11-adr/0066-times-survive-travel.md` §1. A bare instant from a
    /// build before `DOC_SCHEMA_V` 15 means `zoned` in [`Self::timezone`].
    #[serde(with = "crate::time::compat")]
    pub starts_at: SunriseTime,
    /// Optional inclusive end: `floating` or `all_day`, compared with
    /// occurrence keys in civil space (`docs/10-cross-cutting/time.md` §6).
    #[serde(default, with = "crate::time::compat::option")]
    pub ends_at: Option<SunriseTime>,
    /// Scheduling constraints (value list; whole list is one LWW register).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduling_constraints: Vec<ScheduleConstraint>,
    /// Dates explicitly skipped, as instants.
    ///
    /// **Deprecated; remove at `DOC_SCHEMA_FLOOR = 3`.** A live dual of
    /// [`Self::skipped_keys`], which supersedes it: a key
    /// (`YYYY-MM-DDTHH:MM`) is tzdb-drift-immune, while an instant has to be
    /// re-resolved against the routine's timezone on every read and silently
    /// stops matching its occurrence when that zone's rules change. Two
    /// representations of "skipped" is one more than can be kept in agreement.
    ///
    /// It survives now only to read chrono-era payloads and iCal `EXDATE`
    /// imports. Removal is a doc-schema change, so it follows the two-stage
    /// rule in `docs/04-storage/migrations.md` §CRDT doc-schema migrations:
    /// stage A (stop reading) is done — nothing writes a new entry here, every
    /// new skip goes to `skipped_keys` — and stage B (stop writing, drop the
    /// field) lands when `DOC_SCHEMA_FLOOR` reaches 3, which cannot happen
    /// before every device has been on a build that writes keys.
    #[serde(default)]
    pub skip_dates: Vec<Timestamp>,
    /// Occurrence keys (`YYYY-MM-DDTHH:MM`) explicitly skipped via
    /// `SkipRoutineOccurrence`. Preferred over [`Self::skip_dates`] for new
    /// skips: a key is tzdb-drift-immune and needs no instant re-resolution.
    /// Defaults to empty and is omitted on the wire, keeping the chrono-era
    /// fixtures byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_keys: Vec<String>,
    /// What to do when an occurrence is missed.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Streak counter (signed for safety). Merged as a register today;
    /// ADR-0044 §3 derives it from `streak_keys` and `skipped_keys` instead
    /// (#331), because a reset is not a commutative counter operation.
    #[serde(default)]
    pub streak_counter: i64,
    /// Last successful completion (for streak grace): a recorded moment, so
    /// an instant.
    #[serde(default, with = "crate::time::compat::option")]
    pub last_completed_at: Option<SunriseTime>,
    /// Completion grace window in seconds. `None` = the spec default of 24h
    /// ([`crate::streak::DEFAULT_GRACE_WINDOW_S`]); clamped to
    /// [`crate::streak::MAX_GRACE_WINDOW_S`] on read. Absent on the wire when
    /// unset, so pre-streak fixtures still round-trip byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_window_s: Option<u64>,
    /// Forgiveness rule (one missed occurrence per 30-day window does not reset
    /// the streak). Enabled by default; only the disabled case hits the wire.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub forgiveness_enabled: bool,
    /// Anchor of the current streak *and* of its 30-day forgiveness window.
    /// See [`crate::streak`] for the sliding rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streak_started_at: Option<Timestamp>,
    /// Forgivenesses consumed since [`Self::streak_started_at`]; reset to 0 on
    /// every anchor advance.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub forgivenesses_in_window: u32,
    /// Idempotency keys of the occurrences already counted toward the streak,
    /// sorted. Entries are occurrence keys (`YYYY-MM-DDTHH:MM`); the full key
    /// in the spec is this routine's id joined with the entry. Membership is
    /// permanent (no GC), per `docs/08-features/recurrence-engine.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streak_keys: Vec<String>,
    /// Paused.
    #[serde(default)]
    pub paused: bool,
    /// Pause expiry, of any kind: while [`Self::paused`] is set, an
    /// occurrence before it is not generated, and one at or after it is.
    /// `all_day` ("pause until Monday") resumes at the start of that day in
    /// the reader's zone. `None` pauses until unpaused.
    #[serde(default, with = "crate::time::compat::option")]
    pub paused_until: Option<SunriseTime>,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// The root of the series this routine continues, when an edit of
    /// another routine "this and future" created it
    /// (`docs/02-domain/routines-and-recurrence.md` §Edit scope, #331).
    /// `None` for a routine that was never split off: it is its own root.
    ///
    /// It names the series' root, the first routine of the chain, not the
    /// routine split: the root keys every occurrence's task id
    /// ([`Routine::series_root`]), and naming it directly means no reader has
    /// to walk a chain whose links may not have arrived yet. Written once, at
    /// the split. Absent on the wire when `None`, so every routine that was
    /// never split encodes as it did before `DOC_SCHEMA_V` 22; an op carrying
    /// it uses the feature `routine.series`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_from: Option<EntityRef>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Draft used by the UI when creating a Routine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineDraft {
    /// Task template.
    pub template: TaskTemplate,
    /// RRULE body (parsed from the user's input).
    pub rrule: RRule,
    /// IANA timezone: where the routine is set up, and the zone a zoned
    /// anchor is pinned to.
    pub timezone: String,
    /// The anchor: `floating` unless the user pinned a zone, in which case
    /// `zoned` in [`Self::timezone`]. An instant means `zoned` in
    /// [`Self::timezone`] at its wall clock there ([`read_legacy_instant`]);
    /// any other kind is refused.
    pub starts_at: SunriseTime,
    /// Optional inclusive end (`floating` or `all_day`).
    pub ends_at: Option<SunriseTime>,
    /// Optional scheduling constraints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduling_constraints: Vec<ScheduleConstraint>,
    /// Catchup policy.
    pub catchup_policy: RoutineCatchupPolicy,
}

impl RoutineDraft {
    /// Validate the draft: title shape, timezone resolvability, constraint
    /// list, and `ends_at ≥ starts_at`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(
            &self.template.title,
            "routine.template.title",
            MAX_TASK_TITLE_LEN,
        )?;
        if jiff::tz::TimeZone::get(&self.timezone).is_err() {
            return Err(ValidationError::Field {
                field: "routine.timezone",
                constraint: "iana_timezone",
            });
        }
        validate_anchor(&self.starts_at, &self.timezone)?;
        validate_end(self.ends_at.as_ref(), &self.starts_at, &self.timezone)?;
        validate_constraint_list(&self.scheduling_constraints)?;
        Ok(())
    }
}

/// An anchor is `floating`, or `zoned` in the routine's own zone. An instant
/// is accepted and means what a legacy one does ([`read_legacy_instant`]):
/// `zoned` in the routine's zone at that instant's wall clock.
fn validate_anchor(anchor: &SunriseTime, timezone: &str) -> Result<(), ValidationError> {
    match anchor {
        SunriseTime::Floating { .. } | SunriseTime::Instant { .. } => Ok(()),
        SunriseTime::Zoned { tz, .. } if tz == timezone => Ok(()),
        SunriseTime::Zoned { .. } => Err(ValidationError::Field {
            field: "routine.starts_at",
            constraint: "zone_is_timezone",
        }),
        _ => Err(ValidationError::Field {
            field: "routine.starts_at",
            constraint: "floating_or_zoned",
        }),
    }
}

/// An end is not before the anchor, compared in civil space.
fn validate_end(
    end: Option<&SunriseTime>,
    anchor: &SunriseTime,
    timezone: &str,
) -> Result<(), ValidationError> {
    let Some(end) = end else {
        return Ok(());
    };
    let Some(start) = civil_of(anchor, timezone) else {
        return Ok(());
    };
    let before = match end {
        SunriseTime::AllDay { date } => *date < start.date(),
        SunriseTime::Unknown { .. } => false,
        _ => civil_of(end, timezone).is_some_and(|e| e < start),
    };
    if before {
        return Err(ValidationError::Field {
            field: "routine.ends_at",
            constraint: "after_starts_at",
        });
    }
    Ok(())
}

/// The wall-clock value of a routine time, read in the routine's civil
/// frame: its own civil value for a civil kind, an instant's wall clock in
/// `timezone` (the legacy reading), midnight of an all-day date.
fn civil_of(t: &SunriseTime, timezone: &str) -> Option<civil::DateTime> {
    match t {
        SunriseTime::Zoned { civil, .. } | SunriseTime::Floating { civil } => Some(*civil),
        SunriseTime::AllDay { date } => Some(date.to_datetime(civil::Time::midnight())),
        SunriseTime::Instant { at } => {
            let zone = TimeZone::get(timezone).unwrap_or(TimeZone::UTC);
            Some(at.to_zoned(zone).datetime())
        }
        SunriseTime::Unknown { .. } => None,
    }
}

/// How a bare instant written to a routine anchor, end or `UNTIL` before
/// `DOC_SCHEMA_V` 15 reads (#506, `docs/10-cross-cutting/time.md` §1): as
/// `zoned` at its wall-clock time in the routine's `timezone`, which is what
/// expansion always made of it, so an existing routine keeps firing when it
/// did. When `timezone` names no zone this build knows, as `floating` at the
/// instant's wall clock in UTC: there is no zone to pin it to, and UTC is the
/// frame the instant was written in. Any other value is returned unchanged.
#[must_use]
pub fn read_legacy_instant(t: SunriseTime, timezone: &str) -> SunriseTime {
    let SunriseTime::Instant { at } = t else {
        return t;
    };
    match TimeZone::get(timezone) {
        Ok(zone) => SunriseTime::zoned(at.to_zoned(zone).datetime(), timezone),
        Err(_) => SunriseTime::floating(at.to_zoned(TimeZone::UTC).datetime()),
    }
}

/// `t` as the bare instant a build before `DOC_SCHEMA_V` 15 reads, when that
/// instant reads back as exactly `t` ([`read_legacy_instant`]); `None` when
/// it does not.
///
/// A `zoned` value in the routine's own zone has one, unless its wall-clock
/// time is in a DST gap: the gap moves it, and the moved time would read
/// back. An instant is its own. No other kind has one.
fn legacy_instant_of(t: &SunriseTime, timezone: &str) -> Option<SunriseTime> {
    match t {
        SunriseTime::Instant { .. } => Some(t.clone()),
        SunriseTime::Zoned { civil, tz } if tz == timezone => {
            let zone = TimeZone::get(tz).ok()?;
            let at = zone.to_zoned(*civil).ok()?.timestamp();
            (at.to_zoned(zone).datetime() == *civil).then_some(SunriseTime::instant(at))
        }
        _ => None,
    }
}

/// Patch applied via `Command::UpdateRoutine`. `Some(None)` clears an optional
/// field; `None` leaves it unchanged (mirrors [`crate::task::TaskPatch`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutinePatch {
    /// Replace the task template.
    pub template: Option<TaskTemplate>,
    /// Replace the recurrence rule (triggers regeneration of future tasks).
    pub rrule: Option<RRule>,
    /// Replace the IANA timezone string.
    pub timezone: Option<String>,
    /// Replace the anchor (`floating`, or `zoned` in the routine's zone).
    /// A `floating` anchor unpins the routine; a `zoned` one pins it.
    pub starts_at: Option<SunriseTime>,
    /// Set/clear the inclusive end.
    pub ends_at: Option<Option<SunriseTime>>,
    /// Replace the whole scheduling-constraints list (LWW).
    pub scheduling_constraints: Option<Vec<ScheduleConstraint>>,
    /// Replace the catchup policy.
    pub catchup_policy: Option<RoutineCatchupPolicy>,
    /// Set/clear the streak grace window in seconds (`Some(None)` restores the
    /// 24h default). Clamped to [`crate::streak::MAX_GRACE_WINDOW_S`].
    pub grace_window_s: Option<Option<u64>>,
    /// Toggle the streak forgiveness rule.
    pub forgiveness_enabled: Option<bool>,
    /// Pause / unpause.
    pub paused: Option<bool>,
    /// Set/clear the pause expiry.
    pub paused_until: Option<Option<SunriseTime>>,
    /// Archive / unarchive.
    pub archived: Option<bool>,
}

impl RoutinePatch {
    /// Validate the patch's individual fields.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(t) = &self.template {
            let _ = validate_title(&t.title, "routine.template.title", MAX_TASK_TITLE_LEN)?;
        }
        if let Some(tz) = &self.timezone {
            if jiff::tz::TimeZone::get(tz).is_err() {
                return Err(ValidationError::Field {
                    field: "routine.timezone",
                    constraint: "iana_timezone",
                });
            }
        }
        if let Some(list) = &self.scheduling_constraints {
            validate_constraint_list(list)?;
        }
        if let Some(SunriseTime::AllDay { .. } | SunriseTime::Unknown { .. }) = &self.starts_at {
            return Err(ValidationError::Field {
                field: "routine.starts_at",
                constraint: "floating_or_zoned",
            });
        }
        Ok(())
    }
}

impl Routine {
    /// Read the times a build before `DOC_SCHEMA_V` 15 wrote as bare
    /// instants by [`read_legacy_instant`]: the anchor, the end and the
    /// rule's `UNTIL`. A routine that has none is unchanged.
    ///
    /// Decoding keeps the wire value as it arrived; this is what it means.
    /// Every reader that acts on a routine reads it through here: the
    /// engine's row writer and reader, and [`Routine::occurrences_in`].
    /// `paused_until` and `last_completed_at` allow the instant kind, so
    /// theirs stay instants.
    pub fn normalize_legacy_times(&mut self) {
        let tz = self.timezone.clone();
        self.starts_at = read_legacy_instant(self.starts_at.clone(), &tz);
        self.ends_at = self.ends_at.take().map(|t| read_legacy_instant(t, &tz));
        self.rrule.until = self.rrule.until.take().map(|t| read_legacy_instant(t, &tz));
    }

    /// [`Routine::normalize_legacy_times`], by value.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.normalize_legacy_times();
        self
    }

    /// Whether the anchor follows the reader (`floating`).
    #[must_use]
    pub const fn is_floating(&self) -> bool {
        matches!(self.starts_at, SunriseTime::Floating { .. })
    }

    /// The anchor's wall-clock time, read as [`Routine::normalize_legacy_times`]
    /// reads it; `None` for a kind this build does not know.
    #[must_use]
    pub fn anchor_civil(&self) -> Option<civil::DateTime> {
        civil_of(&self.starts_at, &self.timezone)
    }

    /// The zone this routine's occurrences resolve in for a reader in
    /// `reader` (`docs/10-cross-cutting/time.md` §6): the anchor's own zone
    /// when it is `zoned`, the reader's when it is `floating`.
    ///
    /// A zoned anchor naming a zone this build does not know resolves in the
    /// reader's zone, as every such value does (time.md §3). A legacy instant
    /// anchor resolves in `timezone`, or the reader's when that does not
    /// resolve either.
    #[must_use]
    pub fn occurrence_zone(&self, reader: &TimeZone) -> TimeZone {
        match &self.starts_at {
            SunriseTime::Zoned { tz, .. } => TimeZone::get(tz).unwrap_or_else(|_| reader.clone()),
            SunriseTime::Instant { .. } => {
                TimeZone::get(&self.timezone).unwrap_or_else(|_| reader.clone())
            }
            _ => reader.clone(),
        }
    }

    /// The planned time of the occurrence keyed at wall-clock `civil`: of the
    /// anchor's kind (`docs/11-adr/0066-times-survive-travel.md` §1), so a
    /// floating routine's occurrence follows the reader and a zoned one's
    /// stays in its zone. Never an instant.
    #[must_use]
    pub fn occurrence_time(&self, civil: civil::DateTime) -> SunriseTime {
        match &self.starts_at {
            SunriseTime::Zoned { tz, .. } => SunriseTime::zoned(civil, tz.clone()),
            SunriseTime::Instant { .. } => SunriseTime::zoned(civil, self.timezone.clone()),
            _ => SunriseTime::floating(civil),
        }
    }

    /// Move the routine to zone `tz`, keeping every wall-clock time it holds
    /// (`docs/11-adr/0066-times-survive-travel.md` §1): a zoned anchor, end
    /// or `UNTIL` in the old zone is re-pinned to `tz` at the same civil
    /// time, so "08:00 Los Angeles" becomes "08:00 New York", and no
    /// occurrence key changes. A floating anchor stays floating.
    pub fn rezone(&mut self, tz: String) {
        self.normalize_legacy_times();
        let old = std::mem::replace(&mut self.timezone, tz);
        let new = self.timezone.clone();
        let repin = |t: &mut SunriseTime| {
            if let SunriseTime::Zoned { tz: z, .. } = t {
                if *z == old {
                    z.clone_from(&new);
                }
            }
        };
        repin(&mut self.starts_at);
        if let Some(e) = self.ends_at.as_mut() {
            repin(e);
        }
        if let Some(u) = self.rrule.until.as_mut() {
            repin(u);
        }
    }

    /// Pin a floating anchor to [`Self::timezone`] at the same wall-clock
    /// time; a zoned anchor is unchanged. What a new routine falls back to in
    /// a vault where a paired device cannot read a floating one yet.
    pub fn rezone_pinned(&mut self) {
        if let SunriseTime::Floating { civil } = self.starts_at {
            self.starts_at = SunriseTime::zoned(civil, self.timezone.clone());
        }
    }

    /// The checks a draft's times get ([`RoutineDraft::validate`]), for a
    /// routine an edit produced: the anchor is `floating` or `zoned` in the
    /// routine's zone, and the end is not before it.
    ///
    /// # Errors
    /// [`ValidationError::Field`] naming the time that fails.
    pub fn validate_times(&self) -> Result<(), ValidationError> {
        let r = self.clone().normalized();
        validate_anchor(&r.starts_at, &r.timezone)?;
        validate_end(r.ends_at.as_ref(), &r.starts_at, &r.timezone)
    }

    /// This routine in the wire form a build before `DOC_SCHEMA_V` 15 reads,
    /// or `None` when it holds a time that form cannot carry.
    ///
    /// Each [`crate::time::compat`] field is written as the bare instant it
    /// was, where that instant reads back as exactly this value
    /// ([`read_legacy_instant`]): a zoned anchor, end or `UNTIL` in the
    /// routine's own zone, and an instant `paused_until` or
    /// `last_completed_at`. A floating or all-day value has no such instant,
    /// and neither has a zoned wall-clock time in a DST gap. The engine
    /// writes this form while the vault does not require
    /// `routine.civil_times`, so a routine every device can read stays
    /// readable by every device.
    #[must_use]
    pub fn legacy_wire_form(&self) -> Option<Self> {
        let tz = self.timezone.as_str();
        let instant_only = |t: &SunriseTime| match t {
            SunriseTime::Instant { .. } => Some(t.clone()),
            _ => None,
        };
        let mut out = self.clone();
        out.starts_at = legacy_instant_of(&self.starts_at, tz)?;
        out.ends_at = match &self.ends_at {
            Some(t) => Some(legacy_instant_of(t, tz)?),
            None => None,
        };
        out.rrule.until = match &self.rrule.until {
            Some(t) => Some(legacy_instant_of(t, tz)?),
            None => None,
        };
        out.paused_until = match &self.paused_until {
            Some(t) => Some(instant_only(t)?),
            None => None,
        };
        out.last_completed_at = match &self.last_completed_at {
            Some(t) => Some(instant_only(t)?),
            None => None,
        };
        Some(out)
    }
}

/// Per-FREQ materialization horizon (per spec: DAILY=14d, WEEKLY=60d,
/// MONTHLY=180d, YEARLY=540d). Returned in days.
///
/// An unknown frequency gets the shortest horizon: its rule expands to nothing
/// anyway, so a longer one would only be a longer scan of nothing.
#[must_use]
pub fn materialization_horizon_days(freq: &crate::rrule::Frequency) -> u32 {
    match freq {
        crate::rrule::Frequency::Daily | crate::rrule::Frequency::Unknown(_) => 14,
        crate::rrule::Frequency::Weekly => 60,
        crate::rrule::Frequency::Monthly => 180,
        crate::rrule::Frequency::Yearly => 540,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    /// An occurrence's draft carries the template's fields, its stream or
    /// none (ADR-0046 §1), and leaves what is per occurrence unset.
    #[test]
    fn an_occurrence_draft_carries_the_template() {
        let ctx = EntityRef::new(EntityKind::Context, [2; 16]);
        let stream = EntityRef::new(EntityKind::Stream, [3; 16]);
        let mut t = TaskTemplate {
            title: "Water the plants".into(),
            stream_id: Some(stream),
            contexts: vec![ctx],
            energy: None,
            priority: Some(2),
            estimated_duration_s: Some(600),
            body: None,
            unknown: Unknowns::new(),
        };
        let d = t.to_draft(None);
        assert_eq!(d.title, "Water the plants");
        assert_eq!(d.stream_id, Some(stream));
        assert_eq!(d.contexts, vec![ctx]);
        assert_eq!(d.priority, Some(2));
        assert_eq!(d.estimated_duration_s, Some(600));
        assert!(d.planned_at.is_none() && d.hard_due_at.is_none());
        t.stream_id = None;
        assert_eq!(t.to_draft(None).stream_id, None);
    }
}
