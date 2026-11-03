//! The task patch inverse, field by field: every field a `TaskPatch` can
//! write is restored by its inverse, alone and all together.

use super::*;
use sunrise_domain::{Energy, SunriseTime};

/// The names of the fields `p` writes. Destructured whole, so a field
/// added to `TaskPatch` does not compile here until it is listed.
pub(super) fn touched(p: &TaskPatch) -> Vec<&'static str> {
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
    } = p;
    [
        ("title", title.is_some()),
        ("body", body.is_some()),
        ("stream_id", stream_id.is_some()),
        ("contexts", contexts.is_some()),
        ("state", state.is_some()),
        ("priority", priority.is_some()),
        ("energy", energy.is_some()),
        ("estimated_duration_s", estimated_duration_s.is_some()),
        ("planned_at", planned_at.is_some()),
        ("target_at", target_at.is_some()),
        ("hard_due_at", hard_due_at.is_some()),
        ("scheduling_constraints", scheduling_constraints.is_some()),
        ("blocked_by", blocked_by.is_some()),
        ("assignee", assignee.is_some()),
        ("reminder_lead_s", reminder_lead_s.is_some()),
        ("archived", archived.is_some()),
        ("sort_order", sort_order.is_some()),
        ("late_acknowledged_at", late_acknowledged_at.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect()
}

/// `t` with `p` written onto it: each field `p` names replaced by the
/// value it carries, every other left as it was. The oracle the round
/// trips below are read against, written field by field on its own so a
/// swap in `inverse::inverse_task_patch` is not copied into it.
pub(super) fn apply(t: &Task, p: &TaskPatch) -> Task {
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
    } = p.clone();
    let mut t = t.clone();
    if let Some(v) = title {
        t.title = v;
    }
    if let Some(v) = body {
        t.body = v;
    }
    if let Some(v) = stream_id {
        t.stream_id = v;
    }
    if let Some(v) = contexts {
        t.contexts = v.into_iter().collect();
    }
    if let Some(v) = state {
        t.state = v;
    }
    if let Some(v) = priority {
        t.priority = v;
    }
    if let Some(v) = energy {
        t.energy = v;
    }
    if let Some(v) = estimated_duration_s {
        t.estimated_duration_s = v;
    }
    if let Some(v) = planned_at {
        t.planned_at = v;
    }
    if let Some(v) = target_at {
        t.target_at = v;
    }
    if let Some(v) = hard_due_at {
        t.hard_due_at = v;
    }
    if let Some(v) = scheduling_constraints {
        t.scheduling_constraints = v;
    }
    if let Some(v) = blocked_by {
        t.blocked_by = v.into_iter().collect();
    }
    if let Some(v) = assignee {
        t.assignee = v;
    }
    if let Some(v) = reminder_lead_s {
        t.reminder_lead_s = v;
    }
    if let Some(v) = archived {
        t.archived = v;
    }
    if let Some(v) = sort_order {
        t.sort_order = v;
    }
    if let Some(v) = late_acknowledged_at {
        t.late_acknowledged_at = v;
    }
    t
}

pub(super) fn day(d: i8) -> SunriseTime {
    SunriseTime::all_day(jiff::civil::date(2026, 6, d))
}

fn constraint(severity: sunrise_domain::ConstraintSeverity) -> sunrise_domain::ScheduleConstraint {
    sunrise_domain::ScheduleConstraint {
        time_of_day: None,
        days_of_week: sunrise_domain::WeekdaySet::default(),
        date_range: None,
        severity,
        unknown: Unknowns::new(),
    }
}

fn person(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Person, [n; 16])
}

/// A task holding a value in every field a patch can write, each one
/// distinct from its neighbours of the same type, so restoring the wrong
/// field's value (`target_at` for `hard_due_at`, say) shows.
fn every_field_held() -> Task {
    let mut t = task(1);
    t.title = "before".into();
    t.body = Some(sunrise_domain::NoteBody(b"before".to_vec()));
    t.stream_id = Some(sid(1));
    t.contexts = [cid(1)].into();
    t.state = TaskState::InProgress;
    t.priority = Some(2);
    t.energy = Some(Energy::Low);
    t.estimated_duration_s = Some(600);
    t.planned_at = Some(day(1));
    t.target_at = Some(day(2));
    t.hard_due_at = Some(day(3));
    t.scheduling_constraints = vec![constraint(sunrise_domain::ConstraintSeverity::Hard)];
    t.blocked_by = [tid(5)].into();
    t.assignee = Some(person(1));
    t.reminder_lead_s = Some(300);
    t.archived = false;
    t.sort_order = Some("G".into());
    t.late_acknowledged_at = Some(jiff::Timestamp::UNIX_EPOCH);
    t
}

