//! The planner and the reconcile, ported from the Apple client's
//! `ReminderPlanTests` and corrected where that client keyed a reminder on
//! its fire time.

use super::*;
use jiff::civil;
use sunrise_domain::{plan_reminders, ReminderCandidate, ReminderSettings, SunriseTime};
use sunrise_id::EntityKind;

fn task_ref() -> EntityRef {
    EntityRef::new(EntityKind::Task, [1; 16])
}
fn block_ref() -> EntityRef {
    EntityRef::new(EntityKind::Block, [2; 16])
}

const AT: i64 = 1_700_000_000_000;

fn reminder(entity: EntityRef, kind: ReminderKind, fire_at_ms: i64) -> ReminderIntent {
    ReminderIntent {
        entity,
        kind,
        fire_at: Timestamp::from_millisecond(fire_at_ms).unwrap(),
        title: "Renew passport".to_owned(),
        deferred_from: None,
    }
}

fn task_at(ms: i64) -> ReminderIntent {
    reminder(task_ref(), ReminderKind::Task, ms)
}

fn plan_utc(intents: &[ReminderIntent]) -> Vec<PlannedNotification> {
    plan(intents, &TimeZone::UTC, ClockStyle::H24, MAX_SCHEDULED)
}

fn pending_of(planned: &[PlannedNotification]) -> Vec<PendingNotification> {
    planned
        .iter()
        .map(|n| PendingNotification {
            identifier: n.identifier.clone(),
            revision: Some(n.revision.clone()),
        })
        .collect()
}

/// The identity that makes rescheduling idempotent: the same intent computed
/// twice, on either side of a sync, gives the same string.
#[test]
fn the_same_intent_always_gets_the_same_identifier() {
    assert_eq!(identifier_of(&task_at(AT)), identifier_of(&task_at(AT)));
    assert!(identifier_of(&task_at(AT)).starts_with(IDENTIFIER_PREFIX));
}

/// Entity and rule change the identity; the fire time does not
/// (`docs/11-adr/0066-times-survive-travel.md` §5). The Apple planner put the
/// fire time in, so a moved reminder was a different alert; here it is the
/// same alert at a new revision.
#[test]
fn entity_and_kind_change_the_identity_and_the_fire_time_does_not() {
    let base = identifier_of(&task_at(AT));
    assert_ne!(
        base,
        identifier_of(&reminder(block_ref(), ReminderKind::Task, AT))
    );
    assert_ne!(
        base,
        identifier_of(&reminder(task_ref(), ReminderKind::Block, AT))
    );
    assert_ne!(
        base,
        identifier_of(&reminder(task_ref(), ReminderKind::Routine, AT))
    );
    assert_eq!(base, identifier_of(&task_at(AT + 60_000)));
    assert_ne!(
        notification(&task_at(AT), &TimeZone::UTC, ClockStyle::H24).revision,
        notification(&task_at(AT + 60_000), &TimeZone::UTC, ClockStyle::H24).revision,
    );
}

/// A change feed that fires forty times during a sync burst must not produce
/// forty alerts.
#[test]
fn a_second_reconcile_with_the_same_intents_does_nothing() {
    let desired = plan_utc(&[task_at(AT), reminder(block_ref(), ReminderKind::Block, AT)]);
    let first = reconcile(&desired, &[]);
    assert_eq!(first.add.len(), 2);
    assert!(first.cancel.is_empty() && first.replace.is_empty());

    let second = reconcile(&desired, &pending_of(&first.add));
    assert!(second.is_empty(), "an unchanged schedule is not rewritten");
    assert_eq!(second.kept.len(), 2);
}

/// A task moved to a new time keeps its identifier and is replaced in place:
/// nothing is cancelled, nothing added beside it.
#[test]
fn a_reminder_that_moved_is_replaced_under_its_identifier() {
    let before = plan_utc(&[task_at(AT)]);
    let after = plan_utc(&[task_at(AT + 3_600_000)]);
    let r = reconcile(&after, &pending_of(&before));
    assert!(r.add.is_empty() && r.cancel.is_empty());
    assert_eq!(r.replace, after);
}

/// A retitled reminder fires at the same instant and still has to be
/// replaced, or the OS shows the old title.
#[test]
fn a_retitled_reminder_is_replaced() {
    let before = plan_utc(&[task_at(AT)]);
    let mut renamed = task_at(AT);
    renamed.title = "Renew the passport".to_owned();
    let r = reconcile(&plan_utc(&[renamed]), &pending_of(&before));
    assert_eq!(r.replace.len(), 1);
}

