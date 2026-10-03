//! Cross-version merge harness (#326, ADR-0057).
//!
//! The invariant: merging a vault across client versions never breaks and
//! never loses data. Every other convergence test in this crate links one
//! build of `sunrise-core` into both replicas, so none of them can see a
//! violation of it. This one runs two:
//!
//! - **A** is the *baseline*: `sunrise-core` as a pinned commit had it,
//!   running in a separate process ([`baseline`]) because two builds of one
//!   crate do not link into one binary. In the control run A is `HEAD`.
//! - **B** is the account's first device, created by A's build and then
//!   opened by `HEAD` — the upgrade a real user performs.
//! - **C** is a third device, paired by `HEAD`, that only lends its identity
//!   to the newer-build writer ([`future`]).
//!
//! A and B sync through the real `HEAD` relay in-process. A scenario
//! ([`Step`]) interleaves commands on both, newer-build ops delivered to each
//! side in either order, and settles. At the end A is closed, reopened by
//! `HEAD`, and both are compared with the reference [`model`] computes.
//!
//! - *No break:* `HEAD` opens the vaults the baseline wrote, and the baseline
//!   never refuses an op as corruption, never logs an error, never fails a
//!   command, and is still live at the end.
//! - *No loss:* after the upgrade, every field holds what its last writer set,
//!   unless a concurrent write to that same field won.
//!
//! Known violations are expected failures that name their issue ([`gaps`]).

pub mod baseline;
pub mod events;
pub mod future;
pub mod gaps;
pub mod model;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    Clock, Command, CommandResult, Core, CoreError, EngineError, Query, QueryResult, SystemClock,
    Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{Task, TaskDraft, TaskPatch, INBOX_STREAM_BYTES};
use sunrise_id::{EntityKind, EntityRef};

use self::baseline::BaselineReplica;
use self::events::{take_head_events, LoggedEvent};
use self::future::{FutureWrite, FutureWriter};
use self::model::{project, Classified, Field, Model, Projection, Violation, Who, Writer};

/// The shared vault root every run's account is created under. Each run has
/// its own relay and its own directories, so nothing is shared through it.
const ROOT: [u8; 32] = [0x26; 32];

/// How long setup may take to reach a synced three-device account.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one settle may take before the run reports a stall.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a settle that cannot reach equality waits for nothing to change
/// before it accepts the difference and moves on. Only tasks a newer op left
/// the baseline unable to read are allowed to differ; anything else that
/// differs is reported by the final check.
const SETTLE_QUIET: Duration = Duration::from_millis(1_500);

const POLL: Duration = Duration::from_millis(25);

/// Every task's projection on A and on B, in model order.
type Snapshot = Vec<(Option<Projection>, Option<Projection>)>;

/// What an `apply_remote` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Applied, parked, or an idempotent repeat: nothing wrong.
    Accepted,
    /// Refused.
    Refused {
        /// Whether the refusal is the one the sync driver counts as corruption.
        corruption: bool,
        /// The refusal.
        error: String,
    },
}

/// One replica of the two the property compares.
#[async_trait::async_trait]
pub(crate) trait Replica: Send {
    async fn create_task(
        &mut self,
        title: &str,
        priority: Option<u8>,
        stream: EntityRef,
    ) -> Result<EntityRef, String>;
    async fn set_title(&mut self, id: EntityRef, title: &str) -> Result<(), String>;
    async fn set_priority(&mut self, id: EntityRef, priority: Option<u8>) -> Result<(), String>;
    async fn delete(&mut self, id: EntityRef) -> Result<(), String>;
    async fn apply_remote(&mut self, envelope: &[u8]) -> Applied;
    async fn task(&mut self, id: EntityRef) -> Option<Task>;
    async fn status(&mut self) -> Result<(String, u32), String>;
    async fn knows(&mut self, devices: &[[u8; 16]]) -> bool;
    async fn take_events(&mut self) -> Vec<LoggedEvent>;
    async fn close(self: Box<Self>);
}

/// A `HEAD` core in this process.
#[derive(Debug)]
pub(crate) struct HeadReplica(Arc<Core>);

