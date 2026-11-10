//! The planner's canonical encoding: the digest of a solve's inputs and the
//! id of the diff it returns.

use super::{
    Edge, PlanBase, PlanBlock, PlanBlocker, PlanDay, PlanDiff, PlanDiffId, PlanEvent, PlanInputs,
    PlanIntent, PlanPosition, PlanTask, SlotTarget, UnscheduleKeep,
};
use crate::time::SunriseTime;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_id::EntityRef;

/// The digest of every input a solve reads, independent of input order.
#[must_use]
pub fn plan_base(inputs: &PlanInputs) -> PlanBase {
    let mut f = Feed::new(b"sunrise.planner.base.v1");
    let snap = i64::from(inputs.snap_s.max(1));
    f.i64(inputs.now.as_second().div_euclid(snap));
    f.zone(&inputs.zone, inputs.now);
    let mut days: Vec<&PlanDay> = inputs.window.iter().collect();
    days.sort_by_key(|d| d.date);
    f.u64(days.len() as u64);
    for d in days {
        f.str(&d.date.to_string());
        f.ts(d.day.0);
        f.ts(d.day.1);
        f.ts(d.plannable.0);
        f.ts(d.plannable.1);
    }
    let mut tasks: Vec<&PlanTask> = inputs.tasks.iter().collect();
    tasks.sort_by_key(|t| t.id);
    f.u64(tasks.len() as u64);
    for t in tasks {
        f.id(t.id);
        f.opt_time(t.planned_at.as_ref());
        f.u64(t.estimate_s.unwrap_or(u64::MAX));
        f.opt_ts(t.target_due);
        f.opt_ts(t.hard_due);
        let mut blockers: Vec<&PlanBlocker> = t.blocked_by.iter().collect();
        blockers.sort_by_key(|b| b.task);
        f.u64(blockers.len() as u64);
        for b in blockers {
            f.id(b.task);
            f.opt_ts(b.end);
        }
        f.u64(t.constraints.len() as u64);
        for c in &t.constraints {
            // Its wire encoding, which every build writes the same way.
            let mut bytes = Vec::new();
            if ciborium::into_writer(c, &mut bytes).is_err() {
                bytes.clear();
            }
            f.u64(bytes.len() as u64);
            f.0.update(&bytes);
        }
        f.u8(u8::from(t.in_progress) | (u8::from(t.bound) << 1));
    }
    let mut blocks: Vec<&PlanBlock> = inputs.blocks.iter().collect();
    blocks.sort_by_key(|b| b.id);
    f.u64(blocks.len() as u64);
    for b in blocks {
        f.id(b.id);
        f.time(&b.starts_at);
        f.time(&b.ends_at);
        f.u8(u8::from(b.fixed));
    }
    let mut events: Vec<&PlanEvent> = inputs.events.iter().collect();
    events.sort_by_key(|e| e.id);
    f.u64(events.len() as u64);
    for e in events {
        f.id(e.id);
        f.ts(e.start);
        f.ts(e.end);
        f.u8(u8::from(e.busy));
    }
    f.u64(u64::from(inputs.snap_s));
    f.u64(u64::from(inputs.default_task_duration_s));
    f.u64(u64::from(inputs.min_gap_s));
    f.u64(u64::from(inputs.search_budget));
    PlanBase(*f.0.finalize().as_bytes())
}

/// A BLAKE3 feed of length-prefixed, fixed-order fields.
struct Feed(blake3::Hasher);

