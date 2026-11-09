//! The world one solve works in: every item the gesture can touch, its
//! class and position, and the report a solve's final positions make.

use super::encode::{diff_id, plan_base};
use super::{
    intent_items, retime, rules, to_ts, MoveRole, PlanDay, PlanDiff, PlanDiffId, PlanInputs,
    PlanIntent, PlanIntentError, PlanMove, PlanPosition, PlanRule, PlanSeverity, PlanTask,
    PlanUnscheduled, PlanViolation, UnscheduleReason,
};
use crate::time::SunriseTime;
use jiff::civil::Date;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

/// An internal position, in epoch seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pos {
    Slot(i64, i64),
    Day(Date),
    Unplanned,
}

impl Pos {
    pub(super) fn out(&self) -> PlanPosition {
        match self {
            Self::Slot(..) => unreachable!("a slot is rendered from its item's kinds"),
            Self::Day(d) => PlanPosition::Day(*d),
            Self::Unplanned => PlanPosition::Unplanned,
        }
    }

    pub(crate) fn slot(&self) -> Option<(i64, i64)> {
        match self {
            Self::Slot(s, e) => Some((*s, *e)),
            _ => None,
        }
    }
}

/// How an item takes part in a solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Never moves: an event, a fixed block, a past or in-progress item.
    Fixed,
    /// May ripple.
    Movable,
    /// Named by the intent.
    Subject,
    /// Not on the grid: an untimed task, or one bound to a block.
    Off,
}

#[derive(Debug, Clone)]
pub(crate) struct Item<'a> {
    pub(crate) id: EntityRef,
    pub(crate) is_task: bool,
    pub(crate) class: Class,
    pub(crate) before: Pos,
    /// The window day the item's slot starts in.
    pub(crate) day: Option<usize>,
    /// Its duration, seconds.
    pub(crate) dur: i64,
    pub(crate) start_kind: Option<SunriseTime>,
    pub(crate) end_kind: Option<SunriseTime>,
    pub(crate) task: Option<&'a PlanTask>,
}

#[derive(Debug)]
pub(crate) struct World<'a> {
    pub(crate) inputs: &'a PlanInputs,
    pub(crate) window: Vec<PlanDay>,
    /// Every item, by id.
    pub(crate) items: Vec<Item<'a>>,
    pub(crate) index: BTreeMap<EntityRef, usize>,
    pub(crate) subjects: Vec<usize>,
    /// Each task's dependents in the world.
    pub(crate) dependents: BTreeMap<EntityRef, Vec<usize>>,
    pub(crate) snap: i64,
    pub(crate) gap: i64,
}

