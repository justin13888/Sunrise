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
mod delivery;
pub mod events;
pub mod future;
pub mod gaps;
pub mod model;
pub mod projection;
mod replica;
mod step;
pub mod violation;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Core, SystemClock, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::INBOX_STREAM_BYTES;
use sunrise_id::{EntityKind, EntityRef};

use self::baseline::BaselineReplica;
use self::delivery::Delivery;
use self::events::{take_head_events, LoggedEvent};
use self::future::FutureWriter;
use self::model::Model;
use self::projection::project;
pub use self::replica::Applied;
use self::replica::{Closed, HeadReplica, Replica};
pub use self::step::{Mode, Side, Step};
use self::violation::{Classified, Violation, Who};

/// The shared vault root every run's account is created under. Each run has
/// its own relay and its own directories, so nothing is shared through it.
const ROOT: [u8; 32] = [0x26; 32];

/// How long setup may take to reach a synced three-device account.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(25);

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
/// Every task lives in the Inbox, whose ops are sealed under the private key
/// domain (`INBOX_STREAM_BYTES`). Every device holds that domain's key from
/// the moment it is paired: a Stream created later mints its key
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
    ///
    /// Each `HEAD` side also waits for the `DeviceFeatures` of every other
    /// `HEAD` device, so its first write already knows which write mode the
    /// vault is in (ADR-0065 §7): `Patch` in a `HEAD`-only run, where every
    /// device advertises `core.field_ops`, and full-state against a baseline,
    /// which never advertises it.
    async fn setup(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + SETUP_TIMEOUT;
        let others_of_a = [self.devices[1], self.devices[2]];
        let others_of_b = [self.devices[0], self.devices[2]];
        let head_others_of_b: Vec<[u8; 16]> = if self.a_is_baseline {
            vec![self.devices[2]]
        } else {
            others_of_b.to_vec()
        };
        loop {
            let a_ok =
                self.a.knows(&others_of_a).await && self.a.holds_field_ops_of(&others_of_a).await;
            let b_ok = self.b.knows(&others_of_b).await
                && self.b.holds_field_ops_of(&head_others_of_b).await;
            let c_ok = matches!(HeadReplica(Arc::clone(&self.c)).status().await, Ok((_, 0)));
            if a_ok && b_ok && c_ok && self.pending_zero().await {
                // From here on C is the newer build, whose ops only the
                // `FutureWriter` seals, and it owns C's Inbox sequence from 1.
                // C's own `HEAD` core would otherwise publish a `StreamDigest`
                // into the Inbox once it had applied a task (ADR-0043 §5), at
                // a seq the writer also uses: a fork, after which a replica
                // that held the digest first keeps it and drops the newer op.
                self.c.shutdown().await;
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
        a && b
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
        let head = take_head_events();
        if head.iter().any(LoggedEvent::is_unparkable_kind_published) {
            self.model.published_unparkable_kind();
        }
        for e in head.into_iter().filter(LoggedEvent::is_corrupt_op) {
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
