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
//! one `HEAD` produces: the only violations it may show are #319's, which is
//! `HEAD`'s own entity-level last-writer-wins.

use std::collections::{BTreeMap, BTreeSet};

use sunrise_domain::Task;
use sunrise_id::EntityRef;

use super::future::{FutureWrite, FUTURE_FIELD};
use super::gaps::expected_gaps;

/// A field the harness writes and reads back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    /// `title`.
    Title,
    /// `priority`.
    Priority,
    /// `deleted`.
    Deleted,
    /// `state`, as its raw spelling.
    State,
    /// The kind of `due_at`, when set.
    DueKind,
    /// The newer build's top-level field.
    Note,
}

impl Field {
    /// Every field, in projection order.
    pub const ALL: [Self; 6] = [
        Self::Title,
        Self::Priority,
        Self::Deleted,
        Self::State,
        Self::DueKind,
        Self::Note,
    ];
}

impl FutureWrite {
    /// The field this newer write sets, if it sets one this build can read.
    #[must_use]
    pub const fn field(self) -> Option<Field> {
        match self {
            Self::Field => Some(Field::Note),
            Self::EnumValue => Some(Field::State),
            Self::NestedKind => Some(Field::DueKind),
            Self::OpKind => None,
        }
    }

    /// The issue that tracks a baseline dropping this write.
    #[must_use]
    pub const fn baseline_issue(self) -> Option<u32> {
        match self {
            Self::Field => None,
            Self::EnumValue => Some(321),
            Self::NestedKind => Some(322),
            Self::OpKind => Some(320),
        }
    }
}

/// One task as a replica holds it, field by field.
pub type Projection = BTreeMap<Field, Option<String>>;

/// Read the fields the harness tracks out of a `HEAD` `Task`.
#[must_use]
pub fn project(t: &Task) -> Projection {
    let mut p = Projection::new();
    p.insert(Field::Title, Some(t.title.clone()));
    p.insert(Field::Priority, t.priority.map(|n| n.to_string()));
    p.insert(Field::Deleted, Some(t.deleted.to_string()));
    p.insert(Field::State, Some(t.state.as_str().to_owned()));
    p.insert(
        Field::DueKind,
        t.due_at.as_ref().map(|d| d.kind_str().to_owned()),
    );
    p.insert(
        Field::Note,
        t.unknown
            .get(FUTURE_FIELD)
            .and_then(|v| v.get().as_text().map(str::to_owned)),
    );
    p
}

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

/// Which replica a violation was seen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Who {
    /// A before its upgrade.
    A,
    /// A after it was reopened by `HEAD`.
    AUpgraded,
    /// B. For a logged corruption, any `HEAD` core: they share one process
    /// and one log.
    B,
}

/// One way a run broke the invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `HEAD` would not open a vault the baseline wrote.
    UpgradeRefused {
        /// Which vault: B at setup, or A at the end.
        who: Who,
        /// The open's refusal.
        error: String,
    },
    /// A replica refused a newer op handed to it directly.
    Refused {
        /// Where.
        who: Who,
        /// The task the op was about.
        task: EntityRef,
        /// What it set.
        write: FutureWrite,
        /// Newer values it carried from earlier newer writes to the task.
        /// Each op is a whole task as `HEAD` held it, so it carries every
        /// unknown value `HEAD` kept, and any of them can be what the replica
        /// could not read.
        carried: Vec<FutureWrite>,
        /// Whether the refusal is the one the sync driver counts as corruption.
        corruption: bool,
        /// The refusal.
        error: String,
    },
    /// A replica's sync driver logged an inbound op as corruption.
    LoggedCorruption {
        /// Where.
        who: Who,
        /// The message.
        message: String,
    },
    /// A replica logged an error or a warning.
    LoggedError {
        /// Where.
        who: Who,
        /// `ev` and message.
        event: String,
    },
    /// A command the scenario issued failed.
    CommandFailed {
        /// Where.
        who: Who,
        /// What was asked.
        command: String,
        /// The failure.
        error: String,
    },
    /// A replica stopped syncing.
    StoppedSyncing {
        /// Where.
        who: Who,
        /// What was seen.
        detail: String,
    },
    /// A replica never materialized a task.
    Missing {
        /// Where.
        who: Who,
        /// The task.
        task: EntityRef,
    },
    /// A replica holds a value for a field that no last write gave it.
    Lost {
        /// Where.
        who: Who,
        /// The task.
        task: EntityRef,
        /// The field.
        field: Field,
        /// Every acceptable value.
        expected: Vec<Option<String>>,
        /// What the replica holds.
        got: Option<String>,
    },
}

/// A violation and the known gap that explains it, if one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// The violation.
    pub violation: Violation,
    /// The issue of the known gap that accounts for it, or `None`.
    pub issue: Option<u32>,
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
    /// Whether a `HEAD` core published a `StreamDigest` during the run.
    digest_published: bool,
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

    /// Record that a `HEAD` core published a `StreamDigest`, an op kind a
    /// baseline that predates parking classes as corruption when the relay
    /// delivers it.
    pub fn published_digest(&mut self) {
        self.digest_published = true;
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
            // Or `HEAD`'s own `StreamDigest` (ADR-0043 §5), which every
            // `HEAD` core publishes on its anti-entropy tick: an op kind
            // the baseline cannot read, so it reaches the baseline's sync
            // driver exactly as `FutureWrite::OpKind` would.
            Violation::LoggedCorruption { who: Who::A, .. } if cross => self
                .taints
                .first()
                .and_then(|t| t.write.baseline_issue())
                .or_else(|| {
                    self.digest_published
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

    /// A baseline that predates parking logs `HEAD`'s digest as corruption,
    /// which is #320, but only once `HEAD` has published one.
    #[test]
    fn a_published_digest_explains_the_floors_corruption_log_as_320() {
        let mut model = Model::default();
        assert_eq!(
            issue(&model, Some(FLOOR)),
            None,
            "no digest, no explanation"
        );
        model.published_digest();
        assert_eq!(issue(&model, Some(FLOOR)), Some(320));
    }

    /// A baseline cut after the parking fix keeps a digest, so a corruption
    /// log there is not #320 however many digests `HEAD` published.
    #[test]
    fn a_published_digest_explains_nothing_against_a_parking_baseline() {
        let mut model = Model::default();
        model.published_digest();
        let after_parking = "0000000000000000000000000000000000000000";
        assert_eq!(issue(&model, Some(after_parking)), None);
    }

    /// `HEAD` against `HEAD` parks its own digest kind, so a corruption log
    /// in the control run stays unexplained.
    #[test]
    fn a_published_digest_explains_nothing_in_the_control_run() {
        let mut model = Model::default();
        model.published_digest();
        assert_eq!(issue(&model, None), None);
    }
}