impl<'a> World<'a> {
    pub(super) fn build(
        inputs: &'a PlanInputs,
        intent: &PlanIntent,
    ) -> Result<Self, PlanIntentError> {
        let zone = &inputs.zone;
        let now = inputs.now.as_second();
        let mut window = inputs.window.clone();
        window.sort_by_key(|d| d.date);
        window.dedup_by_key(|d| d.date);
        let named: BTreeSet<EntityRef> = intent_items(intent)?;
        let day_of = |s: i64| {
            window
                .iter()
                .position(|d| d.day.0.as_second() <= s && s < d.day.1.as_second())
        };
        let mut items = Vec::new();
        for t in &inputs.tasks {
            let dur = i64::try_from(
                t.estimate_s
                    .filter(|&e| e > 0)
                    .unwrap_or(u64::from(inputs.default_task_duration_s.max(1))),
            )
            .unwrap_or(i64::MAX / 4)
            .min(7 * 86_400);
            let subject = named.contains(&t.id);
            let (before, start_kind) = match &t.planned_at {
                None => (Pos::Unplanned, None),
                Some(SunriseTime::AllDay { date }) => (Pos::Day(*date), None),
                Some(p) => match p.resolve_in(zone) {
                    Some(s) => {
                        let s = s.as_second();
                        (Pos::Slot(s, s + dur), Some(p.clone()))
                    }
                    None => (Pos::Unplanned, None),
                },
            };
            if t.bound && !subject {
                continue;
            }
            let day = before.slot().and_then(|(s, _)| day_of(s));
            let class = if subject {
                Class::Subject
            } else {
                match before {
                    Pos::Slot(s, _) if !t.in_progress && s >= now && day.is_some() => {
                        Class::Movable
                    }
                    Pos::Slot(..) if day.is_some() => Class::Fixed,
                    _ => Class::Off,
                }
            };
            if class == Class::Off && !subject {
                continue;
            }
            items.push(Item {
                id: t.id,
                is_task: true,
                class,
                before,
                day,
                dur,
                end_kind: start_kind.clone(),
                start_kind,
                task: Some(t),
            });
        }
        for b in &inputs.blocks {
            let (Some(s), Some(e)) = (b.starts_at.resolve_in(zone), b.ends_at.resolve_in(zone))
            else {
                continue;
            };
            let (s, e) = (s.as_second(), e.as_second());
            let day = day_of(s);
            let subject = named.contains(&b.id);
            let class = if subject {
                Class::Subject
            } else if day.is_none() && day_of(e - 1).is_none() {
                continue;
            } else if b.fixed || s < now || day.is_none() {
                Class::Fixed
            } else {
                Class::Movable
            };
            items.push(Item {
                id: b.id,
                is_task: false,
                class,
                before: Pos::Slot(s, e.max(s)),
                day,
                dur: (e - s).max(0),
                start_kind: Some(b.starts_at.clone()),
                end_kind: Some(b.ends_at.clone()),
                task: None,
            });
        }
        for ev in &inputs.events {
            if !ev.busy {
                continue;
            }
            let (s, e) = (ev.start.as_second(), ev.end.as_second());
            if e <= s {
                continue;
            }
            items.push(Item {
                id: ev.id,
                is_task: false,
                class: Class::Fixed,
                before: Pos::Slot(s, e),
                day: day_of(s),
                dur: e - s,
                start_kind: None,
                end_kind: None,
                task: None,
            });
        }
        items.sort_by_key(|i| i.id);
        items.dedup_by_key(|i| i.id);
        let index: BTreeMap<EntityRef, usize> =
            items.iter().enumerate().map(|(n, i)| (i.id, n)).collect();
        let mut subjects = Vec::new();
        for id in &named {
            match index.get(id) {
                Some(&i) => subjects.push(i),
                None => return Err(PlanIntentError::Unknown(*id)),
            }
        }
        let mut dependents: BTreeMap<EntityRef, Vec<usize>> = BTreeMap::new();
        for (n, item) in items.iter().enumerate() {
            if let Some(t) = item.task {
                for b in &t.blocked_by {
                    dependents.entry(b.task).or_default().push(n);
                }
            }
        }
        Ok(Self {
            inputs,
            window,
            items,
            index,
            subjects,
            dependents,
            snap: i64::from(inputs.snap_s.max(1)),
            gap: i64::from(inputs.min_gap_s),
        })
    }

    pub(super) fn subject_index(&self, id: EntityRef) -> Result<usize, PlanIntentError> {
        self.index
            .get(&id)
            .copied()
            .ok_or(PlanIntentError::Unknown(id))
    }

    pub(super) fn day_index(&self, date: Date) -> Result<usize, PlanIntentError> {
        self.window
            .iter()
            .position(|d| d.date == date)
            .ok_or(PlanIntentError::DayOutsideWindow(date))
    }

    /// The planner date an instant falls on: its window day's, else its civil
    /// date in the reader's zone.
    pub(super) fn date_of(&self, s: i64) -> Date {
        self.window
            .iter()
            .find(|d| d.day.0.as_second() <= s && s < d.day.1.as_second())
            .map_or_else(
                || to_ts(s).to_zoned(self.inputs.zone.clone()).date(),
                |d| d.date,
            )
    }

    /// The window day an instant is in.
    pub(crate) fn day_at(&self, s: i64) -> Option<usize> {
        self.window
            .iter()
            .position(|d| d.day.0.as_second() <= s && s < d.day.1.as_second())
    }

