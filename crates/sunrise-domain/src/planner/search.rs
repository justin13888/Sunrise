//! The bounded search (`docs/08-features/planner.md` §Solver, steps 2–6).

use super::rules::{blocks_at, blocks_on_day, rules_at, Broken, PlanSeverity};
use super::{Class, Pos, UnscheduleReason, World, UNSCHEDULE_DISPLACEMENT_S};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// How many placements of one item the search branches over, besides
/// unscheduling it.
const BRANCH: usize = 4;

/// What the search decided for one rippled item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    /// A new slot in its day.
    Slot(i64, i64),
    /// The no-time lane of its day.
    Unschedule(UnscheduleReason),
    /// Where it is: the last resort of an item with no other position that
    /// an ordinary submit would accept.
    Stay,
}

/// The best assignment found.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) assign: Vec<(usize, Choice)>,
    pub(crate) truncated: bool,
}

/// The objective's first four terms (ADR-0048 §4): hard violations
/// introduced, items moved, soft violations introduced, displacement.
type Score = (u32, u32, u32, i64);

/// A pending item, in the order the search takes them: earliest current
/// start, then id.
type Pending = BTreeSet<(i64, EntityRef, usize)>;

/// The best complete assignment so far: its score, its moved ids in order
/// (the objective's last term), and the choices.
type Best = (Score, Vec<EntityRef>, Vec<(usize, Choice)>);

struct Cand {
    choice: Choice,
    key: (u32, u32, u32, i64, usize),
}

struct Search<'w, 'a> {
    world: &'w World<'a>,
    cur: Vec<Pos>,
    assigned: Vec<bool>,
    /// Each item's broken rules at its position before the diff.
    was: Vec<Vec<Broken>>,
    /// Per window day, the fixed and subject intervals, sorted.
    fixed: Vec<Vec<(i64, i64)>>,
    path: Vec<(usize, Choice)>,
    best: Option<Best>,
    evals: u32,
    truncated: bool,
}

/// Move the movable items the subjects now collide with, or now break a
/// dependency of, out of the way. `after` holds the subjects' new positions
/// and every other item's old one.
pub(crate) fn ripple(world: &World<'_>, after: &[Pos]) -> Found {
    let n = world.items.len();
    let before: Vec<Pos> = world.items.iter().map(|i| i.before.clone()).collect();
    let was = (0..n)
        .map(|i| match before[i].slot() {
            Some((s, e)) if world.items[i].class == Class::Movable => {
                rules_at(world, i, s, e, &before)
            }
            _ => Vec::new(),
        })
        .collect();
    let mut fixed = vec![Vec::new(); world.window.len()];
    for (i, item) in world.items.iter().enumerate() {
        if !matches!(item.class, Class::Fixed | Class::Subject) {
            continue;
        }
        let Some((s, e)) = after[i].slot() else {
            continue;
        };
        for (d, day) in world.window.iter().enumerate() {
            if s < day.day.1.as_second() && e > day.day.0.as_second() {
                fixed[d].push((s, e));
            }
        }
    }
    for f in &mut fixed {
        f.sort_unstable();
    }
    let mut search = Search {
        world,
        cur: after.to_vec(),
        assigned: vec![false; n],
        was,
        fixed,
        path: Vec::new(),
        best: None,
        evals: 0,
        truncated: false,
    };
    let mut pending = Pending::new();
    for &subj in &world.subjects {
        let Some((s, e)) = after[subj].slot() else {
            continue;
        };
        for (j, item) in world.items.iter().enumerate() {
            if item.class != Class::Movable {
                continue;
            }
            if let Some((s2, e2)) = item.before.slot() {
                if s2 < e + world.gap && s < e2 + world.gap {
                    pending.insert((s2, item.id, j));
                }
            }
        }
        search.push_dependents(subj, e, &mut pending);
    }
    search.dfs(&pending, (0, 0, 0, 0));
    let assign = search.best.map(|(.., a)| a).unwrap_or_default();
    Found {
        assign,
        truncated: search.truncated,
    }
}

