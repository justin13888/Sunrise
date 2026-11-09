use super::*;
use crate::constraint::{ConstraintSeverity, TimeOfDayRange};
use jiff::civil::{date, time, DateTime};
use proptest::prelude::*;

const NY: &str = "America/New_York";

fn zone() -> TimeZone {
    TimeZone::get(NY).unwrap()
}

fn rf(kind: EntityKind, n: u8) -> EntityRef {
    let mut b = [0u8; 16];
    b[0] = 1;
    b[15] = n;
    EntityRef::new(kind, b)
}

fn tid(n: u8) -> EntityRef {
    rf(EntityKind::Task, n)
}

fn bid(n: u8) -> EntityRef {
    rf(EntityKind::Block, n)
}

/// External events are named by any kind; the tests use blocks' ids from a
/// separate range.
fn eid(n: u8) -> EntityRef {
    rf(EntityKind::Block, 200 + n)
}

fn dt(d: Date, h: i8, m: i8) -> DateTime {
    d.to_datetime(time(h, m, 0, 0))
}

fn at(d: Date, h: i8, m: i8) -> SunriseTime {
    SunriseTime::zoned(dt(d, h, m), NY)
}

fn ts(d: Date, h: i8, m: i8) -> Timestamp {
    zone().to_zoned(dt(d, h, m)).unwrap().timestamp()
}

fn midnight(d: Date) -> Timestamp {
    zone().to_zoned(dt(d, 0, 0)).unwrap().timestamp()
}

/// The civil day, plannable from `wake` to `sleep` (hours; a sleep past 24
/// runs into the next morning).
fn plan_day(d: Date, wake: i8, sleep: i8) -> PlanDay {
    let next = d.tomorrow().unwrap();
    let day_end = if sleep > 24 {
        ts(next, sleep - 24, 0)
    } else {
        midnight(next)
    };
    let sleep_at = if sleep >= 24 {
        ts(next, sleep - 24, 0)
    } else {
        ts(d, sleep, 0)
    };
    PlanDay {
        date: d,
        day: (midnight(d), day_end),
        plannable: (ts(d, wake, 0), sleep_at),
    }
}

fn ptask(n: u8, start: Option<SunriseTime>, minutes: u64) -> PlanTask {
    PlanTask {
        id: tid(n),
        planned_at: start,
        estimate_s: Some(minutes * 60),
        target_due: None,
        hard_due: None,
        blocked_by: Vec::new(),
        constraints: Vec::new(),
        in_progress: false,
        bound: false,
    }
}

fn pblock(n: u8, start: SunriseTime, end: SunriseTime, fixed: bool) -> PlanBlock {
    PlanBlock {
        id: bid(n),
        starts_at: start,
        ends_at: end,
        fixed,
    }
}

fn inputs(now: Timestamp, window: Vec<PlanDay>) -> PlanInputs {
    PlanInputs {
        now,
        zone: zone(),
        window,
        tasks: Vec::new(),
        blocks: Vec::new(),
        events: Vec::new(),
        snap_s: DEFAULT_SNAP_S,
        default_task_duration_s: DEFAULT_TASK_DURATION_S,
        min_gap_s: 0,
        search_budget: SEARCH_BUDGET,
    }
}

const D: Date = date(2026, 11, 10);

fn base_day() -> PlanInputs {
    inputs(ts(D, 6, 0), vec![plan_day(D, 7, 23)])
}

fn mv(diff: &PlanDiff, id: EntityRef) -> Option<&PlanMove> {
    diff.moves.iter().find(|m| m.item == id)
}

fn slot_of(p: &PlanPosition) -> (Timestamp, Timestamp) {
    match p {
        PlanPosition::Slot { start, end } => (
            start.resolve_in(&zone()).unwrap(),
            end.resolve_in(&zone()).unwrap(),
        ),
        other => panic!("not a slot: {other:?}"),
    }
}

