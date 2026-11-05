//! How newer ops reach each side, and the settle that waits for both sides to
//! have exchanged everything.

use std::time::Duration;

use sunrise_id::EntityRef;

use super::future::{self, FutureWrite};
use super::model::Writer;
use super::projection::{project, Field, Projection};
use super::replica::{Applied, Replica};
use super::step::Side;
use super::violation::{Violation, Who};
use super::{Run, POLL};

/// How long one settle may take before the run reports a stall.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a settle that cannot reach equality waits for nothing to change
/// before it accepts the difference and moves on. Only tasks a newer op left
/// the baseline unable to read are allowed to differ; anything else that
/// differs is reported by the final check.
const SETTLE_QUIET: Duration = Duration::from_millis(1_500);

/// Every task's projection on A and on B, in model order.
type Snapshot = Vec<(Option<Projection>, Option<Projection>)>;

/// One newer op on its way to one side.
#[derive(Debug, Clone)]
pub(super) struct Delivery {
    to: Side,
    task: EntityRef,
    write: FutureWrite,
    /// What the op sets its field to, as [`project`] reads it back.
    value: Option<String>,
    envelope: Vec<u8>,
}

impl Run {
    /// Settle, have the newer build write a task, hand the op to `first` now
    /// and owe it to the other side until the next settle.
    pub(super) async fn future(&mut self, task: usize, write: FutureWrite, first: Side) {
        self.settle().await;
        let Some(id) = self.pick(task) else { return };
        // B is `HEAD` and has settled, so it holds every earlier write, newer
        // ones included: the state a newer build would start from.
        let Some(current) = self.b.task(id).await else {
            self.violations.push(Violation::Missing {
                who: Who::B,
                task: id,
            });
            return;
        };
        self.future_count += 1;
        let envelope = self
            .future
            .seal(write, &current, self.c.now_ms(), self.future_count);
        let value = project_future(write, self.future_count);
        let fields: Vec<(Field, Option<String>)> = write
            .field()
            .map(|f| (f, value.clone()))
            .into_iter()
            .collect();
        self.model.record(Writer::Future, id, &fields);
        let other = match first {
            Side::A => Side::B,
            Side::B => Side::A,
        };
        let now = Delivery {
            to: first,
            task: id,
            write,
            value,
            envelope,
        };
        let later = Delivery {
            to: other,
            ..now.clone()
        };
        self.deliver(&now).await;
        self.owed.push(later);
    }

    /// Hand one newer op to one side, and record what that side made of it.
    ///
    /// A baseline that accepts the op has not necessarily kept it: a build
    /// that reads an unknown enum value as its fallback accepts the op and
    /// stores the fallback. So acceptance is checked against what the baseline
    /// then holds, and an op it did not keep is recorded the same as one it
    /// refused.
    async fn deliver(&mut self, d: &Delivery) {
        let carried = self.model.carried(d.task);
        match self.side(d.to).apply_remote(&d.envelope).await {
            Applied::Accepted => {
                if d.to == Side::B && self.a_is_baseline {
                    self.model.tainted(d.task, d.write);
                }
                if d.to == Side::A && self.a_is_baseline {
                    if let Some(field) = d.write.field() {
                        let held = self.a.task(d.task).await.as_ref().map(project);
                        let kept = held.and_then(|p| p.get(&field).cloned().flatten());
                        if kept != d.value {
                            self.model.unkept(d.task, d.write, &carried);
                        }
                    }
                }
            }
            Applied::Refused { corruption, error } => {
                if d.to == Side::A && self.a_is_baseline {
                    self.model.unkept(d.task, d.write, &carried);
                }
                self.violations.push(Violation::Refused {
                    who: Self::who(d.to),
                    task: d.task,
                    write: d.write,
                    carried,
                    corruption,
                    error,
                });
            }
        }
    }

    /// Hand every owed newer op over, then wait until A and B have exchanged
    /// everything: outboxes empty, and every task they can both read equal.
    pub(super) async fn settle(&mut self) {
        for d in std::mem::take(&mut self.owed) {
            self.deliver(&d).await;
        }
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        let mut last: Option<Snapshot> = None;
        let mut quiet_since = tokio::time::Instant::now();
        loop {
            let pending = self.pending_zero().await;
            let tasks: Vec<EntityRef> = self.model.tasks().to_vec();
            let mut pairs: Snapshot = Vec::new();
            for t in &tasks {
                let a = self.a.task(*t).await.as_ref().map(project);
                let b = self.b.task(*t).await.as_ref().map(project);
                pairs.push((a, b));
            }
            let equal = tasks
                .iter()
                .zip(&pairs)
                .all(|(t, (x, y))| !self.model.comparable(*t) || x == y);
            if pending && equal {
                break;
            }
            let now = tokio::time::Instant::now();
            let snapshot = Some(pairs);
            if snapshot != last {
                last = snapshot;
                quiet_since = now;
            } else if pending && now.duration_since(quiet_since) >= SETTLE_QUIET {
                break;
            }
            if now >= deadline {
                let status = self.a.status().await;
                self.violations.push(Violation::StoppedSyncing {
                    who: Who::A,
                    detail: format!(
                        "a settle did not finish in {SETTLE_TIMEOUT:?}; A status {status:?}"
                    ),
                });
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        self.model.settle();
        self.live.append(&mut self.fresh);
    }
}

/// The value a newer write gives its field, as [`project`] reads it back.
fn project_future(write: FutureWrite, n: u32) -> Option<String> {
    match write {
        FutureWrite::Field => Some(format!("note {n}")),
        FutureWrite::EnumValue => Some(future::FUTURE_STATE.to_owned()),
        FutureWrite::NestedKind => Some(future::FUTURE_TIME_KIND.to_owned()),
        FutureWrite::OpKind => None,
    }
}
