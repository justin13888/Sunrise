//! What a `HEAD`-only run of the same op set would hold, and why a replica
//! that disagrees with it does.
//!
//! The no-loss property compares each final projection with the projection a
//! `HEAD`-only run of the same ops produces. Running `HEAD` twice does not give
//! that reference: which of two concurrent writes wins is decided by hybrid
//! logical clocks a second run cannot reproduce. So the reference is computed
//! from the ops instead, under the rule the property states — every field a
//! writer set is present unless a later concurrent write to *that field* won:
//!
//! - writes are grouped into epochs, one per settle, because a settle is when
//!   every replica has seen every earlier write;
//! - a field's expected value is the last value it was given in the last epoch
//!   that wrote it, and when more than one writer wrote it in that epoch, any
//!   of their last values is acceptable.
//!
//! The control run (`HEAD` against `HEAD`) is what shows the reference is the
//! one `HEAD` produces: it may show no violation at all. Its devices write
//! `Patch`es of the fields they change (ADR-0065 §7), so two concurrent writes
//! to different fields both survive; #319, entity-level last-writer-wins, is
//! left only to a baseline, against which `HEAD` writes full-state ops.

use std::collections::{BTreeMap, BTreeSet};

use sunrise_id::EntityRef;

use super::future::FutureWrite;
use super::gaps::expected_gaps;
use super::projection::{Field, Projection};
use super::violation::{Classified, Violation, Who};

/// Who wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Writer {
    /// Replica A: the baseline, or `HEAD` in the control run.
    A,
    /// Replica B: always `HEAD`.
    B,
    /// The newer-build writer.
    Future,
}

#[derive(Debug, Clone)]
struct Write {
    task: EntityRef,
    field: Field,
    value: Option<String>,
    writer: Writer,
    epoch: u32,
    at: u64,
}

/// A newer op the baseline did not keep: refused, or accepted with its value
/// replaced.
#[derive(Debug, Clone)]
struct Unkept {
    task: EntityRef,
    write: FutureWrite,
    /// The issue that accounts for it: see [`cause`].
    cause: Option<u32>,
    at: u64,
}

/// The issue that accounts for a baseline not keeping a newer op that set
/// `write` and carried `carried`: the write's own, when the baseline cannot
/// read what it sets, else the first carried value the baseline cannot read.
fn cause(write: FutureWrite, carried: &[FutureWrite]) -> Option<u32> {
    write
        .baseline_issue()
        .or_else(|| carried.iter().find_map(|c| c.baseline_issue()))
}

#[derive(Debug, Clone, Copy)]
struct Taint {
    task: EntityRef,
    write: FutureWrite,
    at: u64,
}

/// Everything the scenario did, in the order it did it.
#[derive(Debug, Default)]
pub struct Model {
    writes: Vec<Write>,
    /// `(task, epoch) -> writers` of any op on that task in that epoch.
    touched: BTreeMap<(EntityRef, u32), BTreeSet<Writer>>,
    unkept: Vec<Unkept>,
    taints: Vec<Taint>,
    /// Whether a `HEAD` core wrote a `StreamDigest` or a `DeviceFeatures`
    /// during the run.
    unparkable_kind_published: bool,
    epoch: u32,
    clock: u64,
    tasks: Vec<EntityRef>,
}

impl Model {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Every settle opens a new epoch.
    pub fn settle(&mut self) {
        self.epoch += 1;
    }

    /// Every task the scenario created, in creation order.
    #[must_use]
    pub fn tasks(&self) -> &[EntityRef] {
        &self.tasks
    }

    /// Record one op by `writer` on `task` writing `fields`.
    pub fn record(&mut self, writer: Writer, task: EntityRef, fields: &[(Field, Option<String>)]) {
        let at = self.tick();
        if !self.tasks.contains(&task) {
            self.tasks.push(task);
        }
        self.touched
            .entry((task, self.epoch))
            .or_default()
            .insert(writer);
        for (field, value) in fields {
            self.writes.push(Write {
                task,
                field: *field,
                value: value.clone(),
                writer,
                epoch: self.epoch,
                at,
            });
        }
    }

    /// Record the baseline not keeping a newer op on `task`: refusing it, or
    /// accepting it and holding something other than what it set.
    pub fn unkept(&mut self, task: EntityRef, write: FutureWrite, carried: &[FutureWrite]) {
        let at = self.tick();
        self.unkept.push(Unkept {
            task,
            write,
            cause: cause(write, carried),
            at,
        });
    }

    /// The newer values a newer op on `task` carries now: every one B has
    /// accepted on it that the baseline cannot read.
    #[must_use]
    pub fn carried(&self, task: EntityRef) -> Vec<FutureWrite> {
        self.taints
            .iter()
            .filter(|t| t.task == task)
            .map(|t| t.write)
            .collect()
    }