/// The inputs as they stand after `diff` is committed.
fn apply(inputs: &PlanInputs, diff: &PlanDiff) -> PlanInputs {
    let mut out = inputs.clone();
    let set = |out: &mut PlanInputs, id: EntityRef, p: &PlanPosition| {
        if let Some(t) = out.tasks.iter_mut().find(|t| t.id == id) {
            t.planned_at = match p {
                PlanPosition::Slot { start, .. } => Some(start.clone()),
                PlanPosition::Day(d) => Some(SunriseTime::all_day(*d)),
                PlanPosition::Unplanned => None,
            };
        }
        if let Some(b) = out.blocks.iter_mut().find(|b| b.id == id) {
            if let PlanPosition::Slot { start, end } = p {
                b.starts_at = start.clone();
                b.ends_at = end.clone();
            }
        }
    };
    for m in &diff.moves {
        set(&mut out, m.item, &m.after);
    }
    for u in &diff.unscheduled {
        set(&mut out, u.item, &PlanPosition::Day(u.day));
    }
    out
}

#[test]
fn a_moved_block_pushes_the_task_it_lands_on_to_the_nearest_free_slot() {
    let mut i = base_day();
    i.blocks.push(pblock(1, at(D, 9, 0), at(D, 10, 0), false));
    i.tasks.push(ptask(1, Some(at(D, 11, 0)), 30));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 11, 0),
        },
    )
    .unwrap();
    assert_eq!(diff.moves.len(), 2, "{diff:#?}");
    assert_eq!(diff.moves[0].item, bid(1));
    assert_eq!(diff.moves[0].role, MoveRole::Subject);
    assert_eq!(
        diff.moves[0].after,
        PlanPosition::Slot {
            start: at(D, 11, 0),
            end: at(D, 12, 0)
        }
    );
    let rippled = mv(&diff, tid(1)).unwrap();
    assert_eq!(rippled.role, MoveRole::Rippled);
    // 10:30 is half an hour away; 12:00, the first free slot after, is an
    // hour.
    assert_eq!(slot_of(&rippled.after), (ts(D, 10, 30), ts(D, 11, 0)));
    // Kept in its own kind.
    assert!(matches!(
        rippled.after,
        PlanPosition::Slot {
            start: SunriseTime::Zoned { .. },
            ..
        }
    ));
    assert!(diff.committable);
    assert!(diff.violations.is_empty(), "{:#?}", diff.violations);
}

#[test]
fn a_drop_where_the_subject_already_is_changes_nothing() {
    let mut i = base_day();
    i.blocks.push(pblock(1, at(D, 9, 0), at(D, 10, 0), true));
    i.tasks.push(ptask(1, Some(at(D, 9, 30)), 30));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 9, 0),
        },
    )
    .unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    // The overlap was there already; it is reported and not introduced.
    assert!(diff.violations.iter().all(|v| !v.introduced));
}

#[test]
fn a_fixed_event_never_moves_and_the_drop_onto_it_is_hard() {
    let mut i = base_day();
    i.events.push(PlanEvent {
        id: eid(1),
        start: ts(D, 10, 0),
        end: ts(D, 11, 0),
        busy: true,
    });
    i.tasks.push(ptask(1, Some(at(D, 8, 0)), 30));
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 10, 0)),
        },
    )
    .unwrap();
    assert_eq!(diff.moves.len(), 1);
    let v = &diff.violations[0];
    assert_eq!(
        (v.item, v.rule, v.severity, v.introduced, v.with),
        (
            tid(1),
            PlanRule::OverlapsFixed,
            PlanSeverity::Hard,
            true,
            Some(eid(1))
        )
    );
    // Hard, not Blocking: the user's drop is deliberate.
    assert!(diff.committable);
}

#[test]
fn a_free_event_is_never_in_the_way() {
    let mut i = base_day();
    i.events.push(PlanEvent {
        id: eid(1),
        start: ts(D, 10, 0),
        end: ts(D, 11, 0),
        busy: false,
    });
    i.tasks.push(ptask(1, Some(at(D, 8, 0)), 30));
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 10, 0)),
        },
    )
    .unwrap();
    assert!(diff.violations.is_empty(), "{:#?}", diff.violations);
}

