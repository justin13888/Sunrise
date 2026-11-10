//! Properties of `solve` over generated days.

use super::*;
use proptest::prelude::*;

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
