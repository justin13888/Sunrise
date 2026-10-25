//! An editor's form, turned into the patch that carries only what changed.
//!
//! An edit form shows a handful of an entity's fields, seeded from the stored
//! entity, and hands back what the user left on screen. The patch the core
//! applies is split-optional: a field left `None` is untouched. So a form
//! that posts every field it renders rewrites values the user never changed,
//! and a time it rewrites loses its kind — an all-day deadline becomes an
//! instant at local midnight, a floating plan is pinned to this device's
//! zone, and a kind a newer build wrote is replaced by its stand-in
//! (#515; `docs/11-adr/0066-times-survive-travel.md` §4, "an editor writes
//! only the fields it changed, and a time it did not change keeps its kind").
//!
//! Each editor here has three parts: the **stored** fields it compares
//! against, the **form** the client hands back, and a `*_patch` function from
//! the pair to the domain patch. A `*_form` function seeds a form from the
//! stored fields, so a form left alone produces an empty patch by
//! construction. Times cross as [`PickedTime`]s and are compared and written
//! by [`crate::civil::field_edit`].

use jiff::tz::TimeZone;
use sunrise_domain::{
    Block, BlockPatch, Energy, RRule, Routine, RoutineCatchupPolicy, RoutinePatch, SunriseTime,
    Task, TaskPatch, TaskTemplate,
};
use sunrise_id::EntityRef;
use thiserror::Error;

use crate::civil::{field_edit, same_kind_at, seed_ms, value_at, CivilError, PickedTime, TimeKind};

/// Why a form could not become a patch.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EditorError {
    /// A title is required and the form's is blank.
    #[error("a title is required")]
    EmptyTitle,
    /// A block's end is not after its start.
    #[error("a block must end after it starts")]
    EndNotAfterStart,
    /// A time could not be read or written.
    #[error(transparent)]
    Time(#[from] CivilError),
}

/// The kind a deadline the user picks for the first time is written as.
///
/// A date-and-time picker yields a wall-clock time with no zone named, which
/// `docs/10-cross-cutting/time.md` §1 writes as `floating`: a deadline set at
/// 17:00 is 17:00 wherever its owner is. The Apple editor wrote an instant.
pub const NEW_DEADLINE_KIND: TimeKind = TimeKind::Floating;

/// The task fields the task editor shows, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFields {
    /// Title.
    pub title: String,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// The soft deadline (ADR-0047 §1).
    pub target_at: Option<SunriseTime>,
    /// The hard deadline.
    pub hard_due_at: Option<SunriseTime>,
}

impl From<&Task> for TaskFields {
    fn from(t: &Task) -> Self {
        Self {
            title: t.title.clone(),
            priority: t.priority,
            energy: t.energy.clone(),
            estimated_duration_s: t.estimated_duration_s,
            target_at: t.target_at.clone(),
            hard_due_at: t.hard_due_at.clone(),
        }
    }
}

/// What the task editor hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskForm {
    /// Title; trimmed before it is compared.
    pub title: String,
    /// Priority 1..=5, or `None` for none.
    pub priority: Option<u8>,
    /// Energy facet, or `None` for any.
    pub energy: Option<Energy>,
    /// Estimate in seconds, or `None` (a zero estimate is none).
    pub estimated_duration_s: Option<u64>,
    /// The soft-deadline picker, or `None` when it is switched off.
    pub target_at: Option<PickedTime>,
    /// The hard-deadline picker, or `None` when it is switched off.
    pub hard_due_at: Option<PickedTime>,
}

/// The task editor's form, seeded from `stored`.
#[must_use]
pub fn task_form(stored: &TaskFields, reader: &TimeZone) -> TaskForm {
    TaskForm {
        title: stored.title.clone(),
        priority: stored.priority,
        energy: stored.energy.clone(),
        estimated_duration_s: stored.estimated_duration_s,
        target_at: stored.target_at.as_ref().map(|v| PickedTime {
            ms: seed_ms(v, reader),
            kind: None,
        }),
        hard_due_at: stored.hard_due_at.as_ref().map(|v| PickedTime {
            ms: seed_ms(v, reader),
            kind: None,
        }),
    }
}

/// The patch the task editor's `form` makes to `stored`: only the fields
/// that changed, and each deadline in the kind it already had.
///
/// # Errors
/// [`EditorError::EmptyTitle`] for a blank title, and
/// [`EditorError::Time`] when the deadline cannot be written.
pub fn task_patch(
    stored: &TaskFields,
    form: &TaskForm,
    reader: &TimeZone,
) -> Result<TaskPatch, EditorError> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err(EditorError::EmptyTitle);
    }
    let estimate = form.estimated_duration_s.filter(|s| *s > 0);
    Ok(TaskPatch {
        title: (title != stored.title).then(|| title.to_owned()),
        priority: changed(stored.priority.as_ref(), form.priority),
        energy: changed(stored.energy.as_ref(), form.energy.clone()),
        estimated_duration_s: changed(stored.estimated_duration_s.as_ref(), estimate),
        target_at: field_edit(
            stored.target_at.as_ref(),
            form.target_at,
            NEW_DEADLINE_KIND,
            reader,
        )?,
        hard_due_at: field_edit(
            stored.hard_due_at.as_ref(),
            form.hard_due_at,
            NEW_DEADLINE_KIND,
            reader,
        )?,
        ..TaskPatch::default()
    })
}