#[test]
fn a_dependency_chain_ripples_in_order_and_breaks_nothing() {
    // A 09:00–10:00 blocks B 10:00–10:30, which blocks C 10:30–11:00. Moving
    // A to 10:00 must push B after 11:00 and C after B.
    let mut i = base_day();
    let mut a = ptask(1, Some(at(D, 9, 0)), 60);
    let mut b = ptask(2, Some(at(D, 10, 0)), 30);
    let mut c = ptask(3, Some(at(D, 10, 30)), 30);
    b.blocked_by.push(PlanBlocker {
        task: tid(1),
        end: Some(ts(D, 10, 0)),
    });
    c.blocked_by.push(PlanBlocker {
        task: tid(2),
        end: Some(ts(D, 10, 30)),
    });
    a.estimate_s = Some(3600);
    i.tasks.extend([a, b, c]);
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 10, 0)),
        },
    )
    .unwrap();
    let b_after = slot_of(&mv(&diff, tid(2)).unwrap().after);
    let c_after = slot_of(&mv(&diff, tid(3)).unwrap().after);
    assert!(b_after.0 >= ts(D, 11, 0), "{diff:#?}");
    assert!(c_after.0 >= b_after.1, "{diff:#?}");
    assert!(
        diff.violations
            .iter()
            .all(|v| v.severity < PlanSeverity::Hard),
        "{:#?}",
        diff.violations
    );
}

fn hard_window(start: i8, end: i8) -> ScheduleConstraint {
    ScheduleConstraint {
        time_of_day: Some(TimeOfDayRange::new(
            time(start, 0, 0, 0),
            time(end, 0, 0, 0),
        )),
        days_of_week: crate::constraint::WeekdaySet::default(),
        date_range: None,
        severity: ConstraintSeverity::Hard,
        unknown: crate::unknown::Unknowns::default(),
    }
}

#[test]
fn a_drop_outside_a_hard_time_of_day_window_is_refused() {
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 30);
    t.constraints.push(hard_window(9, 12));
    i.tasks.push(t);
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 14, 0)),
        },
    )
    .unwrap();
    assert!(!diff.committable);
    assert!(diff
        .violations
        .iter()
        .any(|v| v.rule == PlanRule::OutsideTimeOfDay && v.severity == PlanSeverity::Blocking));
}

#[test]
fn a_rippled_task_with_a_hard_window_is_unscheduled_rather_than_moved_out_of_it() {
    // The task may only run 09:00–10:00, and the subject takes all of it.
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 60);
    t.constraints.push(hard_window(9, 10));
    i.tasks.push(t);
    i.blocks.push(pblock(1, at(D, 14, 0), at(D, 15, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 9, 0),
        },
    )
    .unwrap();
    assert_eq!(diff.unscheduled.len(), 1, "{diff:#?}");
    assert_eq!(diff.unscheduled[0].item, tid(1));
    assert_eq!(diff.unscheduled[0].day, D);
    assert_eq!(
        diff.unscheduled[0].reason,
        UnscheduleReason::WouldViolateHard
    );
    assert!(diff.committable);
}

#[test]
fn nothing_is_placed_past_sleep_and_a_full_evening_unschedules() {
    // Plannable 07:00–23:00. The task sits 22:00–22:30 and the subject
    // takes 22:00–23:00; everything before is busy.
    let mut i = base_day();
    i.events.push(PlanEvent {
        id: eid(1),
        start: ts(D, 7, 0),
        end: ts(D, 22, 0),
        busy: true,
    });
    i.tasks.push(ptask(1, Some(at(D, 22, 0)), 30));
    i.blocks.push(pblock(1, at(D, 6, 0), at(D, 7, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 22, 0),
        },
    )
    .unwrap();
    assert_eq!(diff.unscheduled.len(), 1, "{diff:#?}");
    assert_eq!(diff.unscheduled[0].reason, UnscheduleReason::NoRoomInDay);
}

#[test]
fn a_sleep_past_midnight_is_part_of_the_day() {
    // Plannable 09:00 to 01:00 the next morning; the only free time is
    // 00:00–01:00, which still belongs to D.
    let mut i = inputs(ts(D, 6, 0), vec![plan_day(D, 9, 25)]);
    i.events.push(PlanEvent {
        id: eid(1),
        start: ts(D, 9, 0),
        end: ts(D, 23, 0),
        busy: true,
    });
    i.tasks.push(ptask(1, Some(at(D, 23, 0)), 30));
    i.blocks.push(pblock(1, at(D, 6, 0), at(D, 7, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 23, 0),
        },
    )
    .unwrap();
    let next = D.tomorrow().unwrap();
    let after = slot_of(&mv(&diff, tid(1)).unwrap().after);
    assert_eq!(after, (ts(next, 0, 0), ts(next, 0, 30)), "{diff:#?}");
}