/// A patch that writes every field, each to a value
/// [`every_field_held`] does not hold. Built without `..Default` so a
/// new field does not compile here until it has a value.
fn every_field_patch() -> TaskPatch {
    TaskPatch {
        title: Some("after".into()),
        body: Some(Some(sunrise_domain::NoteBody(b"after".to_vec()))),
        stream_id: Some(Some(sid(2))),
        contexts: Some(vec![cid(2)]),
        state: Some(TaskState::Done),
        priority: Some(Some(4)),
        energy: Some(Some(Energy::High)),
        estimated_duration_s: Some(Some(1200)),
        planned_at: Some(Some(day(11))),
        target_at: Some(Some(day(12))),
        hard_due_at: Some(Some(day(13))),
        scheduling_constraints: Some(vec![constraint(sunrise_domain::ConstraintSeverity::Soft)]),
        blocked_by: Some(vec![tid(6)]),
        assignee: Some(Some(person(2))),
        reminder_lead_s: Some(Some(900)),
        archived: Some(true),
        sort_order: Some(Some("T".into())),
        late_acknowledged_at: Some(Some(
            jiff::Timestamp::UNIX_EPOCH + jiff::SignedDuration::from_secs(60),
        )),
    }
}

/// A patch that empties every field a patch can empty: `Some(None)` for
/// the clearable optionals, an empty list for the lists. The three
/// fields with nothing to clear to (`title`, `state`, `archived`) are
/// left out.
fn every_field_clear() -> TaskPatch {
    TaskPatch {
        title: None,
        body: Some(None),
        stream_id: Some(None),
        contexts: Some(Vec::new()),
        state: None,
        priority: Some(None),
        energy: Some(None),
        estimated_duration_s: Some(None),
        planned_at: Some(None),
        target_at: Some(None),
        hard_due_at: Some(None),
        scheduling_constraints: Some(Vec::new()),
        blocked_by: Some(Vec::new()),
        assignee: Some(None),
        reminder_lead_s: Some(None),
        archived: None,
        sort_order: Some(None),
        late_acknowledged_at: Some(None),
    }
}

/// `p` split into one patch per field it writes, each named.
///
/// A field missing from the list below is caught by
/// `the_field_table_covers_every_field_a_patch_can_write`, which reads
/// the names against [`touched`].
fn split(p: &TaskPatch) -> Vec<(&'static str, TaskPatch)> {
    let mut out = Vec::new();
    macro_rules! each {
            ($($field:ident),* $(,)?) => {$(
                if p.$field.is_some() {
                    out.push((
                        stringify!($field),
                        TaskPatch { $field: p.$field.clone(), ..TaskPatch::default() },
                    ));
                }
            )*};
        }
    each!(
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
    );
    out
}

/// One patch per field: each field of [`every_field_patch`] on its own,
/// and, for every field a patch can clear, the clear of it too.
fn one_field_patches() -> Vec<(&'static str, TaskPatch)> {
    let mut rows = split(&every_field_patch());
    rows.extend(split(&every_field_clear()));
    rows
}

/// The patch `invert` builds for an edit of the task `rows` holds.
fn inverse_of_edit(rows: &Rows, patch: TaskPatch) -> TaskPatch {
    let id = rows.tasks[0].id;
    match invert(rows, &[Command::UpdateTask { id, patch }])
        .expect("invertible")
        .as_slice()
    {
        [Command::UpdateTask { id: back, patch }] if *back == id => patch.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_field_table_covers_every_field_a_patch_can_write() {
    let named: BTreeSet<_> = one_field_patches().into_iter().map(|(n, _)| n).collect();
    let every: BTreeSet<_> = touched(&every_field_patch()).into_iter().collect();
    assert_eq!(named, every);
    assert_eq!(every.len(), 18, "every TaskPatch field");
}

/// Each field, written on its own, is restored by its inverse, and the
/// inverse writes that field and no other.
#[test]
fn every_patch_field_is_restored_by_its_inverse() {
    let before = every_field_held();
    let rows = rows_with(before.clone());
    for (name, forward) in one_field_patches() {
        assert_eq!(touched(&forward), [name], "the row writes {name} alone");
        let after = apply(&before, &forward);
        assert_ne!(after, before, "{name}: the row changes the task");
        let back = inverse_of_edit(&rows, forward);
        assert_eq!(
            touched(&back),
            [name],
            "{name}: the inverse writes only {name}"
        );
        assert_eq!(apply(&after, &back), before, "{name} is restored");
    }
}

/// A patch writing every field at once is undone by one inverse that
/// restores them all.
#[test]
fn a_patch_of_every_field_is_restored_whole() {
    let before = every_field_held();
    let forward = every_field_patch();
    let after = apply(&before, &forward);
    let back = inverse_of_edit(&rows_with(before.clone()), forward.clone());
    assert_eq!(touched(&back), touched(&forward));
    assert_eq!(apply(&after, &back), before);
}

/// A field the task held empty comes back empty: the inverse of setting
/// it is its clear, not a skip.
#[test]
fn a_field_set_from_empty_is_cleared_by_its_inverse() {
    let before = task(1);
    let rows = rows_with(before.clone());
    for (name, forward) in one_field_patches() {
        let after = apply(&before, &forward);
        let back = inverse_of_edit(&rows, forward);
        assert_eq!(apply(&after, &back), before, "{name} is restored");
    }
}