    /// A slot `[s, e)` for item `i`, written in the kinds its times have.
    pub(super) fn slot_out(&self, i: usize, s: i64, e: i64) -> PlanPosition {
        let zone = &self.inputs.zone;
        let item = &self.items[i];
        let fresh = || match zone.iana_name() {
            Some(name) => SunriseTime::zoned(to_ts(s).to_zoned(zone.clone()).datetime(), name),
            None => SunriseTime::instant(to_ts(s)),
        };
        let start_kind = item.start_kind.clone().unwrap_or_else(fresh);
        let end_kind = item.end_kind.clone().unwrap_or_else(|| start_kind.clone());
        PlanPosition::Slot {
            start: retime(&start_kind, s, zone),
            end: retime(&end_kind, e, zone),
        }
    }

    /// The item's position before the diff, in its stored kinds.
    fn before_out(&self, i: usize) -> PlanPosition {
        let item = &self.items[i];
        match (&item.before, &item.start_kind) {
            (Pos::Slot(_, e), Some(start)) => PlanPosition::Slot {
                start: start.clone(),
                end: match (&item.end_kind, item.is_task) {
                    (Some(end), false) => end.clone(),
                    _ => retime(start, *e, &self.inputs.zone),
                },
            },
            (Pos::Slot(s, e), None) => PlanPosition::Slot {
                start: SunriseTime::instant(to_ts(*s)),
                end: SunriseTime::instant(to_ts(*e)),
            },
            (Pos::Day(d), _) => PlanPosition::Day(*d),
            (Pos::Unplanned, _) => PlanPosition::Unplanned,
        }
    }

    pub(super) fn report(
        &self,
        intent: &PlanIntent,
        after: &[Pos],
        out_after: &BTreeMap<usize, PlanPosition>,
        unscheduled: &BTreeMap<usize, UnscheduleReason>,
        invalid_subject: Option<usize>,
        truncated: bool,
    ) -> PlanDiff {
        let before: Vec<Pos> = self.items.iter().map(|i| i.before.clone()).collect();
        let was = rules::all_violations(self, &before, None);
        let now = rules::all_violations(self, after, invalid_subject);
        let was: BTreeSet<(EntityRef, PlanRule, Option<EntityRef>)> =
            was.into_iter().map(|v| (v.item, v.rule, v.with)).collect();
        let mut violations: Vec<PlanViolation> = now
            .into_iter()
            .map(|mut v| {
                v.introduced = !was.contains(&(v.item, v.rule, v.with));
                v
            })
            .collect();
        violations.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.item.cmp(&b.item))
                .then(a.rule.cmp(&b.rule))
                .then(a.with.cmp(&b.with))
        });
        let mut moves = Vec::new();
        for (&i, pos) in out_after {
            if unscheduled.contains_key(&i) || after[i] == self.items[i].before {
                continue;
            }
            let role = if self.items[i].class == Class::Subject {
                MoveRole::Subject
            } else {
                MoveRole::Rippled
            };
            moves.push(PlanMove {
                item: self.items[i].id,
                role,
                before: self.before_out(i),
                after: pos.clone(),
            });
        }
        moves.sort_by(|a, b| a.role.cmp(&b.role).then(a.item.cmp(&b.item)));
        let unscheduled: Vec<PlanUnscheduled> = unscheduled
            .iter()
            .filter_map(|(&i, &reason)| match after[i] {
                // Already date-only on that day: nothing changes.
                Pos::Day(_) if after[i] == self.items[i].before => None,
                Pos::Day(day) => Some(PlanUnscheduled {
                    item: self.items[i].id,
                    before: self.before_out(i),
                    day,
                    reason,
                }),
                _ => None,
            })
            .collect();
        // Only a Blocking violation the commit is answerable for refuses it:
        // one it introduces, or one standing on an item it writes. A hard
        // constraint already broken on an item the gesture leaves alone is
        // flagged, never a reason to refuse every gesture on its day.
        let written: BTreeSet<EntityRef> = moves
            .iter()
            .map(|m| m.item)
            .chain(unscheduled.iter().map(|u| u.item))
            .collect();
        let committable = !violations.iter().any(|v| {
            v.severity == PlanSeverity::Blocking && (v.introduced || written.contains(&v.item))
        });
        let base = plan_base(self.inputs);
        let mut diff = PlanDiff {
            id: PlanDiffId([0; 16]),
            base,
            intent: intent.clone(),
            moves,
            unscheduled,
            violations,
            committable,
            truncated,
        };
        diff.id = diff_id(&diff);
        diff
    }
}