#[test]
fn a_rippled_task_moves_before_rather_than_past_its_hard_deadline() {
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 11, 0)), 30);
    t.hard_due = Some(ts(D, 11, 45));
    i.tasks.push(t);
    i.blocks.push(pblock(1, at(D, 15, 0), at(D, 16, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 11, 0),
        },
    )
    .unwrap();
    let after = slot_of(&mv(&diff, tid(1)).unwrap().after);
    assert_eq!(after, (ts(D, 10, 30), ts(D, 11, 0)), "{diff:#?}");
    assert!(diff.violations.is_empty());
}

#[test]
fn a_subject_dropped_after_its_hard_deadline_is_flagged_and_committable() {
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 30);
    t.hard_due = Some(ts(D, 12, 0));
    i.tasks.push(t);
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 13, 0)),
        },
    )
    .unwrap();
    assert!(diff.committable);
    assert!(diff
        .violations
        .iter()
        .any(|v| v.rule == PlanRule::AfterHardDeadline && v.introduced));
}

#[test]
fn a_dst_day_places_across_the_gap_in_wall_clock_terms() {
    // 2026-03-08: New York springs forward at 02:00. The day is 23 hours.
    let dst = date(2026, 3, 8);
    let mut i = inputs(ts(dst, 0, 0), vec![plan_day(dst, 0, 24)]);
    i.tasks.push(ptask(1, None, 60));
    let diff = solve(
        &i,
        &PlanIntent::AutoPlace {
            tasks: vec![tid(1)],
            day: dst,
            not_before: Some(at(dst, 1, 30)),
        },
    )
    .unwrap();
    let m = mv(&diff, tid(1)).unwrap();
    let PlanPosition::Slot { start, end } = &m.after else {
        panic!("{m:?}");
    };
    assert_eq!(start, &at(dst, 1, 30));
    // One hour after 01:30 EST is 03:30 EDT.
    assert_eq!(end, &at(dst, 3, 30));
}

#[test]
fn a_floating_task_resolves_in_the_reader_zone() {
    let civil = dt(D, 9, 0);
    let mut i = base_day();
    i.tasks
        .push(ptask(1, Some(SunriseTime::floating(civil)), 30));
    i.events.push(PlanEvent {
        id: eid(1),
        start: ts(D, 9, 0),
        end: ts(D, 10, 0),
        busy: true,
    });
    // Read in New York the task overlaps the event, which is a fixed overlap
    // already there; read in Tokyo it is a different instant and does not.
    let ny = solve(
        &i,
        &PlanIntent::Unschedule {
            items: vec![tid(1)],
            keep: UnscheduleKeep::Day,
        },
    )
    .unwrap();
    assert_eq!(ny.moves[0].after, PlanPosition::Day(D));
    let mut tokyo = i.clone();
    tokyo.zone = TimeZone::get("Asia/Tokyo").unwrap();
    let diff = solve(
        &tokyo,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 9, 0)),
        },
    )
    .unwrap();
    assert_eq!(
        mv(&diff, tid(1)).unwrap().before,
        PlanPosition::Slot {
            start: SunriseTime::floating(civil),
            end: SunriseTime::floating(dt(D, 9, 30)),
        }
    );
}

#[test]
fn bulk_defer_and_unschedule_write_days_and_clear_plans() {
    let mut i = base_day();
    i.window.push(plan_day(D.tomorrow().unwrap(), 7, 23));
    i.tasks.push(ptask(1, Some(at(D, 9, 0)), 30));
    i.tasks.push(ptask(2, Some(at(D, 10, 0)), 30));
    let to = D.tomorrow().unwrap();
    let diff = solve(
        &i,
        &PlanIntent::BulkDefer {
            tasks: vec![tid(2), tid(1)],
            to,
            place: false,
        },
    )
    .unwrap();
    assert_eq!(
        diff.moves.iter().map(|m| m.item).collect::<Vec<_>>(),
        vec![tid(1), tid(2)]
    );
    assert!(diff.moves.iter().all(|m| m.after == PlanPosition::Day(to)));
    let diff = solve(
        &i,
        &PlanIntent::Unschedule {
            items: vec![tid(1)],
            keep: UnscheduleKeep::Nothing,
        },
    )
    .unwrap();
    assert_eq!(diff.moves[0].after, PlanPosition::Unplanned);
}

