//! The vocabulary a scenario is written in, and what each step does to a run.

use std::path::PathBuf;

use sunrise_id::EntityRef;

use super::future::FutureWrite;
use super::model::Writer;
use super::projection::Field;
use super::replica::Replica;
use super::violation::{Violation, Who};
use super::Run;

/// One side of the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Replica A.
    A,
    /// Replica B.
    B,
}

/// One step of a scenario. A task is chosen by index into the tasks every
/// replica already holds and nobody has deleted, so no command targets a task
/// its replica cannot have heard of; a step with nothing to target is skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create a task in the shared Stream.
    Create {
        /// Where.
        on: Side,
        /// Distinguishes the title.
        title: u8,
        /// Initial priority.
        priority: Option<u8>,
    },
    /// Rename a task.
    SetTitle {
        /// Where.
        on: Side,
        /// Which task.
        task: usize,
        /// Distinguishes the title.
        title: u8,
    },
    /// Set or clear a task's priority.
    SetPriority {
        /// Where.
        on: Side,
        /// Which task.
        task: usize,
        /// The new priority.
        priority: Option<u8>,
    },
    /// Delete a task.
    Delete {
        /// Where.
        on: Side,
        /// Which task.
        task: usize,
    },
    /// Settle, then have the newer build write a task, handing the op to
    /// `first` now and to the other side at the next settle.
    Future {
        /// Which task.
        task: usize,
        /// What the newer build writes.
        write: FutureWrite,
        /// Who hears it first.
        first: Side,
    },
    /// Wait until both sides have exchanged everything.
    Settle,
}

/// Which builds run.
#[derive(Debug, Clone)]
pub enum Mode {
    /// `HEAD` against `HEAD`: the control run.
    HeadOnly,
    /// The baseline driver at this path against `HEAD`.
    Baseline(PathBuf),
}

impl Run {
    pub(super) fn pick(&self, index: usize) -> Option<EntityRef> {
        if self.live.is_empty() {
            None
        } else {
            Some(self.live[index % self.live.len()])
        }
    }

    pub(super) fn side(&mut self, side: Side) -> &mut dyn Replica {
        match side {
            Side::A => self.a.as_mut(),
            Side::B => &mut self.b,
        }
    }

    pub(super) fn who(side: Side) -> Who {
        match side {
            Side::A => Who::A,
            Side::B => Who::B,
        }
    }

    fn writer(side: Side) -> Writer {
        match side {
            Side::A => Writer::A,
            Side::B => Writer::B,
        }
    }

    fn failed(&mut self, side: Side, command: String, error: String) {
        self.violations.push(Violation::CommandFailed {
            who: Self::who(side),
            command,
            error,
        });
    }

    pub(super) async fn step(&mut self, step: &Step) {
        match *step {
            Step::Create {
                on,
                title,
                priority,
            } => {
                let title = format!("task {title}");
                let inbox = self.inbox;
                match self.side(on).create_task(&title, priority, inbox).await {
                    Ok(id) => {
                        self.model.record(
                            Self::writer(on),
                            id,
                            &[
                                (Field::Title, Some(title)),
                                (Field::Priority, priority.map(|p| p.to_string())),
                                (Field::Deleted, Some("false".into())),
                                (Field::State, Some("todo".into())),
                                (Field::DueKind, None),
                                (Field::Note, None),
                                (Field::Stream, None),
                            ],
                        );
                        self.fresh.push(id);
                    }
                    Err(e) => self.failed(on, format!("create {title:?}"), e),
                }
            }
            Step::SetTitle { on, task, title } => {
                let Some(id) = self.pick(task) else { return };
                let title = format!("renamed {title}");
                match self.side(on).set_title(id, &title).await {
                    Ok(()) => {
                        self.model
                            .record(Self::writer(on), id, &[(Field::Title, Some(title))]);
                    }
                    Err(e) => self.failed(on, format!("rename {id}"), e),
                }
            }
            Step::SetPriority { on, task, priority } => {
                let Some(id) = self.pick(task) else { return };
                match self.side(on).set_priority(id, priority).await {
                    Ok(()) => self.model.record(
                        Self::writer(on),
                        id,
                        &[(Field::Priority, priority.map(|p| p.to_string()))],
                    ),
                    Err(e) => self.failed(on, format!("set priority of {id}"), e),
                }
            }
            Step::Delete { on, task } => {
                let Some(id) = self.pick(task) else { return };
                match self.side(on).delete(id).await {
                    Ok(()) => {
                        self.model.record(
                            Self::writer(on),
                            id,
                            &[(Field::Deleted, Some("true".into()))],
                        );
                        self.live.retain(|t| *t != id);
                    }
                    Err(e) => self.failed(on, format!("delete {id}"), e),
                }
            }
            Step::Future { task, write, first } => self.future(task, write, first).await,
            Step::Settle => self.settle().await,
        }
    }
}
