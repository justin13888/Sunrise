//! Context shifts: a trip, a sabbatical, a stretch away from one job
//! (issue #352, `docs/11-adr/0073-context-shift.md`,
//! `docs/00-product/core-workflows.md` §5).
//!
//! Everything here is a pure function of its inputs: no clock, no storage,
//! no ambient zone. The caller passes `now` and the reader's zone.
//!
//! - [`Shift`] is what the user started: a name, a start, an optional end,
//!   an optional destination zone and place, and the streams and contexts it
//!   keeps primary.
//! - [`active_shift`] picks the one shift in force at an instant: the one
//!   with the latest start, ties broken by the caller's key, so every
//!   replica holding the same shifts picks the same one.
//! - [`ShiftPolicy`] is what a routine does while a shift covers one of its
//!   occurrences, and [`Routine::occurrences_under_shifts`] applies it at
//!   generation time. Nothing already stored is rewritten: the shift changes
//!   what generation produces, and ending it changes it back.
//! - [`Routine::derived_streak_across`] and [`Routine::streak_as_of_across`]
//!   excuse what a `skip` or `pause` shift held back, so a trip does not
//!   break a streak.
//! - [`away_summary`] is the "While you were away" read: the occurrences
//!   missed, the deadlines that passed and the tasks captured during a shift.
//!
//! The name is `Shift`, not "context": `Context` is the tag entity
//! ([`crate::context::Context`]).

use crate::routine::Routine;
use crate::routine_gen::{occurrence_task_id, ExpandError, Occurrence};
use crate::streak::DerivedStreak;
use crate::task::{Task, TaskState};
use crate::time::SunriseTime;
use crate::unknown::UnknownVariant;
use crate::validation::{validate_title, ValidationError};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use std::collections::BTreeSet;
use sunrise_id::{EntityKind, EntityRef};

/// The cause a `skip` shift records on each occurrence it holds back
/// (`docs/00-product/core-workflows.md` §5).
pub const CONTEXT_SHIFT_CAUSE: &str = "context_shift";

/// Longest shift name, in characters: a stream name's bound, since a shift
/// is named like one ("Travel: Tokyo").
pub const MAX_SHIFT_NAME_LEN: usize = 128;

/// The widest gap between two real zones' offsets, rounded up: a key
/// re-anchored to another zone moves by less than this.
const REANCHOR_SLACK: SignedDuration = SignedDuration::from_hours(27);

/// What a routine does while a shift covers one of its occurrences.
///
/// An unrecognised policy reads as [`ShiftPolicy::Pause`] and is written back
/// verbatim (ADR-0045 §6): `pause` generates nothing, so a build that cannot
/// read a newer policy never authors an occurrence that policy may not want.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ShiftPolicy {
    /// The horizon stops: no occurrence the shift covers is generated, and
    /// none is missed. Generation resumes at the shift's end with no
    /// backfill. The default.
    #[default]
    Pause,
    /// Occurrences the shift covers keep their wall-clock time and move to
    /// the shift's destination zone: "09:00 Los Angeles" fires at 09:00
    /// Tokyo while the user is in Tokyo.
    ShiftTz,
    /// Occurrences the shift covers are generated as skipped, with cause
    /// [`CONTEXT_SHIFT_CAUSE`]; the streak excuses them.
    Skip,
    /// The routine ignores the shift.
    Unchanged,
    /// A policy this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(ShiftPolicy, fallback = Pause, {
    Pause => "pause",
    ShiftTz => "shift_tz",
    Skip => "skip",
    Unchanged => "unchanged",
});

impl ShiftPolicy {
    /// Whether the streak excuses an occurrence this policy holds back while
    /// a shift covers it: `pause` and `skip` hold it back, so missing it is
    /// not the user's miss.
    #[must_use]
    pub fn excuses_streak(&self) -> bool {
        matches!(self.effective(), Self::Pause | Self::Skip)
    }
}

/// A context shift the user started (ADR-0073 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shift {
    /// What the user calls it ("Travel: Tokyo").
    pub name: String,
    /// When it starts: a recorded moment, so an instant.
    pub starts_at: Timestamp,
    /// When it ends, exclusive; `None` while it runs until ended.
    pub ends_at: Option<Timestamp>,
    /// The destination's IANA zone, which a `shift_tz` routine re-anchors
    /// to; `None` when the shift does not move the user.
    pub zone: Option<String>,
    /// The destination place (`plc_`), if the user named one.
    pub place: Option<EntityRef>,
    /// The streams the shift keeps primary.
    pub primary_streams: BTreeSet<EntityRef>,
    /// The contexts the shift keeps primary: a task carrying one stays
    /// primary whatever its stream.
    pub primary_contexts: BTreeSet<EntityRef>,
}

