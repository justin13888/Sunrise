//! The rules a position is judged by (`docs/08-features/planner.md`
//! §Severity), and the violations a set of positions leaves.

use super::{to_ts, Class, PlanViolation, Pos, World};
use crate::constraint::{ConstraintSeverity, ScheduleConstraint, WeekdaySet};
use crate::time::SunriseTime;
use jiff::civil::Date;
use sunrise_id::EntityRef;

/// The rule a violation breaks.
///
/// Place annotations (`PlaceUnknown`, `PlaceMismatch`) are not produced by
/// this build: they are `Info` only and never a solver input (ADR-0051 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlanRule {
    /// Overlaps an external event, a fixed block, or a past or in-progress
    /// item.
    OverlapsFixed,
    /// Overlaps another movable item.
    Overlaps,
    /// Ends after its hard deadline is missed.
    AfterHardDeadline,
    /// Ends after its target is missed.
    AfterTarget,
    /// Starts before a blocker ends, or has a blocker with no time.
    BeforeBlocker,
    /// Outside a constraint's time of day.
    OutsideTimeOfDay,
    /// On a weekday a constraint excludes.
    WrongDayOfWeek,
    /// Outside a constraint's date range.
    OutsideDateRange,
    /// Outside its day's plannable window (wake to sleep).
    OutsideDay,
    /// The subject starts before now.
    InPast,
    /// The write would fail the entity's invariants: a block whose end is not
    /// after its start.
    Invalid,
}

impl PlanRule {
    /// A stable code for the rule.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::OverlapsFixed => "overlaps_fixed",
            Self::Overlaps => "overlaps",
            Self::AfterHardDeadline => "after_hard_deadline",
            Self::AfterTarget => "after_target",
            Self::BeforeBlocker => "before_blocker",
            Self::OutsideTimeOfDay => "outside_time_of_day",
            Self::WrongDayOfWeek => "wrong_day_of_week",
            Self::OutsideDateRange => "outside_date_range",
            Self::OutsideDay => "outside_day",
            Self::InPast => "in_past",
            Self::Invalid => "invalid",
        }
    }
}

/// How bad a violation is. Ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlanSeverity {
    /// Something to see that nobody can decide yet.
    Info,
    /// A soft constraint, a target, the day's bounds, a flexible overlap.
    Soft,
    /// A hard deadline, a dependency, an overlap with a fixed item.
    Hard,
    /// A write an ordinary submit would refuse: the diff is not committable.
    Blocking,
}

/// One rule broken at a position: the rule, its severity, the other party.
pub(crate) type Broken = (PlanRule, PlanSeverity, Option<EntityRef>);

/// The rules item `i` breaks at `[s, e)` other than overlaps, reading every
/// other item's position from `pos`.
pub(crate) fn rules_at(world: &World<'_>, i: usize, s: i64, e: i64, pos: &[Pos]) -> Vec<Broken> {
    let mut out = Vec::new();
    let item = &world.items[i];
    if let Some(t) = item.task {
        if t.hard_due.is_some_and(|d| e > d.as_second()) {
            out.push((PlanRule::AfterHardDeadline, PlanSeverity::Hard, None));
        }
        if t.target_due.is_some_and(|d| e > d.as_second()) {
            out.push((PlanRule::AfterTarget, PlanSeverity::Soft, None));
        }
        for b in &t.blocked_by {
            let end = match world.index.get(&b.task) {
                Some(&j) => pos[j].slot().map(|(_, be)| be),
                None => b.end.map(jiff::Timestamp::as_second),
            };
            match end {
                Some(be) if s < be => {
                    out.push((PlanRule::BeforeBlocker, PlanSeverity::Hard, Some(b.task)));
                }
                Some(_) => {}
                None => out.push((PlanRule::BeforeBlocker, PlanSeverity::Info, Some(b.task))),
            }
        }
        for c in &t.constraints {
            if let Some(rule) = constraint_rule(world, c, s) {
                out.push((rule, severity_of(c), None));
            }
        }
    }
    if item.class != Class::Fixed {
        if let Some(d) = world.day_at(s) {
            let (lo, hi) = world.window[d].plannable;
            if s < lo.as_second() || e > hi.as_second() {
                out.push((PlanRule::OutsideDay, PlanSeverity::Soft, None));
            }
        }
    }
    if item.class == Class::Subject && s < world.inputs.now.as_second() {
        out.push((PlanRule::InPast, PlanSeverity::Soft, None));
    }
    out
}

/// Whether a slot starting at `s` breaks a `hard` time constraint of item
/// `i`: a position no rippled item may take (ADR-0048 §4).
pub(crate) fn blocks_at(world: &World<'_>, i: usize, s: i64) -> bool {
    world.items[i].task.is_some_and(|t| {
        t.constraints.iter().any(|c| {
            c.severity == ConstraintSeverity::Hard && constraint_rule(world, c, s).is_some()
        })
    })
}