/// Whether `patch` changes nothing, so a client can skip the write (and the
/// undo step it would push).
#[must_use]
pub fn task_patch_is_empty(patch: &TaskPatch) -> bool {
    let TaskPatch {
        title,
        body,
        stream_id,
        contexts,
        state,
        priority,
        energy,
        estimated_duration_s,
        planned_at,
        target_at,
        hard_due_at,
        scheduling_constraints,
        blocked_by,
        assignee,
        reminder_lead_s,
        archived,
        sort_order,
        late_acknowledged_at,
    } = patch;
    title.is_none()
        && body.is_none()
        && stream_id.is_none()
        && contexts.is_none()
        && state.is_none()
        && priority.is_none()
        && energy.is_none()
        && estimated_duration_s.is_none()
        && planned_at.is_none()
        && target_at.is_none()
        && hard_due_at.is_none()
        && scheduling_constraints.is_none()
        && blocked_by.is_none()
        && assignee.is_none()
        && reminder_lead_s.is_none()
        && archived.is_none()
        && sort_order.is_none()
        && late_acknowledged_at.is_none()
}

/// The kind a block editor's picker shows for a stored bound.
///
/// The picker offers zoned, floating and instant. All-day has no entry — a
/// block is a range within a day — and neither has a kind this build does
/// not know, so both read as zoned, which is what saving a change to them
/// makes them.
#[must_use]
pub const fn block_picker_kind(value: &SunriseTime) -> TimeKind {
    match TimeKind::of(value) {
        Some(TimeKind::Instant) => TimeKind::Instant,
        Some(TimeKind::Floating) => TimeKind::Floating,
        Some(TimeKind::Zoned | TimeKind::AllDay) | None => TimeKind::Zoned,
    }
}

/// Whether a moved bound can keep its stored kind: everything but an all-day
/// date (a grid gives it a time of day) and a kind this build cannot write.
const fn keeps_kind_on_grid(value: &SunriseTime) -> bool {
    matches!(
        TimeKind::of(value),
        Some(TimeKind::Instant | TimeKind::Zoned | TimeKind::Floating)
    )
}

/// What the block editor hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockForm {
    /// Title; trimmed, and blank clears it.
    pub title: String,
    /// Follow the single bound task's title.
    pub title_track_task: bool,
    /// Start picker, epoch milliseconds.
    pub starts_at_ms: i64,
    /// End picker, epoch milliseconds.
    pub ends_at_ms: i64,
    /// The kind picker.
    pub kind: TimeKind,
}

/// The block editor's form, seeded from `block`.
///
/// The **stored** title, not the resolved one a grid row renders: that may be
/// a bound task's live title, and writing it back would turn a tracking block
/// into one with a hard-coded copy of a name it was following.
#[must_use]
pub fn block_form(block: &Block, reader: &TimeZone) -> BlockForm {
    BlockForm {
        title: block.title.clone().unwrap_or_default(),
        title_track_task: block.title_track_task,
        starts_at_ms: seed_ms(&block.starts_at, reader),
        ends_at_ms: seed_ms(&block.ends_at, reader),
        kind: block_picker_kind(&block.starts_at),
    }
}

/// The patch the block editor's `form` makes to `block`.
///
/// A bound is written when its moment moved or the kind picker changed.
/// With the kind unchanged, a moved bound keeps its stored kind and zone
/// ([`crate::civil::same_kind_at`]); the Apple editor re-expressed it in the
/// picker's kind in this device's zone, which re-pinned a Berlin 09:00 to
/// wherever the editor was opened. A changed kind writes both bounds fresh in
/// the reader's zone, so the two stay the same kind
/// (`docs/10-cross-cutting/time.md` §1).
///
/// # Errors
/// [`EditorError::EndNotAfterStart`] when the form's end is not after its
/// start, and [`EditorError::Time`] when a bound cannot be written.
pub fn block_patch(
    block: &Block,
    form: &BlockForm,
    reader: &TimeZone,
) -> Result<BlockPatch, EditorError> {
    if form.ends_at_ms <= form.starts_at_ms {
        return Err(EditorError::EndNotAfterStart);
    }
    let title = form.title.trim();
    let title = (Some(title) != block.title.as_deref().or(Some("")))
        .then(|| (!title.is_empty()).then(|| title.to_owned()));
    let bounds = [
        (&block.starts_at, form.starts_at_ms),
        (&block.ends_at, form.ends_at_ms),
    ];
    let moved = bounds.map(|(stored, ms)| ms != seed_ms(stored, reader));
    // Both bounds are rewritten in the picker's kind when the kind changed,
    // or when a bound that moved has no picker entry of its own (all-day, or
    // unknown): either way the stored kind cannot be kept, and rewriting one
    // bound alone would leave the two of different kinds.
    let rewrite = form.kind != block_picker_kind(&block.starts_at)
        || bounds
            .iter()
            .zip(moved)
            .any(|((stored, _), m)| m && !keeps_kind_on_grid(stored));
    let mut next = [None, None];
    for (slot, ((stored, ms), m)) in next.iter_mut().zip(bounds.into_iter().zip(moved)) {
        let value = if rewrite {
            Some(value_at(ms, form.kind, reader)?)
        } else if m {
            same_kind_at(stored, ms, reader)?
        } else {
            None
        };
        *slot = value.filter(|v| v != stored);
    }
    let [starts_at, ends_at] = next;
    Ok(BlockPatch {
        starts_at,
        ends_at,
        title,
        title_track_task: (form.title_track_task != block.title_track_task)
            .then_some(form.title_track_task),
        stream_id: None,
    })
}