impl Feed {
    fn new(domain: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(domain);
        Self(h)
    }
    fn u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }
    fn u64(&mut self, v: u64) {
        self.0.update(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.update(&v.to_be_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        self.0.update(s.as_bytes());
    }
    fn id(&mut self, id: EntityRef) {
        self.str(id.kind().prefix());
        self.0.update(id.bytes());
    }
    fn ts(&mut self, t: Timestamp) {
        self.i64(t.as_second());
        self.i64(i64::from(t.subsec_nanosecond()));
    }
    fn opt_ts(&mut self, t: Option<Timestamp>) {
        match t {
            Some(t) => {
                self.u8(1);
                self.ts(t);
            }
            None => self.u8(0),
        }
    }
    fn time(&mut self, t: &SunriseTime) {
        let (ms, kind, tz) = t.to_parts();
        self.i64(ms);
        self.str(kind);
        self.str(tz.as_deref().unwrap_or(""));
        self.str(t.civil_part().as_deref().unwrap_or(""));
    }
    fn opt_time(&mut self, t: Option<&SunriseTime>) {
        match t {
            Some(t) => {
                self.u8(1);
                self.time(t);
            }
            None => self.u8(0),
        }
    }
    fn zone(&mut self, zone: &TimeZone, now: Timestamp) {
        if let Some(name) = zone.iana_name() {
            self.str(name);
        } else {
            self.str("");
            self.i64(i64::from(zone.to_offset(now).seconds()));
        }
    }
    fn position(&mut self, p: &PlanPosition) {
        match p {
            PlanPosition::Slot { start, end } => {
                self.u8(0);
                self.time(start);
                self.time(end);
            }
            PlanPosition::Day(d) => {
                self.u8(1);
                self.str(&d.to_string());
            }
            PlanPosition::Unplanned => self.u8(2),
        }
    }
    fn intent(&mut self, intent: &PlanIntent) {
        let ids = |f: &mut Self, list: &[EntityRef]| {
            f.u64(list.len() as u64);
            for id in list {
                f.id(*id);
            }
        };
        match intent {
            PlanIntent::MoveTask { task, to } => {
                self.u8(0);
                self.id(*task);
                match to {
                    SlotTarget::At(t) => {
                        self.u8(0);
                        self.time(t);
                    }
                    SlotTarget::Day(d) => {
                        self.u8(1);
                        self.str(&d.to_string());
                    }
                }
            }
            PlanIntent::MoveBlock { block, to_start } => {
                self.u8(1);
                self.id(*block);
                self.time(to_start);
            }
            PlanIntent::ResizeBlock { block, edge, to } => {
                self.u8(2);
                self.id(*block);
                self.u8(u8::from(*edge == Edge::End));
                self.time(to);
            }
            PlanIntent::BulkDefer { tasks, to, place } => {
                self.u8(3);
                ids(self, tasks);
                self.str(&to.to_string());
                self.u8(u8::from(*place));
            }
            PlanIntent::Unschedule { items, keep } => {
                self.u8(4);
                ids(self, items);
                self.u8(u8::from(*keep == UnscheduleKeep::Nothing));
            }
            PlanIntent::AutoPlace {
                tasks,
                day,
                not_before,
            } => {
                self.u8(5);
                ids(self, tasks);
                self.str(&day.to_string());
                self.opt_time(not_before.as_ref());
            }
        }
    }
}

/// The diff's id: BLAKE3 over (base, intent, moves, unscheduled,
/// violations), truncated to 16 bytes.
pub(super) fn diff_id(diff: &PlanDiff) -> PlanDiffId {
    let mut f = Feed::new(b"sunrise.planner.diff.v1");
    f.0.update(&diff.base.0);
    f.intent(&diff.intent);
    f.u64(diff.moves.len() as u64);
    for m in &diff.moves {
        f.id(m.item);
        f.u8(m.role as u8);
        f.position(&m.before);
        f.position(&m.after);
    }
    f.u64(diff.unscheduled.len() as u64);
    for u in &diff.unscheduled {
        f.id(u.item);
        f.position(&u.before);
        f.str(&u.day.to_string());
        f.u8(u.reason as u8);
    }
    f.u64(diff.violations.len() as u64);
    for v in &diff.violations {
        f.id(v.item);
        f.str(v.rule.code());
        f.u8(v.severity as u8);
        f.u8(u8::from(v.introduced));
        match v.with {
            Some(w) => {
                f.u8(1);
                f.id(w);
            }
            None => f.u8(0),
        }
    }
    f.u8(u8::from(diff.truncated));
    let bytes = f.0.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&bytes.as_bytes()[..16]);
    PlanDiffId(id)
}
