//! The seam's editors and defer targets: the records cross both ways without
//! losing a kind, and an untouched form is an empty edit.

use super::*;
use jiff::civil::date;
use sunrise_domain::{Task, TaskState, Unknowns};
use sunrise_id::EntityKind;

const TZ: &str = "America/Toronto";

fn task(hard_due_at: Option<SunriseTime>) -> TaskItem {
    TaskItem::from(&Task {
        id: EntityRef::new(EntityKind::Task, [1; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        title: "Renew passport".into(),
        body: None,
        stream_id: None,
        contexts: std::collections::BTreeSet::new(),
        state: TaskState::Todo,
        priority: Some(2),
        energy: None,
        estimated_duration_s: None,
        planned_at: None,
        target_at: None,
        hard_due_at,
        scheduling_constraints: Vec::new(),
        completed_at: None,
        deferred_count: 0,
        blocks: std::collections::BTreeSet::new(),
        blocked_by: std::collections::BTreeSet::new(),
        assignee: None,
        routine_id: None,
        routine_occurrence: None,
        reminder_lead_s: None,
        sort_order: None,
        triaged_at: None,
        cancel_reason: None,
        late_acknowledged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    })
}

fn is_empty(edit: &TaskEdit) -> bool {
    edit.title.is_none()
        && edit.set_priority.is_none()
        && !edit.clear_priority
        && edit.set_energy.is_none()
        && !edit.clear_energy
        && edit.set_estimated_duration_s.is_none()
        && !edit.clear_estimated_duration
        && edit.set_due_at.is_none()
        && !edit.clear_due_at
}

#[test]
fn an_untouched_task_form_is_an_empty_edit_and_a_title_edit_keeps_an_all_day_deadline() {
    let item = task(Some(SunriseTime::all_day(date(2026, 3, 13))));
    let form = task_editor_form(item.clone(), TZ.into()).unwrap();
    assert!(is_empty(
        &task_editor_edit(item.clone(), form.clone(), TZ.into()).unwrap()
    ));

    let mut renamed = form;
    renamed.title = "Renew passport now".into();
    let edit = task_editor_edit(item, renamed, TZ.into()).unwrap();
    assert_eq!(edit.title.as_deref(), Some("Renew passport now"));
    assert!(edit.set_due_at.is_none() && !edit.clear_due_at);
}

#[test]
fn a_moved_deadline_crosses_back_as_a_date() {
    let item = task(Some(SunriseTime::all_day(date(2026, 3, 13))));
    let mut form = task_editor_form(item.clone(), TZ.into()).unwrap();
    form.due_at = form.due_at.map(|p| PickedTime {
        ms: p.ms + 86_400_000,
        kind: None,
    });
    let edit = task_editor_edit(item, form, TZ.into()).unwrap();
    assert!(matches!(
        edit.set_due_at,
        Some(TimeValue::AllDay { date: d }) if d == date(2026, 3, 14)
    ));
}

#[test]
fn an_unknown_zone_is_refused_not_read_as_utc() {
    let item = task(None);
    assert!(matches!(
        task_editor_form(item, "Not/AZone".into()),
        Err(BindingError::BadTime { .. })
    ));
    assert!(matches!(
        today_capture_scheduled_at(0, "Not/AZone".into()),
        Err(BindingError::BadTime { .. })
    ));
}

#[test]
fn a_defer_keeps_the_plans_kind_across_the_seam() {
    // Saturday 2026-03-07 12:00 in Toronto.
    let now_ms = 1_772_902_800_000;
    let plan = TimeValue::Floating {
        civil: date(2026, 3, 7).at(9, 0, 0, 0),
    };
    let to = defer_time_value(Some(plan), 1, now_ms, TZ.into()).unwrap();
    assert!(matches!(
        to,
        TimeValue::Floating { civil } if civil == date(2026, 3, 8).at(9, 0, 0, 0)
    ));
    let hour = snooze_time_value(None, SnoozeSpan::OneHour, now_ms, TZ.into()).unwrap();
    assert!(matches!(hour, TimeValue::Instant { at } if at.as_millisecond() == 1_772_906_400_000));
    let today = today_capture_scheduled_at(now_ms, TZ.into()).unwrap();
    assert!(matches!(today, TimeValue::AllDay { date: d } if d == date(2026, 3, 7)));
    let tomorrow = scheduled_day_value(1, now_ms, TZ.into()).unwrap();
    assert!(matches!(tomorrow, TimeValue::AllDay { date: d } if d == date(2026, 3, 8)));
    let picked = scheduled_time_value(1_772_902_800_000, None, TZ.into()).unwrap();
    assert!(matches!(picked, TimeValue::Floating { .. }));
}

fn routine() -> RoutineItem {
    RoutineItem::from(&sunrise_domain::Routine {
        id: EntityRef::new(EntityKind::Routine, [2; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        template: TaskTemplate {
            title: "Water the plants".into(),
            stream_id: None,
            contexts: vec![EntityRef::new(EntityKind::Context, [3; 16])],
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        },
        rrule: RRule::parse("FREQ=DAILY").unwrap(),
        timezone: TZ.into(),
        starts_at: Timestamp::UNIX_EPOCH.into(),
        ends_at: None,
        scheduling_constraints: Vec::new(),
        skip_dates: Vec::new(),
        skipped_keys: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
        streak_counter: 0,
        last_completed_at: None,
        grace_window_s: None,
        forgiveness_enabled: true,
        streak_started_at: None,
        forgivenesses_in_window: 0,
        streak_keys: Vec::new(),
        paused: false,
        paused_until: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    })
}

#[test]
fn a_routine_form_sends_only_what_changed() {
    let item = routine();
    let form = routine_editor_form(item.clone());
    let edit = routine_editor_edit(item.clone(), form.clone()).unwrap();
    assert!(edit.template.is_none() && edit.rrule.is_none() && edit.timezone.is_none());
    assert!(edit.catchup_policy.is_none() && edit.set_ends_at.is_none() && !edit.clear_ends_at);

    let mut renamed = form;
    renamed.title = "Water the ferns".into();
    let edit = routine_editor_edit(item, renamed).unwrap();
    let template = edit.template.expect("the title changed");
    assert_eq!(template.title, "Water the ferns");
    assert_eq!(
        template.contexts.len(),
        1,
        "the contexts the form hides survive"
    );
}