/// Requests from a build that keyed on the fire time carry no revision and
/// another identifier: they are withdrawn and their reminders re-added once,
/// under the stable identifier.
#[test]
fn a_request_from_before_stable_identifiers_is_withdrawn_once() {
    let legacy = format!("{IDENTIFIER_PREFIX}task.{}.{AT}", task_ref());
    let desired = plan_utc(&[task_at(AT)]);
    let r = reconcile(
        &desired,
        &[PendingNotification {
            identifier: legacy.clone(),
            revision: None,
        }],
    );
    assert_eq!(r.cancel, [legacy]);
    assert_eq!(r.add, desired);

    let held_without_revision = PendingNotification {
        identifier: desired[0].identifier.clone(),
        revision: None,
    };
    assert_eq!(
        reconcile(&desired, &[held_without_revision]).replace,
        desired
    );
}

/// Nothing wanted means everything of ours withdrawn — a device demoted from
/// primary, or a user turning reminders off.
#[test]
fn an_empty_schedule_cancels_what_was_pending() {
    let pending = pending_of(&plan_utc(&[task_at(AT)]));
    let r = reconcile(&[], &pending);
    assert_eq!(r.cancel, [pending[0].identifier.clone()]);
    assert!(r.add.is_empty());
}

/// Reconciliation cancels; without a namespace it would cancel others' work.
#[test]
fn nothing_outside_the_namespace_is_touched() {
    let foreign = ["com.apple.something", "sunrise.other.thing"].map(|id| PendingNotification {
        identifier: id.to_owned(),
        revision: None,
    });
    assert!(reconcile(&[], &foreign).is_empty());
}

/// The earliest survive the cap, in order, and the cut is the same on every
/// run.
#[test]
fn the_earliest_reminders_are_the_ones_that_survive_the_cap() {
    let mut many: Vec<ReminderIntent> = (0..80u8)
        .map(|n| {
            reminder(
                EntityRef::new(EntityKind::Task, [n; 16]),
                ReminderKind::Task,
                AT + i64::from(n) * 60_000,
            )
        })
        .collect();
    many.reverse();
    let planned = plan_utc(&many);
    assert_eq!(planned.len(), MAX_SCHEDULED);
    assert_eq!(planned[0].fire_at_ms, AT);
    assert!(planned
        .windows(2)
        .all(|w| w[0].fire_at_ms <= w[1].fire_at_ms));
    assert!(plan(&many, &TimeZone::UTC, ClockStyle::H24, 0).is_empty());
}

/// Ties on the instant break on the identifier, and one identity is held once.
#[test]
fn a_tie_is_ordered_and_a_duplicate_identity_is_held_once() {
    let a = reminder(task_ref(), ReminderKind::Task, AT);
    let b = reminder(block_ref(), ReminderKind::Block, AT);
    let one = plan_utc(&[a.clone(), b.clone()]);
    let two = plan_utc(&[b, a.clone()]);
    assert_eq!(one, two);
    assert_eq!(plan_utc(&[task_at(AT + 1), a]).len(), 1);
}

/// A block has no Mark Done and no snooze behind it; a task has both.
#[test]
fn a_block_carries_no_actions_and_a_task_carries_three() {
    assert!(ReminderCategory::Block.snooze_spans().is_empty());
    assert!(!ReminderCategory::Block.offers_completion());
    assert!(ReminderCategory::Task.offers_completion());
    assert_eq!(
        ReminderCategory::Task.snooze_spans(),
        [SnoozeSpan::OneHour, SnoozeSpan::Tomorrow]
    );
}

/// A routine occurrence gets the task buttons, but not the task identity.
#[test]
fn a_routine_is_scheduled_as_a_task_with_its_own_identity() {
    let routine = notification(
        &reminder(task_ref(), ReminderKind::Routine, AT),
        &TimeZone::UTC,
        ClockStyle::H24,
    );
    assert_eq!(routine.category, ReminderCategory::Task);
    assert_ne!(routine.identifier, identifier_of(&task_at(AT)));
}