#[test]
fn auto_place_fills_the_first_free_slots_in_order_and_unschedules_the_rest() {
    let mut i = inputs(ts(D, 6, 0), vec![plan_day(D, 9, 11)]);
    i.events.push(PlanEvent {
        id: eid(1),
        start: ts(D, 9, 30),
        end: ts(D, 10, 0),
        busy: true,
    });
    i.tasks.push(ptask(1, None, 30));
    i.tasks.push(ptask(2, None, 60));
    i.tasks.push(ptask(3, None, 30));
    let diff = solve(
        &i,
        &PlanIntent::AutoPlace {
            tasks: vec![tid(2), tid(1), tid(3)],
            day: D,
            not_before: None,
        },
    )
    .unwrap();
    // 2 (an hour) cannot fit 09:00–09:30, so it takes 10:00–11:00; 1 takes
    // 09:00; 3 has no room left.
    assert_eq!(
        slot_of(&mv(&diff, tid(2)).unwrap().after),
        (ts(D, 10, 0), ts(D, 11, 0))
    );
    assert_eq!(
        slot_of(&mv(&diff, tid(1)).unwrap().after),
        (ts(D, 9, 0), ts(D, 9, 30))
    );
    assert_eq!(diff.unscheduled.len(), 1);
    assert_eq!(diff.unscheduled[0].item, tid(3));
}

#[test]
fn a_resize_that_inverts_the_block_is_blocking() {
    let mut i = base_day();
    i.blocks.push(pblock(1, at(D, 9, 0), at(D, 10, 0), true));
    let diff = solve(
        &i,
        &PlanIntent::ResizeBlock {
            block: bid(1),
            edge: Edge::End,
            to: at(D, 8, 0),
        },
    )
    .unwrap();
    assert!(!diff.committable);
    assert_eq!(diff.violations[0].rule, PlanRule::Invalid);
}

#[test]
fn intents_naming_the_wrong_thing_are_refused() {
    let i = base_day();
    assert_eq!(
        solve(
            &i,
            &PlanIntent::MoveTask {
                task: bid(1),
                to: SlotTarget::Day(D),
            }
        ),
        Err(PlanIntentError::WrongKind(bid(1)))
    );
    assert_eq!(
        solve(
            &i,
            &PlanIntent::MoveTask {
                task: tid(9),
                to: SlotTarget::Day(D),
            }
        ),
        Err(PlanIntentError::Unknown(tid(9)))
    );
    assert_eq!(
        solve(
            &i,
            &PlanIntent::BulkDefer {
                tasks: Vec::new(),
                to: D,
                place: false,
            }
        ),
        Err(PlanIntentError::BadList)
    );
}

#[test]
fn a_past_item_is_fixed() {
    let mut i = base_day();
    i.now = ts(D, 12, 0);
    i.tasks.push(ptask(1, Some(at(D, 11, 0)), 120));
    i.blocks.push(pblock(1, at(D, 15, 0), at(D, 16, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 12, 0),
        },
    )
    .unwrap();
    assert!(mv(&diff, tid(1)).is_none());
    assert!(diff
        .violations
        .iter()
        .any(|v| v.item == bid(1) && v.rule == PlanRule::OverlapsFixed));
}

// ---- properties ----