impl Search<'_, '_> {
    fn dfs(&mut self, pending: &Pending, score: Score) {
        if let Some((best, ..)) = &self.best {
            if score > *best {
                return;
            }
        }
        let Some(&(_, _, i)) = pending.iter().next() else {
            let mut moved: Vec<EntityRef> = self
                .path
                .iter()
                .filter(|(j, c)| match c {
                    Choice::Slot(s, e) => self.world.items[*j].before != Pos::Slot(*s, *e),
                    Choice::Unschedule(_) => true,
                    Choice::Stay => false,
                })
                .map(|(j, _)| self.world.items[*j].id)
                .collect();
            moved.sort_unstable();
            let better = match &self.best {
                None => true,
                Some((b, bm, _)) => (score, &moved) < (*b, bm),
            };
            if better {
                self.best = Some((score, moved, self.path.clone()));
            }
            return;
        };
        self.evals = self.evals.saturating_add(1);
        if self.evals > self.world.inputs.search_budget {
            self.truncated = true;
        }
        let cands = self.candidates(i);
        for (k, cand) in cands.into_iter().enumerate() {
            if k > 0 && (self.truncated && self.best.is_some()) {
                break;
            }
            let mut next = pending.clone();
            next.retain(|&(_, _, j)| j != i);
            let (pos, moved, disp) = match cand.choice {
                Choice::Slot(s, e) => {
                    let (b0, _) = self.world.items[i].before.slot().unwrap_or((s, e));
                    let moved = u32::from(self.world.items[i].before != Pos::Slot(s, e));
                    (Pos::Slot(s, e), moved, (s - b0).abs())
                }
                Choice::Unschedule(_) => {
                    let d = self.world.items[i].day.unwrap_or(0);
                    (
                        Pos::Day(self.world.window[d].date),
                        1,
                        UNSCHEDULE_DISPLACEMENT_S,
                    )
                }
                Choice::Stay => (self.cur[i].clone(), 0, 0),
            };
            let old = std::mem::replace(&mut self.cur[i], pos);
            self.assigned[i] = true;
            let mut hard = cand.key.0;
            if let Choice::Slot(s, e) = cand.choice {
                for (j, item) in self.world.items.iter().enumerate() {
                    if item.class != Class::Movable || self.assigned[j] {
                        continue;
                    }
                    if let Some((s2, e2)) = self.cur[j].slot() {
                        if s2 < e + self.world.gap && s < e2 + self.world.gap {
                            next.insert((s2, item.id, j));
                        }
                    }
                }
                hard += self.push_dependents(i, e, &mut next);
            }
            let score2 = (
                score.0 + hard,
                score.1 + moved,
                score.2 + cand.key.2,
                score.3 + disp,
            );
            self.path.push((i, cand.choice));
            self.dfs(&next, score2);
            self.path.pop();
            self.assigned[i] = false;
            self.cur[i] = old;
        }
    }

    /// Queue the movable dependents of item `i` that now start before its
    /// end `e`, and count the ones that cannot move and newly break the
    /// dependency.
    fn push_dependents(&self, i: usize, e: i64, pending: &mut Pending) -> u32 {
        let item = &self.world.items[i];
        let Some(deps) = self.world.dependents.get(&item.id) else {
            return 0;
        };
        let mut hard = 0;
        for &d in deps {
            let Some((ds, _)) = self.cur[d].slot() else {
                continue;
            };
            if ds >= e {
                continue;
            }
            let dep = &self.world.items[d];
            if dep.class == Class::Movable && !self.assigned[d] {
                pending.insert((ds, dep.id, d));
                continue;
            }
            let was_broken = match (dep.before.slot(), item.before.slot()) {
                (Some((bs, _)), Some((_, be))) => bs < be,
                _ => false,
            };
            if !was_broken && dep.class != Class::Off {
                hard += 1;
            }
        }
        hard
    }