/// The visible half of quiet-hours queueing, with the original time read in
/// the reader's zone and written in the caller's clock style.
#[test]
fn a_quiet_hours_deferral_is_explained_on_the_notification() {
    let mut queued = task_at(AT);
    // 2023-11-14T06:30:00Z.
    queued.deferred_from = Some(Timestamp::from_second(1_699_943_400).unwrap());
    let utc = notification(&queued, &TimeZone::UTC, ClockStyle::H24);
    assert_eq!(utc.body, "Held for quiet hours · due 06:30");
    assert_eq!(
        utc.deferred_from_ms,
        Some(1_699_943_400_000),
        "the fact a localized body says"
    );
    let toronto = TimeZone::get("America/Toronto").unwrap();
    assert_eq!(
        body(&queued, &toronto, ClockStyle::H12),
        "Held for quiet hours · due 1:30 AM"
    );
    assert!(notification(&task_at(AT), &TimeZone::UTC, ClockStyle::H24)
        .body
        .is_empty());

    let mut block = reminder(block_ref(), ReminderKind::Block, AT);
    assert_eq!(
        body(&block, &TimeZone::UTC, ClockStyle::H24),
        BLOCK_STARTING
    );
    block.deferred_from = queued.deferred_from;
    assert_eq!(
        body(&block, &TimeZone::UTC, ClockStyle::H24),
        "Time block starting · Held for quiet hours · due 06:30"
    );
}

#[test]
fn the_clock_style_follows_the_time_format_preference() {
    assert_eq!(
        ClockStyle::from_time_format(Some("h12"), true),
        ClockStyle::H12
    );
    assert_eq!(
        ClockStyle::from_time_format(Some("h24"), false),
        ClockStyle::H24
    );
    assert_eq!(
        ClockStyle::from_time_format(Some("locale"), true),
        ClockStyle::H24
    );
    assert_eq!(ClockStyle::from_time_format(None, false), ClockStyle::H12);
}

fn toronto_candidate(n: u8, at: SunriseTime) -> ReminderCandidate {
    ReminderCandidate {
        entity: EntityRef::new(EntityKind::Task, [n; 16]),
        kind: ReminderKind::Task,
        at,
        title: format!("task {n}"),
        own_lead_s: None,
        stream_lead_s: None,
    }
}

/// `candidates` planned by a reader in Toronto over `[now, to]`.
fn toronto_plan(candidates: &[ReminderCandidate], now: &str, to: &str) -> Vec<PlannedNotification> {
    let toronto = TimeZone::get("America/Toronto").unwrap();
    let intents = plan_reminders(
        candidates,
        now.parse().unwrap(),
        to.parse().unwrap(),
        &toronto,
        &ReminderSettings::default(),
    );
    plan(&intents, &toronto, ClockStyle::H24, MAX_SCHEDULED)
}

/// A floating and a zoned reminder at the wall clock `at`.
fn both_kinds_at(n: u8, at: civil::DateTime) -> [ReminderCandidate; 2] {
    [
        toronto_candidate(n, SunriseTime::floating(at)),
        toronto_candidate(n + 1, SunriseTime::zoned(at, "America/Toronto")),
    ]
}

fn ms_of(s: &str) -> i64 {
    s.parse::<Timestamp>().unwrap().as_millisecond()
}

/// 2026-11-01 falls back in Toronto: a reminder at 01:30 fires at the earlier
/// instant, 01:30 EDT, and one at 09:00 at 09:00 EST.
#[test]
fn a_reminder_in_a_fold_fires_at_the_earlier_instant() {
    let day = civil::date(2026, 11, 1);
    let candidates = [
        both_kinds_at(1, day.at(1, 30, 0, 0)),
        both_kinds_at(3, day.at(9, 0, 0, 0)),
    ]
    .concat();
    let planned = toronto_plan(&candidates, "2026-11-01T04:00Z", "2026-11-01T20:00Z");
    let fires: Vec<i64> = planned.iter().map(|n| n.fire_at_ms).collect();
    let (fold, morning) = (ms_of("2026-11-01T05:30Z"), ms_of("2026-11-01T14:00Z"));
    assert_eq!(fires, [fold, fold, morning, morning]);
}

/// Once the earlier instant has passed, the fold's second 01:30 is not a
/// second firing: the reminder has gone from the plan.
#[test]
fn a_reminder_in_a_fold_is_not_planned_again_at_the_later_instant() {
    let candidates = both_kinds_at(1, civil::date(2026, 11, 1).at(1, 30, 0, 0));
    assert!(toronto_plan(&candidates, "2026-11-01T05:31Z", "2026-11-01T20:00Z").is_empty());
}