impl Shift {
    /// Validate the shift's shape: a name, an end after the start, a zone
    /// this build knows, a place that is a place, and focus lists of streams
    /// and contexts.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidTitle`] for an empty name, and
    /// [`ValidationError::Field`] naming the field that fails otherwise.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(&self.name, "shift.name", MAX_SHIFT_NAME_LEN)?;
        if self.ends_at.is_some_and(|end| end <= self.starts_at) {
            return Err(field("shift.ends_at", "after_starts_at"));
        }
        if let Some(zone) = &self.zone {
            if TimeZone::get(zone).is_err() {
                return Err(field("shift.zone", "iana_timezone"));
            }
        }
        if self.place.is_some_and(|p| p.kind() != EntityKind::Place) {
            return Err(field("shift.place", "place_ref"));
        }
        if self
            .primary_streams
            .iter()
            .any(|s| s.kind() != EntityKind::Stream)
        {
            return Err(field("shift.primary_streams", "stream_ref"));
        }
        if self
            .primary_contexts
            .iter()
            .any(|c| c.kind() != EntityKind::Context)
        {
            return Err(field("shift.primary_contexts", "context_ref"));
        }
        Ok(())
    }

    /// Whether the shift is in force at `at`: at or after its start, and
    /// before its end when it has one.
    #[must_use]
    pub fn covers(&self, at: Timestamp) -> bool {
        self.starts_at <= at && self.ends_at.is_none_or(|end| at < end)
    }

    /// Whether the shift focuses anything. A shift that names no stream and
    /// no context demotes nothing.
    #[must_use]
    pub fn has_focus(&self) -> bool {
        !self.primary_streams.is_empty() || !self.primary_contexts.is_empty()
    }

    /// Whether views keep stream `stream` primary while this shift is
    /// active. Every stream is primary under a shift with no focus; under one
    /// with focus, only the streams it names are. `None` is no stream.
    #[must_use]
    pub fn keeps_stream_primary(&self, stream: Option<&EntityRef>) -> bool {
        !self.has_focus() || stream.is_some_and(|s| self.primary_streams.contains(s))
    }

    /// Whether views keep `task` primary while this shift is active: its
    /// stream is primary ([`Shift::keeps_stream_primary`]), or it carries a
    /// context the shift keeps primary. The rest are demoted behind the
    /// collapsed "Paused for …" group, never deleted or rescheduled.
    #[must_use]
    pub fn keeps_task_primary(&self, task: &Task) -> bool {
        self.keeps_stream_primary(task.stream_id.as_ref())
            || task
                .contexts
                .iter()
                .any(|c| self.primary_contexts.contains(c))
    }

    /// The window this shift held by `until`: from its start to its end, or
    /// to `until` when it has not ended by then. `None` when it had not
    /// started by `until`.
    #[must_use]
    pub fn window_until(&self, until: Timestamp) -> Option<(Timestamp, Timestamp)> {
        let end = self.ends_at.map_or(until, |e| e.min(until));
        (self.starts_at < end).then_some((self.starts_at, end))
    }
}

fn field(field: &'static str, constraint: &'static str) -> ValidationError {
    ValidationError::Field { field, constraint }
}

/// The shift in force at `at` among `shifts`: of those that cover `at`, the
/// one with the latest start, a tie going to the greatest key.
///
/// `K` is the caller's identity for a shift, its entity id once shifts are
/// stored: two devices that start shifts concurrently hold the same set once
/// they have synced, and so pick the same one, whatever order they learned
/// of them in.
#[must_use]
pub fn active_shift<K: Ord>(shifts: &[(K, Shift)], at: Timestamp) -> Option<&(K, Shift)> {
    shifts
        .iter()
        .filter(|(_, s)| s.covers(at))
        .max_by(|(ka, a), (kb, b)| a.starts_at.cmp(&b.starts_at).then_with(|| ka.cmp(kb)))
}

/// One occurrence a routine generates under its shift policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShiftedOccurrence {
    /// The occurrence. Its key is the rule's, whatever the shift did; its
    /// instant is where it fires, re-anchored when a `shift_tz` shift moved
    /// it.
    pub occurrence: Occurrence,
    /// Its planned time: [`Routine::occurrence_time`] when no shift moved
    /// it, `zoned` in the shift's zone at the key's wall clock when one did.
    pub planned_at: SunriseTime,
    /// Whether a `shift_tz` shift re-anchored it.
    pub reanchored: bool,
}

/// What a routine's generation produces in a window under its shift policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShiftGeneration {
    /// The occurrences to generate, by instant.
    pub generate: Vec<ShiftedOccurrence>,
    /// The occurrences a `skip` shift holds back, by instant: each is
    /// skipped with cause [`CONTEXT_SHIFT_CAUSE`].
    pub skipped: Vec<Occurrence>,
}

