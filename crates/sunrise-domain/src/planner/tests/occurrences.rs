//! One occurrence of a recurring block: how its drag ripples, which other
//! occurrences it may move, and how its key enters the plan base.

use super::*;

const E: Date = date(2026, 11, 11);

/// The occurrence of a daily block `bid(1)` at 09:00–10:00 on `d`.
fn occurrence(d: Date) -> PlanBlock {
    PlanBlock {
        occurrence: Some(format!("{d}T09:00")),
        ..pblock(1, at(d, 9, 0), at(d, 10, 0), false)
    }
}

fn drag(d: Date, scope: OccurrenceScope, to_start: SunriseTime) -> PlanIntent {
    PlanIntent::MoveBlock {
        block: bid(1),
        occurrence: Some(PlanOccurrence {
            key: format!("{d}T09:00"),
            scope,
        }),
        to_start,
    }
}

/// Two days, each with an occurrence of `bid(1)` and a flexible block at
/// 11:00–12:00.
fn two_days_of_a_series() -> PlanInputs {
    let mut i = inputs(ts(D, 6, 0), vec![plan_day(D, 7, 23), plan_day(E, 7, 23)]);
    i.blocks.push(occurrence(D));
    i.blocks.push(occurrence(E));
    i.blocks.push(pblock(2, at(D, 11, 0), at(D, 12, 0), false));
    i.blocks.push(pblock(3, at(E, 11, 0), at(E, 12, 0), false));
    i
}

#[test]
fn a_dragged_occurrence_ripples_its_own_day_and_moves_alone() {
    let i = two_days_of_a_series();
    let diff = solve(&i, &drag(D, OccurrenceScope::This, at(D, 11, 0))).unwrap();
    assert_eq!(diff.moves.len(), 2, "{diff:#?}");
    let subject = &diff.moves[0];
    assert_eq!(
        (subject.item, subject.role, subject.occurrence.as_deref()),
        (bid(1), MoveRole::Subject, Some("2026-11-10T09:00"))
    );
    assert_eq!(slot_of(&subject.after), (ts(D, 11, 0), ts(D, 12, 0)));
    let rippled = mv(&diff, bid(2)).unwrap();
    assert_eq!(rippled.occurrence, None);
    assert_eq!(rippled.role, MoveRole::Rippled);
    // The other day's block and the other occurrence stay where they are.
    assert!(mv(&diff, bid(3)).is_none());
    assert!(diff.committable, "{:#?}", diff.violations);
}

#[test]
fn another_occurrence_is_fixed_unless_the_drag_reaches_it() {
    let i = two_days_of_a_series();
    // Onto the next day's occurrence: that one is in the way of this event
    // alone, and never ripples.
    let this = solve(&i, &drag(D, OccurrenceScope::This, at(E, 9, 0))).unwrap();
    assert!(this.violations.iter().any(|v| v.item == bid(1)
        && v.rule == PlanRule::OverlapsFixed
        && v.with == Some(bid(1))
        && v.introduced));
    assert_eq!(this.moves.len(), 1, "{this:#?}");
    // A drag of every event moves that one too, so it is in nobody's way.
    let all = solve(&i, &drag(D, OccurrenceScope::All, at(E, 9, 0))).unwrap();
    assert!(
        all.violations.iter().all(|v| v.with != Some(bid(1))),
        "{:#?}",
        all.violations
    );
    assert_ne!(this.id, all.id);
}

#[test]
fn a_recurring_block_is_dragged_by_its_occurrence() {
    let i = two_days_of_a_series();
    let whole = PlanIntent::MoveBlock {
        block: bid(1),
        occurrence: None,
        to_start: at(D, 11, 0),
    };
    assert_eq!(solve(&i, &whole), Err(PlanIntentError::Unknown(bid(1))));
    let elsewhere = PlanIntent::MoveBlock {
        block: bid(2),
        occurrence: Some(PlanOccurrence {
            key: "2026-11-10T11:00".into(),
            scope: OccurrenceScope::This,
        }),
        to_start: at(D, 13, 0),
    };
    assert_eq!(solve(&i, &elsewhere), Err(PlanIntentError::Unknown(bid(2))));
}

#[test]
fn an_occurrence_is_part_of_the_base() {
    let i = two_days_of_a_series();
    let mut other = i.clone();
    other.blocks[1].occurrence = Some("2026-11-11T09:30".into());
    assert_ne!(plan_base(&i), plan_base(&other));
}