    /// The best few slots for item `i` in its day, nearest its old start
    /// first, and unscheduling it.
    fn candidates(&self, i: usize) -> Vec<Cand> {
        let world = self.world;
        let item = &world.items[i];
        let unschedule = |reason| Cand {
            choice: Choice::Unschedule(reason),
            key: (0, 0, 0, UNSCHEDULE_DISPLACEMENT_S, usize::MAX),
        };
        let (Some(d), Some((s0, _))) = (item.day, item.before.slot()) else {
            return vec![unschedule(UnscheduleReason::NoRoomInDay)];
        };
        let snap = world.snap;
        let gap = world.gap;
        let dur = item.dur;
        let (lo, hi) = world.window[d].plannable;
        let lo = ceil_snap(lo.as_second().max(world.inputs.now.as_second()), snap);
        let hi = hi.as_second() - dur;
        let mut busy: Vec<(i64, i64)> = self.fixed[d].clone();
        let (day_lo, day_hi) = world.window[d].day;
        for j in 0..world.items.len() {
            if j == i || !self.assigned[j] {
                continue;
            }
            if let Some((s, e)) = self.cur[j].slot() {
                if s < day_hi.as_second() && e > day_lo.as_second() {
                    busy.push((s, e));
                }
            }
        }
        busy.sort_unstable();
        let mut soft_busy: Vec<(i64, i64)> = Vec::new();
        for (j, other) in world.items.iter().enumerate() {
            if j == i || self.assigned[j] || other.class != Class::Movable {
                continue;
            }
            if let Some(slot) = self.cur[j].slot() {
                soft_busy.push(slot);
            }
        }
        let has_rules = item.task.is_some_and(|t| {
            t.hard_due.is_some()
                || t.target_due.is_some()
                || !t.blocked_by.is_empty()
                || !t.constraints.is_empty()
        });
        let was = &self.was[i];
        let mut out: Vec<Cand> = Vec::new();
        let mut clean = 0;
        let mut hit_hard = false;
        if lo <= hi {
            let anchor = floor_snap(s0.clamp(lo, hi), snap).max(lo);
            let mut seq = 0usize;
            let mut step = 0i64;
            loop {
                let after = anchor + step * snap;
                let before = anchor - step * snap;
                let mut any = false;
                for (side, c) in [after, before].into_iter().enumerate() {
                    if step == 0 && side == 1 {
                        continue;
                    }
                    if c < lo || c > hi {
                        continue;
                    }
                    any = true;
                    seq += 1;
                    let (s, e) = (c, c + dur);
                    if overlaps_any(&busy, s - gap, e + gap) {
                        continue;
                    }
                    if blocks_at(world, i, s) {
                        hit_hard = true;
                        continue;
                    }
                    let (mut hard, mut soft) = (0u32, 0u32);
                    if has_rules {
                        for b in rules_at(world, i, s, e, &self.cur) {
                            if was.contains(&b) {
                                continue;
                            }
                            match b.1 {
                                PlanSeverity::Hard | PlanSeverity::Blocking => hard += 1,
                                PlanSeverity::Soft => soft += 1,
                                PlanSeverity::Info => {}
                            }
                        }
                    }
                    if hard > 0 {
                        hit_hard = true;
                    }
                    let pushes = u32::try_from(
                        soft_busy
                            .iter()
                            .filter(|&&(s2, e2)| s2 < e + gap && s < e2 + gap)
                            .count(),
                    )
                    .unwrap_or(u32::MAX);
                    if hard == 0 && pushes == 0 && soft == 0 {
                        clean += 1;
                    }
                    out.push(Cand {
                        choice: Choice::Slot(s, e),
                        key: (hard, pushes, soft, (s - s0).abs(), seq),
                    });
                }
                if !any || clean >= BRANCH {
                    break;
                }
                step += 1;
            }
        }
        let reason = if hit_hard {
            UnscheduleReason::WouldViolateHard
        } else {
            UnscheduleReason::NoRoomInDay
        };
        out.sort_by_key(|c| c.key);
        out.truncate(BRANCH);
        if !blocks_on_day(world, i, world.window[d].date) {
            let un = unschedule(reason);
            let at = out.partition_point(|c| c.key < un.key);
            out.insert(at, un);
        } else if out.is_empty() {
            out.push(Cand {
                choice: Choice::Stay,
                key: (0, 0, 1, 0, usize::MAX),
            });
        }
        out
    }
}

/// Place each of `order` in the first free slot of window day `d`, in order,
/// at or after `not_before` and now. `None` for a task with no room.
pub(crate) fn auto_place(
    world: &World<'_>,
    order: &[usize],
    d: usize,
    not_before: Option<i64>,
) -> Vec<(usize, Option<(i64, i64)>)> {
    let day = &world.window[d];
    let (day_lo, day_hi) = (day.day.0.as_second(), day.day.1.as_second());
    let listed: BTreeSet<usize> = order.iter().copied().collect();
    let mut busy: Vec<(i64, i64)> = world
        .items
        .iter()
        .enumerate()
        .filter(|(j, item)| !listed.contains(j) && item.class != Class::Off)
        .filter_map(|(_, item)| item.before.slot())
        .filter(|&(s, e)| s < day_hi && e > day_lo)
        .collect();
    busy.sort_unstable();
    let lo = day
        .plannable
        .0
        .as_second()
        .max(world.inputs.now.as_second())
        .max(not_before.unwrap_or(i64::MIN));
    let lo = ceil_snap(lo, world.snap);
    let hi = day.plannable.1.as_second();
    let mut out = Vec::with_capacity(order.len());
    for &i in order {
        let dur = world.items[i].dur;
        let mut c = lo;
        let mut found = None;
        while c + dur <= hi {
            let (s, e) = (c, c + dur);
            if !overlaps_any(&busy, s - world.gap, e + world.gap) && !blocks_at(world, i, s) {
                found = Some((s, e));
                break;
            }
            c += world.snap;
        }
        if let Some(slot) = found {
            let at = busy.partition_point(|b| *b < slot);
            busy.insert(at, slot);
        }
        out.push((i, found));
    }
    out
}

/// Whether `[s, e)` overlaps any interval of `sorted` (sorted by start).
fn overlaps_any(sorted: &[(i64, i64)], s: i64, e: i64) -> bool {
    // Only intervals starting before `e` can overlap; scan those back from
    // the last, which is cheap because intervals in a day are few and short.
    let upto = sorted.partition_point(|&(bs, _)| bs < e);
    sorted[..upto].iter().any(|&(_, be)| be > s)
}

fn ceil_snap(x: i64, snap: i64) -> i64 {
    x.div_euclid(snap) * snap + if x.rem_euclid(snap) == 0 { 0 } else { snap }
}

fn floor_snap(x: i64, snap: i64) -> i64 {
    x.div_euclid(snap) * snap
}