/// Whether task `i` with no time on day `d` breaks a `hard` time
/// constraint: the no-time lane is then no place for it either.
pub(crate) fn blocks_on_day(world: &World<'_>, i: usize, d: Date) -> bool {
    day_rules(world, i, d)
        .iter()
        .any(|b| b.1 == PlanSeverity::Blocking)
}

/// The constraint rules task `i` breaks with no time on day `d`, read as an
/// ordinary submit reads an all-day plan: at the start of the day in the
/// reader's zone. A `hard` one is Blocking there too, so the no-time lane is
/// not a way around it.
pub(crate) fn day_rules(world: &World<'_>, i: usize, d: Date) -> Vec<Broken> {
    let Some(t) = world.items[i].task else {
        return Vec::new();
    };
    let Some(s) = SunriseTime::all_day(d).resolve_in(&world.inputs.zone) else {
        return Vec::new();
    };
    let s = s.as_second();
    t.constraints
        .iter()
        .filter_map(|c| constraint_rule(world, c, s).map(|rule| (rule, severity_of(c), None)))
        .collect()
}

fn severity_of(c: &ScheduleConstraint) -> PlanSeverity {
    if c.severity == ConstraintSeverity::Hard {
        PlanSeverity::Blocking
    } else {
        PlanSeverity::Soft
    }
}

/// The dimension of `c` that a slot starting at `s` breaks, or `None`.
///
/// Checked at the start, as an ordinary submit checks a plan
/// (`docs/02-domain/scheduling-constraints.md`), so Blocking here is exactly
/// what that submit would refuse.
fn constraint_rule(world: &World<'_>, c: &ScheduleConstraint, s: i64) -> Option<PlanRule> {
    let zone = &world.inputs.zone;
    let at = |x: i64| to_ts(x).to_zoned(zone.clone());
    let points = [at(s)];
    if points.iter().all(|z| c.is_satisfied_at(z)) {
        return None;
    }
    let only = |time_of_day, days_of_week, date_range| ScheduleConstraint {
        time_of_day,
        days_of_week,
        date_range,
        severity: c.severity.clone(),
        unknown: crate::unknown::Unknowns::default(),
    };
    if let Some(t) = &c.time_of_day {
        let tod = only(Some(t.clone()), WeekdaySet::default(), None);
        if !points.iter().all(|z| tod.is_satisfied_at(z)) {
            return Some(PlanRule::OutsideTimeOfDay);
        }
    }
    let dow = only(c.time_of_day.clone(), c.days_of_week.clone(), None);
    if !points.iter().all(|z| dow.is_satisfied_at(z)) {
        return Some(PlanRule::WrongDayOfWeek);
    }
    Some(PlanRule::OutsideDateRange)
}

/// Every violation on a movable item or a subject at `pos`. `invalid` names a
/// subject whose write would fail its invariants.
pub(crate) fn all_violations(
    world: &World<'_>,
    pos: &[Pos],
    invalid: Option<usize>,
) -> Vec<PlanViolation> {
    let mut slots: Vec<(i64, i64, usize)> = pos
        .iter()
        .enumerate()
        .filter_map(|(j, p)| p.slot().map(|(s, e)| (s, e, j)))
        .filter(|&(s, e, j)| e > s && world.items[j].class != Class::Off)
        .collect();
    slots.sort_unstable();
    let mut out = Vec::new();
    for (i, item) in world.items.iter().enumerate() {
        if !matches!(item.class, Class::Subject | Class::Movable) {
            continue;
        }
        let mut push = |(rule, severity, with): Broken| {
            out.push(PlanViolation {
                item: item.id,
                rule,
                severity,
                introduced: false,
                with,
            });
        };
        if invalid == Some(i) {
            push((PlanRule::Invalid, PlanSeverity::Blocking, None));
            continue;
        }
        let (s, e) = match &pos[i] {
            Pos::Slot(s, e) => (*s, *e),
            Pos::Day(d) => {
                for b in day_rules(world, i, *d) {
                    push(b);
                }
                continue;
            }
            Pos::Unplanned => continue,
        };
        for &(s2, e2, j) in &slots {
            if s2 >= e {
                break;
            }
            if j == i || e2 <= s {
                continue;
            }
            let other = &world.items[j];
            if other.class == Class::Fixed {
                push((PlanRule::OverlapsFixed, PlanSeverity::Hard, Some(other.id)));
            } else {
                push((PlanRule::Overlaps, PlanSeverity::Soft, Some(other.id)));
            }
        }
        for b in rules_at(world, i, s, e, pos) {
            push(b);
        }
    }
    out
}