/// The routine fields the routine editor shows, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineFields {
    /// The task each occurrence generates.
    pub template: TaskTemplate,
    /// The recurrence.
    pub rrule: RRule,
    /// IANA zone the rule expands in.
    pub timezone: String,
    /// What happens to occurrences missed offline.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Series end (`floating` or `all_day`).
    pub ends_at: Option<SunriseTime>,
}

impl From<&Routine> for RoutineFields {
    fn from(r: &Routine) -> Self {
        Self {
            template: r.template.clone(),
            rrule: r.rrule.clone(),
            timezone: r.timezone.clone(),
            catchup_policy: r.catchup_policy.clone(),
            ends_at: r.ends_at.clone(),
        }
    }
}

/// What the routine editor hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineForm {
    /// Title of each generated task; trimmed.
    pub title: String,
    /// Stream each generated task lands in, or `None` for no stream.
    pub stream_id: Option<EntityRef>,
    /// Priority 1..=5, or `None`.
    pub priority: Option<u8>,
    /// Energy facet, or `None`.
    pub energy: Option<Energy>,
    /// Estimate in seconds, or `None` (a zero estimate is none).
    pub estimated_duration_s: Option<u64>,
    /// The recurrence the cadence field reads.
    pub rrule: RRule,
    /// IANA zone.
    pub timezone: String,
    /// Catch-up policy.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Series end, or `None` when the routine does not stop.
    pub ends_at: Option<SunriseTime>,
}

/// The routine editor's form, seeded from `stored`.
#[must_use]
pub fn routine_form(stored: &RoutineFields) -> RoutineForm {
    RoutineForm {
        title: stored.template.title.clone(),
        stream_id: stored.template.stream_id,
        priority: stored.template.priority,
        energy: stored.template.energy.clone(),
        estimated_duration_s: stored.template.estimated_duration_s,
        rrule: stored.rrule.clone(),
        timezone: stored.timezone.clone(),
        catchup_policy: stored.catchup_policy.clone(),
        ends_at: stored.ends_at.clone(),
    }
}

/// The patch the routine editor's `form` makes to `stored`.
///
/// The template is one field in the patch and replaces the whole template,
/// so it is sent only when one of the five fields the form shows changed,
/// and it is the stored template with those five replaced: contexts, body
/// and the fields a newer build wrote go back as they arrived (ADR-0045 §6).
/// The Apple editor sent the template, the rule, the zone, the policy and the
/// end on every save. `ends_at` is a `SunriseTime` and is compared as the
/// value it is, kind included, so an end the form did not touch keeps its
/// kind.
///
/// # Errors
/// [`EditorError::EmptyTitle`] for a blank title.
pub fn routine_patch(
    stored: &RoutineFields,
    form: &RoutineForm,
) -> Result<RoutinePatch, EditorError> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err(EditorError::EmptyTitle);
    }
    let template = TaskTemplate {
        title: title.to_owned(),
        stream_id: form.stream_id,
        priority: form.priority,
        energy: form.energy.clone(),
        estimated_duration_s: form.estimated_duration_s.filter(|s| *s > 0),
        ..stored.template.clone()
    };
    Ok(RoutinePatch {
        template: (template != stored.template).then_some(template),
        rrule: (form.rrule != stored.rrule).then(|| form.rrule.clone()),
        timezone: (form.timezone != stored.timezone).then(|| form.timezone.clone()),
        catchup_policy: (form.catchup_policy != stored.catchup_policy)
            .then(|| form.catchup_policy.clone()),
        ends_at: changed(stored.ends_at.as_ref(), form.ends_at.clone()),
        ..RoutinePatch::default()
    })
}

/// `None` when `next` is `stored`, else the domain's set-or-clear.
#[allow(clippy::option_option)]
fn changed<T: PartialEq>(stored: Option<&T>, next: Option<T>) -> Option<Option<T>> {
    (stored != next.as_ref()).then_some(next)
}

#[cfg(test)]
mod tests;