impl HeadReplica {
    async fn submit(&self, cmd: Command) -> Result<CommandResult, String> {
        self.0.submit(cmd).await.map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Replica for HeadReplica {
    async fn create_task(
        &mut self,
        title: &str,
        priority: Option<u8>,
        stream: EntityRef,
    ) -> Result<EntityRef, String> {
        let draft = TaskDraft {
            title: title.to_owned(),
            stream_id: Some(stream),
            priority,
            ..TaskDraft::default()
        };
        Ok(self.submit(Command::CreateTask(draft)).await?.entity)
    }

    async fn set_title(&mut self, id: EntityRef, title: &str) -> Result<(), String> {
        let patch = TaskPatch {
            title: Some(title.to_owned()),
            ..TaskPatch::default()
        };
        self.submit(Command::UpdateTask { id, patch })
            .await
            .map(drop)
    }

    async fn set_priority(&mut self, id: EntityRef, priority: Option<u8>) -> Result<(), String> {
        let patch = TaskPatch {
            priority: Some(priority),
            ..TaskPatch::default()
        };
        self.submit(Command::UpdateTask { id, patch })
            .await
            .map(drop)
    }

    async fn delete(&mut self, id: EntityRef) -> Result<(), String> {
        self.submit(Command::DeleteTask(id)).await.map(drop)
    }

    async fn apply_remote(&mut self, envelope: &[u8]) -> Applied {
        match self.0.apply_remote(envelope).await {
            Ok(_) => Applied::Accepted,
            Err(e) => Applied::Refused {
                corruption: matches!(e, CoreError::Engine(EngineError::RemoteOpInvalid(_))),
                error: e.to_string(),
            },
        }
    }

    async fn task(&mut self, id: EntityRef) -> Option<Task> {
        match self.0.query(Query::EntityById(id)).await {
            Ok(QueryResult::Task(t)) => Some(*t),
            _ => None,
        }
    }

    async fn status(&mut self) -> Result<(String, u32), String> {
        match self.0.query(Query::SyncStatus).await {
            Ok(QueryResult::SyncStatus(s)) => Ok((format!("{:?}", s.state), s.outbox_pending)),
            Ok(other) => Err(format!("expected SyncStatus, got {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn knows(&mut self, devices: &[[u8; 16]]) -> bool {
        let Ok(QueryResult::Devices(rows)) = self.0.query(Query::DeviceList).await else {
            return false;
        };
        devices
            .iter()
            .all(|d| rows.iter().any(|r| r.device_id == *d))
    }

    async fn take_events(&mut self) -> Vec<LoggedEvent> {
        // `HEAD` cores share one process and one capture; the run reads it
        // once for all of them rather than per replica.
        Vec::new()
    }

    async fn close(self: Box<Self>) {
        self.0.shutdown().await;
    }
}

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

/// Everything a run found.
#[derive(Debug)]
pub struct Report {
    /// The baseline's full commit id, or `None` for the control run.
    pub baseline: Option<String>,
    /// Every violation, classified.
    pub violations: Vec<Classified>,
}

impl Report {
    /// The violations no known gap accounts for.
    #[must_use]
    pub fn unexplained(&self) -> Vec<&Violation> {
        self.violations
            .iter()
            .filter(|c| c.issue.is_none())
            .map(|c| &c.violation)
            .collect()
    }

    /// The issues of the known gaps this run hit.
    #[must_use]
    pub fn issues(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.violations.iter().filter_map(|c| c.issue).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// One newer op on its way to one side.
#[derive(Debug, Clone)]
struct Delivery {
    to: Side,
    task: EntityRef,
    write: FutureWrite,
    /// What the op sets its field to, as [`project`] reads it back.
    value: Option<String>,
    envelope: Vec<u8>,
}

struct Run {
    a: Box<dyn Replica>,
    a_is_baseline: bool,
    /// The vault root A is reopened under at the upgrade.
    root: [u8; 32],
    b: HeadReplica,
    c: Arc<Core>,
    /// The Stream every task lives in.
    inbox: EntityRef,
    devices: [[u8; 16]; 3],
    future: FutureWriter,
    model: Model,
    /// Tasks every replica holds, minus the deleted.
    live: Vec<EntityRef>,
    /// Created since the last settle; join `live` at the next one.
    fresh: Vec<EntityRef>,
    /// Newer ops still owed to one side, handed over at the next settle.
    owed: Vec<Delivery>,
    violations: Vec<Violation>,
    future_count: u32,
}

/// Run `steps` in `mode` and report what broke.
///
/// # Panics
/// When the harness itself cannot be built: no relay, a `HEAD` core that does
/// not open, a driver that does not start. Those are not findings about the
/// invariant and are not reported as violations.
pub async fn run(mode: &Mode, steps: &[Step]) -> Report {
    let dir = tempfile::tempdir().expect("tempdir");
    let (addr, relay) = crate::spawn_relay().await;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    // Drain anything an earlier run in this process left behind.
    let _ = take_head_events();

    let Pair {
        a,
        baseline_ref,
        b,
        device_a,
        root,
    } = match open_pair(mode, dir.path(), addr, &clock).await {
        Ok(pair) => pair,
        // `HEAD` would not open the vault the baseline wrote: nothing past
        // this point can run, and this is the whole report.
        Err((baseline_ref, refusal)) => {
            relay.abort();
            let violations = Model::default().classify(baseline_ref.as_deref(), vec![refusal]);
            return Report {
                baseline: baseline_ref,
                violations,
            };
        }
    };
    let device_b = b.device_id();
    let (c, future) = open_future_writer(&b, &dir.path().join("c"), addr, &clock).await;

    let mut run = Run {
        a,
        a_is_baseline: baseline_ref.is_some(),
        root,
        b: HeadReplica(b),
        devices: [device_a, device_b, c.device_id()],
        c,
        inbox: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        future,
        model: Model::default(),
        live: Vec::new(),
        fresh: Vec::new(),
        owed: Vec::new(),
        violations: Vec::new(),
        future_count: 0,
    };

    if run.setup().await {
        for step in steps {
            run.step(step).await;
        }
        run.settle().await;
        run.finish(&dir.path().join("a"), addr, &clock).await;
    }

    let Run {
        a,
        b,
        c,
        model,
        violations,
        ..
    } = run;
    a.close().await;
    Box::new(b).close().await;
    c.shutdown().await;
    relay.abort();

    let violations = model.classify(baseline_ref.as_deref(), violations);
    Report {
        baseline: baseline_ref,
        violations,
    }
}

/// The two replicas the property compares, and what reopening them needs.
struct Pair {
    a: Box<dyn Replica>,
    baseline_ref: Option<String>,
    b: Arc<Core>,
    device_a: [u8; 16],
    /// The vault root both were created under.
    root: [u8; 32],
}

/// Open the vault in `dir` with `HEAD`, as the device it already is.
async fn reopen(
    dir: &Path,
    root: [u8; 32],
    addr: SocketAddr,
    clock: &Arc<dyn Clock>,
) -> Result<Arc<Core>, String> {
    crate::try_open_core_unlocked(
        dir,
        addr,
        Arc::clone(clock),
        Some(crate::ws_factory(addr)),
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(root),
            paired: None,
        },
    )
    .await
    .map_err(|e| format!("{e:?}"))
}

/// Create the account and open A and B: A as `mode` says, B as `HEAD`.
///
/// In a baseline run B is the upgrade: the baseline creates it, and `HEAD`
/// opening it is the first thing the property checks. A refusal comes back as
/// the violation, with the baseline's tag.
async fn open_pair(
    mode: &Mode,
    dir: &Path,
    addr: SocketAddr,
    clock: &Arc<dyn Clock>,
) -> Result<Pair, (Option<String>, Violation)> {
    match mode {
        Mode::HeadOnly => {
            let b = crate::open_synced_core(&dir.join("b"), ROOT, addr, Arc::clone(clock)).await;
            let a = crate::open_paired_core(&dir.join("a"), &b, addr, Arc::clone(clock)).await;
            Ok(Pair {
                device_a: a.device_id(),
                a: Box::new(HeadReplica(a)),
                baseline_ref: None,
                b,
                root: ROOT,
            })
        }
        Mode::Baseline(binary) => {
            let mut driver = BaselineReplica::spawn(binary).expect("start the baseline driver");
            let init = driver
                .init(
                    &dir.join("a"),
                    &dir.join("b"),
                    &format!("http://{addr}"),
                    ROOT,
                )
                .expect("the baseline creates the account");
            let tag = driver.baseline.clone();
            let b = match reopen(&dir.join("b"), init.root, addr, clock).await {
                Ok(b) => b,
                Err(error) => {
                    return Err((Some(tag), Violation::UpgradeRefused { who: Who::B, error }))
                }
            };
            assert_eq!(
                b.device_id(),
                init.device_b,
                "HEAD reopened a different device"
            );
            Ok(Pair {
                a: Box::new(driver),
                baseline_ref: Some(tag),
                b,
                device_a: init.device_a,
                root: init.root,
            })
        }
    }
}

/// Pair C to B with `HEAD` and build the newer-build writer on C's identity.
///
/// Every task lives in the Inbox. It is the one Stream whose key every device
/// holds from the moment it is paired: a Stream created later mints its key
/// with its first op and reaches the others by key envelope, which would make
/// the writer's key a race with that envelope.
async fn open_future_writer(
    b: &Core,
    dir: &Path,
    addr: SocketAddr,
    clock: &Arc<dyn Clock>,
) -> (Arc<Core>, FutureWriter) {
    let payload = crate::pair_with(b);
    let signing_seed = payload.d_s_priv;
    let c = crate::open_core_paired(
        dir,
        payload.vault_root,
        addr,
        Arc::clone(clock),
        Some(crate::ws_factory(addr)),
        Some(Box::new(payload)),
    )
    .await;
    let keys = b.held_stream_keys();
    let (epoch, key) = keys
        .get(&INBOX_STREAM_BYTES)
        .and_then(|epochs| epochs.iter().next_back().map(|(e, k)| (*e, *k)))
        .expect("B holds the Inbox key");
    let writer = FutureWriter::new(c.device_id(), signing_seed, INBOX_STREAM_BYTES, epoch, key);
    (c, writer)
}

impl Run {
    /// Wait for a synced three-device account. A baseline that never gets
    /// there has stopped syncing before the scenario began.
    async fn setup(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + SETUP_TIMEOUT;
        let others_of_a = [self.devices[1], self.devices[2]];
        let others_of_b = [self.devices[0], self.devices[2]];
        loop {
            let a_ok = self.a.knows(&others_of_a).await;
            let b_ok = self.b.knows(&others_of_b).await;
            if a_ok && b_ok && self.pending_zero().await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                let status = self.a.status().await;
                self.violations.push(Violation::StoppedSyncing {
                    who: Who::A,
                    detail: format!(
                        "setup never converged: A knows B and C: {a_ok}; \
                         B knows A and C: {b_ok}; A status {status:?}"
                    ),
                });
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn pending_zero(&mut self) -> bool {
        let a = matches!(self.a.status().await, Ok((_, 0)));
        let b = matches!(self.b.status().await, Ok((_, 0)));
        let c = matches!(HeadReplica(Arc::clone(&self.c)).status().await, Ok((_, 0)));
        a && b && c
    }

    fn pick(&self, index: usize) -> Option<EntityRef> {
        if self.live.is_empty() {
            None
        } else {
            Some(self.live[index % self.live.len()])
        }
    }

    fn side(&mut self, side: Side) -> &mut dyn Replica {
        match side {
            Side::A => self.a.as_mut(),
            Side::B => &mut self.b,
        }
    }

    fn who(side: Side) -> Who {
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

    async fn step(&mut self, step: &Step) {
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

    /// Settle, have the newer build write a task, hand the op to `first` now
    /// and owe it to the other side until the next settle.
    async fn future(&mut self, task: usize, write: FutureWrite, first: Side) {
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
    async fn settle(&mut self) {
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

    /// Read A's no-break evidence, upgrade A to `HEAD`, and compare both
    /// replicas with the reference.
    async fn finish(&mut self, dir_a: &Path, addr: SocketAddr, clock: &Arc<dyn Clock>) {
        match self.a.status().await {
            Ok((state, _)) if state == "Live" => {}
            other => self.violations.push(Violation::StoppedSyncing {
                who: Who::A,
                detail: format!("A ended the scenario in {other:?}, not Live"),
            }),
        }
        // The baseline's own log: every corruption and every error is a break.
        for e in self.a.take_events().await {
            if e.is_corrupt_op() {
                self.violations.push(Violation::LoggedCorruption {
                    who: Who::A,
                    message: e.message,
                });
            } else if e.level == "ERROR" || e.level == "WARN" {
                self.violations.push(Violation::LoggedError {
                    who: Who::A,
                    event: format!("{} {:?}: {}", e.level, e.ev, e.message),
                });
            }
        }
        // The `HEAD` cores share this process's log, so a corruption there is
        // attributed to `HEAD` as a whole, which `Who::B` stands for.
        for e in take_head_events()
            .into_iter()
            .filter(LoggedEvent::is_corrupt_op)
        {
            self.violations.push(Violation::LoggedCorruption {
                who: Who::B,
                message: e.message,
            });
        }

        // Upgrade A: close whatever build it is, reopen its directory with
        // `HEAD`. Opening runs the parked-op replay.
        let a = std::mem::replace(&mut self.a, Box::new(Closed));
        a.close().await;
        let upgraded = match reopen(dir_a, self.root, addr, clock).await {
            Ok(core) => Some(core),
            Err(error) => {
                self.violations.push(Violation::UpgradeRefused {
                    who: Who::AUpgraded,
                    error,
                });
                None
            }
        };
        if let Some(core) = &upgraded {
            self.a = Box::new(HeadReplica(Arc::clone(core)));
            self.a_is_baseline = false;
            self.settle().await;
        }

        let tasks: Vec<EntityRef> = self.model.tasks().to_vec();
        let mut fa = BTreeMap::new();
        let mut fb = BTreeMap::new();
        for t in tasks {
            fa.insert(t, self.a.task(t).await.as_ref().map(project));
            fb.insert(t, self.b.task(t).await.as_ref().map(project));
        }
        // A that did not open has no state to compare; its refusal above is
        // the violation, and B is still checked.
        if upgraded.is_some() {
            let lost_a = self.model.check(Who::AUpgraded, &fa);
            self.violations.extend(lost_a);
        }
        let lost_b = self.model.check(Who::B, &fb);
        self.violations.extend(lost_b);
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

/// The placeholder A holds between closing the old build and opening `HEAD`.
struct Closed;

#[async_trait::async_trait]
impl Replica for Closed {
    async fn create_task(
        &mut self,
        _: &str,
        _: Option<u8>,
        _: EntityRef,
    ) -> Result<EntityRef, String> {
        Err("closed".into())
    }
    async fn set_title(&mut self, _: EntityRef, _: &str) -> Result<(), String> {
        Err("closed".into())
    }
    async fn set_priority(&mut self, _: EntityRef, _: Option<u8>) -> Result<(), String> {
        Err("closed".into())
    }
    async fn delete(&mut self, _: EntityRef) -> Result<(), String> {
        Err("closed".into())
    }
    async fn apply_remote(&mut self, _: &[u8]) -> Applied {
        Applied::Refused {
            corruption: false,
            error: "closed".into(),
        }
    }
    async fn task(&mut self, _: EntityRef) -> Option<Task> {
        None
    }
    async fn status(&mut self) -> Result<(String, u32), String> {
        Err("closed".into())
    }
    async fn knows(&mut self, _: &[[u8; 16]]) -> bool {
        false
    }
    async fn take_events(&mut self) -> Vec<LoggedEvent> {
        Vec::new()
    }
    async fn close(self: Box<Self>) {}
}