impl Routine {
    /// This routine's occurrences in `window` for a reader in `reader`,
    /// under `policy` while one of `shifts` is in force
    /// ([`active_shift`]).
    ///
    /// What [`Routine::occurrences_in`] yields is the starting point, so the
    /// routine's own end, pause and skips apply first. Then each occurrence
    /// the active shift covers, at the instant it would fire, is:
    ///
    /// - `pause` (and an unknown policy): dropped;
    /// - `skip`: moved to [`ShiftGeneration::skipped`];
    /// - `unchanged`: generated as is;
    /// - `shift_tz`: generated at its key's wall clock in the zone of the
    ///   shift that, re-anchored, covers it. A floating routine already
    ///   follows the reader (ADR-0066 §1) and is generated as is, and so is
    ///   one whose shift names no zone.
    ///
    /// # Errors
    ///
    /// [`ExpandError::WindowTooLarge`] from the expansion.
    pub fn occurrences_under_shifts<K: Ord>(
        &self,
        window: (Timestamp, Timestamp),
        reader: &TimeZone,
        shifts: &[(K, Shift)],
        policy: &ShiftPolicy,
    ) -> Result<ShiftGeneration, ExpandError> {
        let policy = policy.effective();
        let reanchors = policy == ShiftPolicy::ShiftTz && !self.is_floating();
        // A re-anchored key can move into the window from up to a zone gap
        // outside it, so widen the expansion and filter on the final instant.
        let expanded = if reanchors {
            (
                window
                    .0
                    .checked_sub(REANCHOR_SLACK)
                    .unwrap_or(Timestamp::MIN),
                window
                    .1
                    .checked_add(REANCHOR_SLACK)
                    .unwrap_or(Timestamp::MAX),
            )
        } else {
            window
        };
        let in_window = |at: Timestamp| window.0 <= at && at < window.1;
        // Only a shift naming a zone this build knows decides where an
        // occurrence fires: one with none (a conference inside a trip) leaves
        // the enclosing shift's zone in force for the days it covers.
        let zoned: Vec<(&K, Shift)> = if reanchors {
            shifts
                .iter()
                .filter(|(_, s)| s.zone.as_deref().is_some_and(|z| TimeZone::get(z).is_ok()))
                .map(|(k, s)| (k, s.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let mut out = ShiftGeneration::default();
        for occ in self.occurrences_in(expanded, reader)? {
            if reanchors {
                if let Some((zone, at)) = reanchor(&occ, &zoned) {
                    if in_window(at) {
                        out.generate.push(ShiftedOccurrence {
                            planned_at: SunriseTime::zoned(occ.civil, zone),
                            occurrence: Occurrence { at, ..occ },
                            reanchored: true,
                        });
                    }
                    continue;
                }
            }
            if !in_window(occ.at) {
                continue;
            }
            let covered = active_shift(shifts, occ.at).is_some();
            match (&policy, covered) {
                (ShiftPolicy::Pause, true) => {}
                (ShiftPolicy::Skip, true) => out.skipped.push(occ),
                _ => out.generate.push(ShiftedOccurrence {
                    planned_at: self.occurrence_time(occ.civil),
                    occurrence: occ,
                    reanchored: false,
                }),
            }
        }
        out.generate
            .sort_by_key(|o| (o.occurrence.at, o.occurrence.key.clone()));
        Ok(out)
    }

    /// [`Routine::derived_streak`], with every occurrence `policy` holds back
    /// while one of `shifts` covers it excused: neither counted nor missed,
    /// so it spends no forgiveness ([`ShiftPolicy::excuses_streak`]).
    #[must_use]
    pub fn derived_streak_across<K: Ord>(
        &self,
        shifts: &[(K, Shift)],
        policy: &ShiftPolicy,
    ) -> DerivedStreak {
        let excuses = policy.excuses_streak();
        self.derived_streak_excusing(&|o: &Occurrence| {
            excuses && active_shift(shifts, o.at).is_some()
        })
    }

    /// [`Routine::streak_as_of`], with every occurrence `policy` holds back
    /// while one of `shifts` covers it excused, as
    /// [`Routine::derived_streak_across`] does.
    #[must_use]
    pub fn streak_as_of_across<K: Ord>(
        &self,
        now: Timestamp,
        reader: &TimeZone,
        shifts: &[(K, Shift)],
        policy: &ShiftPolicy,
    ) -> DerivedStreak {
        let excuses = policy.excuses_streak();
        self.streak_as_of_excusing(now, reader, &|o: &Occurrence| {
            excuses && active_shift(shifts, o.at).is_some()
        })
    }
}

/// Where a `shift_tz` shift moves `occ`: the zone of the shift that, with the
/// key re-anchored to its zone by the one DST rule, is the one in force at
/// the re-anchored instant, and that instant. `None` when no shift with a
/// zone this build knows covers it re-anchored. The caller passes only the
/// shifts with such a zone, so a zoneless shift neither moves an occurrence
/// nor hides the zone of a shift it sits inside.
fn reanchor<K: Ord>(occ: &Occurrence, shifts: &[(K, Shift)]) -> Option<(String, Timestamp)> {
    let mut by_precedence: Vec<&(K, Shift)> = shifts.iter().collect();
    by_precedence
        .sort_by(|(ka, a), (kb, b)| b.starts_at.cmp(&a.starts_at).then_with(|| kb.cmp(ka)));
    by_precedence.into_iter().find_map(|entry| {
        let shift = &entry.1;
        let name = shift.zone.as_ref()?;
        let zone = TimeZone::get(name).ok()?;
        let at = zone
            .to_ambiguous_zoned(occ.civil)
            .compatible()
            .ok()?
            .timestamp();
        let active = active_shift(shifts, at)?;
        std::ptr::eq(active, entry).then(|| (name.clone(), at))
    })
}

/// One routine occurrence the user missed during a shift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissedOccurrence {
    /// The routine.
    pub routine: EntityRef,
    /// The occurrence's key.
    pub key: String,
    /// When it was due.
    pub at: Timestamp,
}

/// The "While you were away" summary of a shift (ADR-0073 §5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AwaySummary {
    /// The window summarised: the shift's start to its end, or to the
    /// moment of the read when it has not ended.
    pub window: Option<(Timestamp, Timestamp)>,
    /// Routine occurrences generated during the shift and not completed, by
    /// instant and then routine.
    pub missed: Vec<MissedOccurrence>,
    /// Open tasks whose hard deadline passed during the shift, by deadline.
    pub deadlines_passed: Vec<EntityRef>,
    /// Tasks captured during the shift, by creation time. A routine's
    /// generated occurrences are not captures.
    pub captured: Vec<EntityRef>,
}

/// What accrued during `shift`, read at `now` for a reader in `reader`.
///
/// - **Missed**: each occurrence of each live routine that its policy
///   generated during the shift ([`Routine::occurrences_under_shifts`]), that
///   is not in its streak keys and whose task, when one exists in `tasks`,
///   is neither done nor cancelled. A `pause` or `skip` routine misses
///   nothing the shift covered.
/// - **Deadlines passed**: each open task whose `hard_due_at`, resolved in
///   `reader`, fell in the window.
/// - **Captured**: each live task created in the window that no routine
///   generated.
///
/// # Errors
///
/// [`ExpandError::WindowTooLarge`] from a routine's expansion.
pub fn away_summary(
    shift: &Shift,
    now: Timestamp,
    routines: &[(Routine, ShiftPolicy)],
    tasks: &[Task],
    reader: &TimeZone,
) -> Result<AwaySummary, ExpandError> {
    let Some(window) = shift.window_until(now) else {
        return Ok(AwaySummary::default());
    };
    let in_window = |at: Timestamp| window.0 <= at && at < window.1;
    let shifts = [((), shift.clone())];

    let mut missed = Vec::new();
    for (routine, policy) in routines {
        let generated = routine.occurrences_under_shifts(window, reader, &shifts, policy)?;
        for o in generated.generate {
            let key = o.occurrence.key;
            if routine.streak_key_seen(&key) {
                continue;
            }
            let id = occurrence_task_id(&routine.series_root(), &key);
            let closed = tasks.iter().any(|t| {
                t.id == id && matches!(t.state.effective(), TaskState::Done | TaskState::Cancelled)
            });
            if !closed {
                missed.push(MissedOccurrence {
                    routine: routine.id,
                    key,
                    at: o.occurrence.at,
                });
            }
        }
    }
    missed.sort_by(|a, b| (a.at, a.routine, &a.key).cmp(&(b.at, b.routine, &b.key)));

    let mut deadlines: Vec<(Timestamp, EntityRef)> = tasks
        .iter()
        .filter(|t| t.is_open())
        .filter_map(|t| Some((t.hard_due_at.as_ref()?.resolve_in(reader)?, t.id)))
        .filter(|(at, _)| in_window(*at))
        .collect();
    deadlines.sort();

    let mut captured: Vec<(Timestamp, EntityRef)> = tasks
        .iter()
        .filter(|t| !t.deleted && t.routine_id.is_none() && in_window(t.created_at))
        .map(|t| (t.created_at, t.id))
        .collect();
    captured.sort();

    Ok(AwaySummary {
        window: Some(window),
        missed,
        deadlines_passed: deadlines.into_iter().map(|(_, id)| id).collect(),
        captured: captured.into_iter().map(|(_, id)| id).collect(),
    })
}

#[cfg(test)]
mod tests;