/// 2026-03-08 springs forward in Toronto: a reminder at 02:30 moves forward by
/// the gap to 03:30 EDT, and one at 09:00 is 09:00 EDT.
#[test]
fn a_reminder_in_a_gap_moves_forward_by_the_gap() {
    let day = civil::date(2026, 3, 8);
    let candidates = [
        both_kinds_at(1, day.at(2, 30, 0, 0)),
        both_kinds_at(3, day.at(9, 0, 0, 0)),
    ]
    .concat();
    let planned = toronto_plan(&candidates, "2026-03-08T05:00Z", "2026-03-08T20:00Z");
    let fires: Vec<i64> = planned.iter().map(|n| n.fire_at_ms).collect();
    let (gap, morning) = (ms_of("2026-03-08T07:30Z"), ms_of("2026-03-08T13:00Z"));
    assert_eq!(fires, [gap, gap, morning, morning]);
}

/// Re-planning later on a DST day, before anything fires, gives the same
/// requests, so the second reconcile has nothing to do.
#[test]
fn a_second_reconcile_on_a_dst_day_is_empty() {
    for (at, first, again, to) in [
        (
            civil::date(2026, 11, 1).at(1, 30, 0, 0),
            "2026-11-01T04:00Z",
            "2026-11-01T05:00Z",
            "2026-11-01T20:00Z",
        ),
        (
            civil::date(2026, 3, 8).at(2, 30, 0, 0),
            "2026-03-08T05:00Z",
            "2026-03-08T07:00Z",
            "2026-03-08T20:00Z",
        ),
    ] {
        let candidates = both_kinds_at(1, at);
        let armed = toronto_plan(&candidates, first, to);
        let added = reconcile(&armed, &[]);
        assert_eq!(added.add, armed, "{first}");
        let replanned = toronto_plan(&candidates, again, to);
        let second = reconcile(&replanned, &pending_of(&added.add));
        assert!(second.is_empty(), "{first}: {second:?}");
        assert_eq!(second.kept.len(), 2, "{first}");
    }
}

/// A zone change re-plans a floating reminder to its wall clock in the new
/// zone, and the reconcile replaces it under the same identifier — the core
/// half of #520. A zoned reminder keeps its instant and its revision.
#[test]
fn a_zone_change_replaces_a_floating_reminder_and_keeps_a_zoned_one() {
    let nine = civil::date(2026, 6, 1).at(9, 0, 0, 0);
    let toronto = TimeZone::get("America/Toronto").unwrap();
    let london = TimeZone::get("Europe/London").unwrap();
    let candidate = |n: u8, at: SunriseTime| ReminderCandidate {
        entity: EntityRef::new(EntityKind::Task, [n; 16]),
        kind: ReminderKind::Task,
        at,
        title: format!("task {n}"),
        own_lead_s: None,
        stream_lead_s: None,
    };
    let candidates = [
        candidate(1, SunriseTime::floating(nine)),
        candidate(
            2,
            SunriseTime::Instant {
                at: nine.to_zoned(toronto.clone()).unwrap().timestamp(),
            },
        ),
    ];
    let now = Timestamp::from_second(1_780_272_000).unwrap(); // 2026-06-01T00:00Z
    let horizon = now + jiff::SignedDuration::from_hours(48);
    let settings = ReminderSettings::default();
    let in_zone = |tz: &TimeZone| {
        plan(
            &plan_reminders(&candidates, now, horizon, tz, &settings),
            tz,
            ClockStyle::H24,
            MAX_SCHEDULED,
        )
    };

    let armed = in_zone(&toronto);
    assert_eq!(armed.len(), 2);
    let moved = in_zone(&london);
    let r = reconcile(&moved, &pending_of(&armed));
    assert!(r.add.is_empty() && r.cancel.is_empty(), "{r:?}");
    assert_eq!(r.replace.len(), 1, "the floating one moved");
    assert_eq!(r.replace[0].entity, candidates[0].entity);
    assert_eq!(
        r.replace[0].fire_at_ms,
        nine.to_zoned(london).unwrap().timestamp().as_millisecond()
    );
    assert_eq!(
        r.kept,
        [identifier(candidates[1].entity, ReminderKind::Task)]
    );
}