    /// Record B accepting a newer op whose value the baseline cannot read, so
    /// every later B write of `task` carries it.
    pub fn tainted(&mut self, task: EntityRef, write: FutureWrite) {
        if matches!(write, FutureWrite::EnumValue | FutureWrite::NestedKind) {
            let at = self.tick();
            self.taints.push(Taint { task, write, at });
        }
    }

    /// Record that a `HEAD` core wrote an op of a kind of `HEAD`'s own — a
    /// `StreamDigest` or a `DeviceFeatures` — which a baseline that predates
    /// parking classes as corruption when the relay delivers it.
    pub fn published_unparkable_kind(&mut self) {
        self.unparkable_kind_published = true;
    }

    /// Whether a baseline A can be expected to agree with B about `task` at a
    /// settle. A task a newer op reached that A could not read is allowed to
    /// differ; the final check says whether that difference is a known gap.
    #[must_use]
    pub fn comparable(&self, task: EntityRef) -> bool {
        !self.unkept.iter().any(|u| u.task == task) && !self.taints.iter().any(|t| t.task == task)
    }

    /// The acceptable values of `field` on `task`, and the epoch they were
    /// written in.
    fn expected(&self, task: EntityRef, field: Field) -> Option<(u32, Vec<&Write>)> {
        let ws: Vec<&Write> = self
            .writes
            .iter()
            .filter(|w| w.task == task && w.field == field)
            .collect();
        let last_epoch = ws.iter().map(|w| w.epoch).max()?;
        let mut last_by_writer: BTreeMap<Writer, &Write> = BTreeMap::new();
        for w in ws.into_iter().filter(|w| w.epoch == last_epoch) {
            last_by_writer.insert(w.writer, w);
        }
        Some((last_epoch, last_by_writer.into_values().collect()))
    }

    /// Compare one replica's final state with the reference.
    #[must_use]
    pub fn check(
        &self,
        who: Who,
        finals: &BTreeMap<EntityRef, Option<Projection>>,
    ) -> Vec<Violation> {
        let mut out = Vec::new();
        for task in &self.tasks {
            let Some(Some(p)) = finals.get(task) else {
                out.push(Violation::Missing { who, task: *task });
                continue;
            };
            for field in Field::ALL {
                let Some((_, candidates)) = self.expected(*task, field) else {
                    continue;
                };
                let got = p.get(&field).cloned().flatten();
                if candidates.iter().any(|w| w.value == got) {
                    continue;
                }
                out.push(Violation::Lost {
                    who,
                    task: *task,
                    field,
                    expected: candidates.iter().map(|w| w.value.clone()).collect(),
                    got,
                });
            }
        }
        out
    }

    /// Attribute each violation to the known gap that explains it, if any, for
    /// a run against `baseline` (`None` for `HEAD` against `HEAD`).
    #[must_use]
    pub fn classify(&self, baseline: Option<&str>, violations: Vec<Violation>) -> Vec<Classified> {
        let expected: Vec<u32> = expected_gaps(baseline).iter().map(|g| g.issue).collect();
        violations
            .into_iter()
            .map(|violation| {
                let issue = self
                    .explain(baseline.is_some(), &violation)
                    .filter(|i| expected.contains(i));
                Classified { violation, issue }
            })
            .collect()
    }

    fn explain(&self, cross: bool, v: &Violation) -> Option<u32> {
        match v {
            Violation::Refused {
                who: Who::A,
                write,
                carried,
                corruption: true,
                ..
            } if cross => cause(*write, carried),
            // The baseline's sync driver dropping a `HEAD` write that carries a
            // newer value `HEAD` kept: the same unknown value, one hop later.
            // Or an op kind of `HEAD`'s own the baseline cannot read, which
            // reaches its sync driver exactly as `FutureWrite::OpKind` would:
            // a `StreamDigest` (ADR-0043 §5), published on the anti-entropy
            // tick, or a `DeviceFeatures` (ADR-0045 §7), which every `HEAD`
            // core writes when it opens the vault. The second is in every run,
            // the first only in a run long enough to reach the tick.
            Violation::LoggedCorruption { who: Who::A, .. } if cross => self
                .taints
                .first()
                .and_then(|t| t.write.baseline_issue())
                .or_else(|| {
                    self.unparkable_kind_published
                        .then_some(FutureWrite::OpKind)
                        .and_then(FutureWrite::baseline_issue)
                }),
            Violation::Lost {
                who, task, field, ..
            } => self.explain_loss(cross, *who, *task, *field),
            _ => None,
        }
    }