/// A day of `n` tasks, `b` movable blocks, a fixed block and an event, with
/// the given starts (quarter hours from 07:00), estimates and dependencies.
fn world(starts: &[(u8, u8)], deps: &[(u8, u8)], event_at: u8, now_q: u8) -> PlanInputs {
    let q = |n: u8| ts(D, 7, 0).as_second() + i64::from(n) * 900;
    let mut i = base_day();
    i.now = Timestamp::from_second(q(now_q)).unwrap();
    for (n, &(start, len)) in starts.iter().enumerate() {
        let n = u8::try_from(n).unwrap();
        let start_t = SunriseTime::instant(Timestamp::from_second(q(start)).unwrap());
        let mut t = ptask(n, Some(start_t), u64::from(len % 6 + 1) * 15);
        for &(a, b) in deps {
            if usize::from(b) == usize::from(n) && a < b {
                t.blocked_by.push(PlanBlocker {
                    task: tid(a),
                    end: None,
                });
            }
        }
        i.tasks.push(t);
    }
    i.blocks.push(PlanBlock {
        id: bid(100),
        starts_at: SunriseTime::instant(Timestamp::from_second(q(20)).unwrap()),
        ends_at: SunriseTime::instant(Timestamp::from_second(q(24)).unwrap()),
        fixed: true,
    });
    i.events.push(PlanEvent {
        id: eid(1),
        start: Timestamp::from_second(q(event_at)).unwrap(),
        end: Timestamp::from_second(q(event_at + 4)).unwrap(),
        busy: true,
    });
    i
}

fn shuffled(i: &PlanInputs) -> PlanInputs {
    let mut o = i.clone();
    o.tasks.reverse();
    o.blocks.reverse();
    o.events.reverse();
    o.window.reverse();
    o
}

prop_compose! {
    fn arb_case()(
        starts in prop::collection::vec((0u8..60, 0u8..6), 1..12),
        deps in prop::collection::vec((0u8..12, 0u8..12), 0..6),
        event_at in 0u8..56,
        now_q in 0u8..8,
        subject in 0usize..12,
        to_q in 0u8..60,
    ) -> (PlanInputs, PlanIntent) {
        let i = world(&starts, &deps, event_at, now_q);
        let n = u8::try_from(subject % starts.len()).unwrap();
        let to = SunriseTime::instant(
            Timestamp::from_second(ts(D, 7, 0).as_second() + i64::from(to_q) * 900).unwrap(),
        );
        (i, PlanIntent::MoveTask { task: tid(n), to: SlotTarget::At(to) })
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: sunrise_test_seed::proptest_cases(128),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..ProptestConfig::default()
    })]

    #[test]
    fn the_same_inputs_in_any_order_give_the_same_diff((i, intent) in arb_case()) {
        let a = solve(&i, &intent).unwrap();
        let b = solve(&shuffled(&i), &intent).unwrap();
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(a.id, solve(&i, &intent).unwrap().id);
    }

    #[test]
    fn fixed_items_never_ripple((i, intent) in arb_case()) {
        let diff = solve(&i, &intent).unwrap();
        let now = i.now;
        for m in diff.moves.iter().filter(|m| m.role == MoveRole::Rippled) {
            prop_assert!(m.item != bid(100) && m.item != eid(1));
            let (s, _) = slot_of(&m.before);
            prop_assert!(s >= now, "a past item moved: {:?}", m);
        }
    }

    #[test]
    fn a_drop_in_place_is_empty((i, _intent) in arb_case(), n in 0usize..12) {
        let n = n % i.tasks.len();
        let t = &i.tasks[n];
        let intent = PlanIntent::MoveTask {
            task: t.id,
            to: SlotTarget::At(t.planned_at.clone().unwrap()),
        };
        let diff = solve(&i, &intent).unwrap();
        prop_assert!(diff.moves.is_empty() && diff.unscheduled.is_empty());
    }

    #[test]
    fn committing_then_previewing_again_is_empty((i, intent) in arb_case()) {
        let diff = solve(&i, &intent).unwrap();
        let again = solve(&apply(&i, &diff), &intent).unwrap();
        prop_assert!(again.is_empty(), "{:#?}", again);
    }

    #[test]
    fn no_hard_violation_is_introduced_on_anything_but_the_subject(
        (i, intent) in arb_case()
    ) {
        let diff = solve(&i, &intent).unwrap();
        let PlanIntent::MoveTask { task, .. } = intent else { unreachable!() };
        if !diff.truncated {
            for v in &diff.violations {
                prop_assert!(
                    v.item == task || !v.introduced || v.severity < PlanSeverity::Hard,
                    "{:#?}", diff
                );
            }
        }
    }
}