    fn explain_loss(&self, cross: bool, who: Who, task: EntityRef, field: Field) -> Option<u32> {
        let (epoch, candidates) = self.expected(task, field)?;
        let writers = self.touched.get(&(task, epoch));
        if writers.is_some_and(|w| w.len() >= 2) {
            return Some(319);
        }
        if !cross {
            return None;
        }
        let unkept: Vec<&Unkept> = self.unkept.iter().filter(|u| u.task == task).collect();
        // A after its upgrade holds what the baseline made of the newer op:
        // nothing, or a fallback in its place.
        if who == Who::AUpgraded {
            if let Some(u) = unkept.iter().find(|u| u.write.field() == Some(field)) {
                return u.cause;
            }
        }
        // The baseline wrote the task after failing to keep a newer op on it,
        // and its write, made without the newer value, replaced it everywhere.
        let baseline_wrote_after = |at: u64| {
            self.writes
                .iter()
                .any(|w| w.task == task && w.writer == Writer::A && w.at > at)
        };
        if unkept.iter().any(|u| baseline_wrote_after(u.at)) {
            return Some(324);
        }
        // A `HEAD` write that carried a newer value was dropped by the
        // baseline, so whatever else that write set never reached A either.
        if matches!(who, Who::A | Who::AUpgraded) {
            if let Some(t) = self.taints.iter().find(|t| {
                t.task == task
                    && candidates
                        .iter()
                        .any(|w| w.writer == Writer::B && w.at > t.at)
            }) {
                return t.write.baseline_issue();
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_version::gaps::FLOOR;

    fn corruption_at_a() -> Vec<Violation> {
        vec![Violation::LoggedCorruption {
            who: Who::A,
            message: "scheduling a resync".into(),
        }]
    }

    fn issue(model: &Model, baseline: Option<&str>) -> Option<u32> {
        model.classify(baseline, corruption_at_a())[0].issue
    }

    /// A baseline that predates parking logs `HEAD`'s digest or feature
    /// advertisement as corruption, which is #320, but only once `HEAD` has
    /// written one.
    #[test]
    fn an_unparkable_head_kind_explains_the_floors_corruption_log_as_320() {
        let mut model = Model::default();
        assert_eq!(
            issue(&model, Some(FLOOR)),
            None,
            "no such op, no explanation"
        );
        model.published_unparkable_kind();
        assert_eq!(issue(&model, Some(FLOOR)), Some(320));
    }

    /// A baseline cut after the parking fix parks both kinds, so a corruption
    /// log there is not #320 however many of them `HEAD` wrote.
    #[test]
    fn an_unparkable_head_kind_explains_nothing_against_a_parking_baseline() {
        let mut model = Model::default();
        model.published_unparkable_kind();
        let after_parking = "0000000000000000000000000000000000000000";
        assert_eq!(issue(&model, Some(after_parking)), None);
    }

    /// `HEAD` against `HEAD` parks nothing of its own, so a corruption log in
    /// the control run stays unexplained.
    #[test]
    fn an_unparkable_head_kind_explains_nothing_in_the_control_run() {
        let mut model = Model::default();
        model.published_unparkable_kind();
        assert_eq!(issue(&model, None), None);
    }

    /// The nightly `Cross-version merge (ADR-0042 floor)` failure, seed
    /// `9a11c123` in `proptest-regressions/tests/cross_version_convergence.txt`,
    /// without the baseline driver.
    ///
    /// The floor refuses the newer op kind handed to it, and its sync driver
    /// logs three inbound ops as corruption: the `DeviceFeatures` that B and
    /// C wrote when `HEAD` opened them, delivered by the relay and delivered
    /// again by the resync each corruption log schedules. The
    /// run was too short for any `HEAD` core to reach its anti-entropy tick,
    /// so no `StreamDigest` was published, and the classifier, which only
    /// knew the digest, left the three logs unexplained. The `HEAD` log of
    /// that run holds the advertisements and nothing else of note.
    #[test]
    fn the_floors_corruption_logs_of_heads_feature_advertisements_are_320() {
        use crate::cross_version::events::LoggedEvent;

        let head_log = [LoggedEvent {
            level: "DEBUG".into(),
            ev: Some("core.feature.advertised".into()),
            cause: None,
            message: "advertised the features this build supports".into(),
        }];
        let mut model = Model::default();
        if head_log
            .iter()
            .any(LoggedEvent::is_unparkable_kind_published)
        {
            model.published_unparkable_kind();
        }
        let task = EntityRef::new(sunrise_id::EntityKind::Task, [7; 16]);
        let mut violations = vec![Violation::Refused {
            who: Who::A,
            task,
            write: FutureWrite::OpKind,
            carried: Vec::new(),
            corruption: true,
            error: "remote op invalid: inner op: unknown variant `X326Annotate`".into(),
        }];
        violations.extend(std::iter::repeat_n(corruption_at_a(), 3).flatten());

        let classified = model.classify(Some(FLOOR), violations);
        assert!(
            classified.iter().all(|c| c.issue == Some(320)),
            "{classified:?}"
        );
    }
}
