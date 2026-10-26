//! Pruning the version DAG below what every device has passed (ADR-0065
//! D25, issue #581).
//!
//! The rule itself is a pure function of an entity's versions and of what
//! compaction knows (`merge::prune::passed`), tested first on hand-built
//! inputs. The rest drive real engines: devices that edit, sync, publish
//! their digests and compact in any order, a replica that never prunes and
//! takes every op in another order, a replica that joins a snapshot of
//! one of them, and the cases the rule names. Last, a snapshot's
//! `doc_state` made hostile, whose refused join must leave nothing behind.

use super::compaction::{ack_outbox, all_envs, digest_op, DAY, INBOX};
use super::conflicts::conflicts_of;
use super::*;
use crate::engine::merge::{passed, Node, OpRef, Passage, Stamp};
use crate::engine::CompactionPolicy;
use ciborium::value::Value;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_crypto::decode_envelope;

// ---- the rule, on hand-built versions ----

const S: [u8; 16] = [0x51; 16];
const S2: [u8; 16] = [0x52; 16];
const A: [u8; 16] = [0xA0; 16];
const B: [u8; 16] = [0xB0; 16];

fn at(device: [u8; 16], seq: u64, ms: u64) -> Stamp {
    Stamp {
        hlc: sunrise_cbor::hlc::Hlc::at(ms),
        device,
        seq,
        stream: S,
    }
}

/// A version stamped `me`, naming `parents`, whose frontier joins its own
/// stamp with every node in `reaches`.
fn node(me: Stamp, parents: &[Stamp], reaches: &[Stamp]) -> Node {
    let mut frontier = BTreeMap::new();
    for s in reaches.iter().chain([&me]) {
        frontier
            .entry(s.device)
            .and_modify(|held: &mut Stamp| *held = (*held).max(*s))
            .or_insert(*s);
    }
    Node {
        at: me,
        parents: parents.iter().map(Stamp::op_ref).collect(),
        frontier,
        head: false,
        keys: false,
    }
}

/// A's chain `a1 ← a2 ← a3`, with `a3` the head, every op compacted, and B
/// having acknowledged all of it.
struct Chain {
    nodes: Vec<Node>,
    compacted: BTreeSet<OpRef>,
    passage: Passage,
    a: [Stamp; 3],
}

fn chain() -> Chain {
    let a = [at(A, 1, 10), at(A, 2, 20), at(A, 3, 30)];
    let mut nodes = vec![
        node(a[0], &[], &[]),
        node(a[1], &[a[0]], &[a[0]]),
        node(a[2], &[a[1]], &[a[0], a[1]]),
    ];
    nodes[2].head = true;
    let mut passage = Passage::default();
    passage.floors.insert((S, A), 3);
    passage.cursors.insert((S, B), 7);
    passage.acks.insert((S, B, A), 3);
    passage.acks.insert((S, B, B), 7);
    passage.writers.insert(B);
    Chain {
        compacted: a.iter().map(Stamp::op_ref).collect(),
        nodes,
        passage,
        a,
    }
}

fn pass(c: &Chain) -> BTreeSet<OpRef> {
    passed(&c.nodes, &BTreeSet::new(), &c.compacted, &c.passage)
}

/// Every version below a head that a device acknowledged a descendant of is
/// passed; the head, which is also A's newest, is not.
#[test]
fn a_chain_every_device_passed_keeps_only_its_head() {
    let c = chain();
    assert_eq!(pass(&c), BTreeSet::from([c.a[0].op_ref(), c.a[1].op_ref()]));
}

/// (a): a version whose op is still in the log, or above its floor, stays,
/// and so does the one below it whose reader it is (c).
#[test]
fn a_version_whose_op_is_not_compacted_stays_and_holds_its_parent() {
    let mut c = chain();
    c.compacted.remove(&c.a[1].op_ref());
    assert!(pass(&c).is_empty(), "a2 is uncompacted, and a1's reader");
}

/// (b): a register member and the newest key-writing `Patch` stay.
#[test]
fn a_register_member_and_the_streak_witness_stay() {
    let c = chain();
    let members = BTreeSet::from([c.a[0].op_ref()]);
    assert_eq!(
        passed(&c.nodes, &members, &c.compacted, &c.passage),
        BTreeSet::from([c.a[1].op_ref()])
    );
    let mut c = chain();
    c.nodes[0].keys = true;
    c.nodes[1].keys = true;
    assert_eq!(
        pass(&c),
        BTreeSet::from([c.a[0].op_ref()]),
        "only the newest that wrote the keys is kept"
    );
}

/// (b): a device's newest version stays even when it is not a head.
#[test]
fn each_devices_newest_version_stays() {
    let mut c = chain();
    let b1 = at(B, 1, 15);
    // B's only version is below A's head, so it is no head, but it is B's
    // newest: B's next version's frontier joins it (D2).
    c.nodes.insert(1, node(b1, &[c.a[0]], &[c.a[0]]));
    c.nodes[2] = node(c.a[1], &[c.a[0], b1], &[c.a[0], b1]);
    c.nodes[3] = node(c.a[2], &[c.a[1]], &[c.a[0], b1, c.a[1]]);
    c.nodes[3].head = true;
    c.compacted.insert(b1.op_ref());
    c.passage.floors.insert((S, B), 1);
    let gone = pass(&c);
    assert!(!gone.contains(&b1.op_ref()), "{gone:?}");
    assert_eq!(gone, BTreeSet::from([c.a[0].op_ref(), c.a[1].op_ref()]));
}

/// (c): a version read by one in another stream stays: a replica that joins
/// this stream's snapshot can still fold that reader from its op.
#[test]
fn a_version_read_from_another_stream_stays() {
    let mut c = chain();
    c.nodes[2].at.stream = S2;
    c.a[2].stream = S2;
    c.compacted = c.a.iter().map(Stamp::op_ref).collect();
    c.passage.floors.insert((S2, A), 3);
    c.passage.acks.insert((S2, B, A), 3);
    c.passage.cursors.insert((S2, B), 7);
    c.passage.acks.insert((S2, B, B), 7);
    assert_eq!(pass(&c), BTreeSet::from([c.a[0].op_ref()]));
}

/// (d): a parent named and neither held nor covered is an ancestor still to
/// come, and nothing of the entity is pruned.
#[test]
fn an_entity_with_a_missing_parent_prunes_nothing() {
    let mut c = chain();
    let missing = at(B, 9, 5);
    c.nodes[0].parents.push(missing.op_ref());
    assert!(pass(&c).is_empty());
    // Covered, it was pruned here or never an op of this entity.
    c.passage.floors.insert((S, B), 9);
    assert_eq!(pass(&c).len(), 2);
}

/// (e): a device that acknowledged too little, whose own prefix this
/// replica lacks, that holds an op here unapplied, or that never spoke,
/// holds pruning back; one that wrote the descendant itself passes it.
#[test]
fn a_device_passes_only_what_it_acknowledged_with_its_prefix_held() {
    let c = chain();
    let tweak = |f: &dyn Fn(&mut Passage)| {
        let mut c = chain();
        f(&mut c.passage);
        pass(&c)
    };
    assert_eq!(
        tweak(&|p| {
            p.acks.insert((S, B, A), 2);
        }),
        BTreeSet::from([c.a[0].op_ref()]),
        "B reached a2, so passed a1 alone"
    );
    assert!(tweak(&|p| {
        p.acks.insert((S, B, A), 1);
    })
    .is_empty());
    assert!(
        tweak(&|p| {
            p.cursors.insert((S, B), 6);
        })
        .is_empty(),
        "B's own ops before its digest are not all here"
    );
    assert!(tweak(&|p| {
        p.cursors.remove(&(S, B));
    })
    .is_empty());
    assert_eq!(
        tweak(&|p| {
            p.acks.remove(&(S, B, B));
            p.cursors.insert((S, B), 0);
        })
        .len(),
        2,
        "a digest that names no position of B's own asks for none"
    );
    assert!(tweak(&|p| {
        p.stuck.insert(B);
    })
    .is_empty());
    assert!(tweak(&|p| {
        p.acks.clear();
    })
    .is_empty());
    assert!(tweak(&|p| {
        p.writers.insert([0xC0; 16]);
    })
    .is_empty());
    assert_eq!(
        tweak(&|p| {
            p.writers.clear();
        })
        .len(),
        2,
        "with no other device, this replica's own heads decide"
    );
    // A, as a writer of its own, passed what it wrote below a version of its
    // own it holds the prefix of.
    assert_eq!(
        tweak(&|p| {
            p.writers.insert(A);
            p.cursors.insert((S, A), 3);
        })
        .len(),
        2
    );
    assert_eq!(
        tweak(&|p| {
            p.writers.insert(A);
            p.cursors.insert((S, A), 2);
        }),
        BTreeSet::from([c.a[0].op_ref()]),
        "A's prefix reaches a2 here, so a1 alone"
    );
}

/// (e) reads `≺`, which is strict: a device that acknowledged a version and
/// nothing above it may still name it, and has passed only what is below.
#[test]
fn a_device_that_acknowledged_a_version_alone_has_not_passed_it() {
    let mut c = chain();
    let d = [0xD0; 16];
    c.passage.writers.insert(d);
    c.passage.acks.insert((S, d, A), 2);
    assert_eq!(pass(&c), BTreeSet::from([c.a[0].op_ref()]));
}

/// (e) through another device's version: a version two branches descend
/// from is passed by a device that acknowledged either, even one whose
/// frontier reaches it through another device's entry.
#[test]
fn a_version_two_branches_descend_from_is_passed_through_either() {
    let mut c = chain();
    let b1 = at(B, 1, 25);
    // B's b1 names a2, and is B's newest and a head beside A's a3.
    c.nodes.insert(2, node(b1, &[c.a[1]], &[c.a[0], c.a[1]]));
    c.nodes[2].head = true;
    c.compacted.insert(b1.op_ref());
    c.passage.floors.insert((S, B), 1);
    // D acknowledged b1 alone: its frontier reaches a2 on A's entry, which
    // is inclusive, so D passed a2 as well as a1.
    let d = [0xD0; 16];
    c.passage.writers.insert(d);
    c.passage.acks.insert((S, d, B), 1);
    assert_eq!(pass(&c), BTreeSet::from([c.a[0].op_ref(), c.a[1].op_ref()]));
    c.passage.acks.remove(&(S, d, B));
    c.passage.acks.insert((S, d, A), 2);
    assert_eq!(
        pass(&c),
        BTreeSet::from([c.a[0].op_ref()]),
        "on A's own entry a2 is exclusive"
    );
}

/// A device's reach is the greatest over what it acknowledged: a later
/// version that reaches less on one device does not lower it.
#[test]
fn a_later_acknowledgement_reaching_less_does_not_lower_the_reach() {
    let mut c = chain();
    // B's b1, newer than A's head, names only a1.
    let b1 = at(B, 1, 35);
    c.nodes.push(node(b1, &[c.a[0]], &[c.a[0]]));
    c.nodes[3].head = true;
    c.compacted.insert(b1.op_ref());
    c.passage.floors.insert((S, B), 1);
    let d = [0xD0; 16];
    c.passage.writers.insert(d);
    c.passage.acks.insert((S, d, A), 3);
    c.passage.acks.insert((S, d, B), 1);
    assert_eq!(pass(&c), BTreeSet::from([c.a[0].op_ref(), c.a[1].op_ref()]));
}

/// Two acknowledged versions that reach one stamp, one as its own (which
/// does not precede itself) and a later one through its frontier, reach it
/// inclusively.
#[test]
fn reaching_a_stamp_through_a_later_version_makes_it_inclusive() {
    let mut c = chain();
    let b1 = at(B, 1, 25);
    c.nodes.insert(2, node(b1, &[c.a[1]], &[c.a[0], c.a[1]]));
    c.nodes[2].head = true;
    c.compacted.insert(b1.op_ref());
    c.passage.floors.insert((S, B), 1);
    let d = [0xD0; 16];
    c.passage.writers.insert(d);
    // D acknowledged a2 and b1, not a3.
    c.passage.acks.insert((S, d, A), 2);
    c.passage.acks.insert((S, d, B), 1);
    assert_eq!(pass(&c), BTreeSet::from([c.a[0].op_ref(), c.a[1].op_ref()]));
}

// ---- real engines ----

/// A retention window of nothing: every acknowledged op is eligible.
fn policy() -> CompactionPolicy {
    CompactionPolicy {
        retention_ms: 0,
        ..CompactionPolicy::default()
    }
}

/// Devices of one vault, each on its own clock, that trust each other and,
/// in `patch` mode, have advertised `core.field_ops`, so every command
/// writes a `Patch` naming its writer's heads (ADR-0065 §7).
struct Devices {
    engines: Vec<Engine>,
    clocks: Vec<Arc<FakeClock>>,
    dbs: Vec<Db>,
    /// Versions pruned so far.
    pruned: u64,
}

impl Devices {
    fn new(n: u8, patch: bool) -> Self {
        let clocks: Vec<Arc<FakeClock>> = (0..n)
            .map(|_| Arc::new(FakeClock(PLMutex::new(T0))))
            .collect();
        let engines: Vec<Engine> = (0..n)
            .map(|i| {
                engine_seeded(ROOT, [i + 1; 32], clocks[usize::from(i)].clone())
                    .with_features(crate::feature::FEATURES)
            })
            .collect();
        let mut dbs: Vec<Db> = engines.iter().map(|_| db_root(ROOT)).collect();
        for (i, e) in engines.iter().enumerate() {
            for (j, other) in engines.iter().enumerate() {
                if i != j {
                    trust(e, &mut dbs[i], other);
                }
            }
            if patch {
                e.advertise_features(&mut dbs[i]).unwrap();
            }
        }
        let mut d = Self {
            engines,
            clocks,
            dbs,
            pruned: 0,
        };
        d.sync_all();
        d
    }

    /// Deliver every op `from` holds that `to` neither holds nor has folded
    /// below a floor, in relay order.
    fn sync(&mut self, from: usize, to: usize) {
        let me = self.engines[to].keychain.device_id();
        for env in all_envs(&self.dbs[from]) {
            let e = decode_envelope(&env).unwrap();
            let db = &mut self.dbs[to];
            let seq = i64::try_from(e.seq).unwrap();
            let held: bool = db
                .conn()
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM ops WHERE stream_id = ?1 AND device_id = ?2
                                    AND seq = ?3)
                         OR EXISTS (SELECT 1 FROM compaction_floor WHERE stream_id = ?1
                                    AND device_id = ?2 AND seq >= ?3)",
                    params![&e.stream_id[..], &e.device_id[..], seq],
                    |r| r.get(0),
                )
                .unwrap();
            if e.device_id != me && !held {
                self.engines[to].apply_remote_all(db, &env).unwrap();
            }
        }
    }

    fn sync_all(&mut self) {
        for _ in 0..2 {
            for from in 0..self.engines.len() {
                for to in 0..self.engines.len() {
                    if from != to {
                        self.sync(from, to);
                    }
                }
            }
        }
    }

    fn at(&self, who: usize, ms: u64) {
        set_clock(&self.clocks[who], ms);
    }

    fn edit(&mut self, who: usize, task: EntityRef, patch: TaskPatch) {
        self.engines[who]
            .apply(&mut self.dbs[who], Command::UpdateTask { id: task, patch })
            .unwrap();
    }

    fn digest(&mut self, who: usize) {
        digest_op(&self.engines[who], &mut self.dbs[who], &INBOX);
    }

    /// Fold `who`'s log, prune, and check the rebuild still lands on it.
    fn compact(&mut self, who: usize) -> crate::engine::CompactionReport {
        ack_outbox(&mut self.dbs[who]);
        let report = self.engines[who]
            .compact_op_log(&mut self.dbs[who], &policy())
            .unwrap();
        self.pruned += report.versions_pruned;
        self.engines[who].assert_rebuild_identity(&self.dbs[who]);
        report
    }
}

/// A replica that trusts every engine in `peers` and has applied nothing.
fn receiver(peers: &[Engine]) -> (Engine, Db) {
    receiver_at(peers, T0 + 1_000_000)
}

/// A replica's contiguous prefix in one stream: each device's
/// `last_applied_seq`. A snapshot written there states it as its frontier.
type Prefix = BTreeMap<Vec<u8>, i64>;

/// `db`'s [`Prefix`] in `stream`.
fn prefix(db: &Db, stream: &[u8; 16]) -> Prefix {
    let mut stmt = db
        .conn()
        .prepare("SELECT device_id, last_applied_seq FROM sync_cursors WHERE stream_id = ?1")
        .unwrap();
    stmt.query_map(params![&stream[..]], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// [`receiver`], its clock at `now_ms`.
fn receiver_at(peers: &[Engine], now_ms: u64) -> (Engine, Db) {
    let e = engine_seeded(ROOT, [0x77; 32], Arc::new(FakeClock(PLMutex::new(now_ms))));
    let mut db = db_root(ROOT);
    for p in peers {
        trust(&e, &mut db, p);
    }
    (e, db)
}

/// Everything a reader sees and the merge decides by: every projection
/// table but the version DAG's own, which pruning shrinks, and the group
/// rows' local arrival times; and the heads, which a writer names.
fn outcome(db: &Db) -> Vec<String> {
    let mut out: Vec<String> = crate::engine::rebuild::check::dump(db.conn())
        .into_iter()
        .filter(|(t, _)| !matches!(*t, "entity_versions" | "entity_frontier" | "pending_groups"))
        .flat_map(|(t, rows)| rows.into_iter().map(move |r| format!("{t}: {r}")))
        .collect();
    let mut stmt = db
        .conn()
        .prepare("SELECT entity_id, stream, device, seq FROM entity_versions WHERE is_head = 1")
        .unwrap();
    let mut heads: Vec<String> = stmt
        .query_map([], |r| {
            Ok(format!(
                "head: {:?} {:?} {:?} {}",
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    heads.sort();
    out.extend(heads);
    out
}

fn versions_of(db: &Db, entity: EntityRef) -> (i64, i64) {
    let count = |table: &str| -> i64 {
        db.conn()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE entity_id = ?"),
                [&entity.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    (count("entity_versions"), count("entity_frontier"))
}

fn title(n: usize) -> TaskPatch {
    TaskPatch {
        title: Some(format!("t{n}")),
        ..Default::default()
    }
}

fn priority(n: usize) -> TaskPatch {
    TaskPatch {
        priority: Some(Some(u8::try_from(n % 5).unwrap() + 1)),
        ..Default::default()
    }
}

/// Add `ctx` to `task`'s contexts as `who` holds them, or, with `on`
/// false, take it away: an OR-set add or remove. Nothing when the set is
/// already so.
fn tag(d: &mut Devices, who: usize, task: EntityRef, ctx: EntityRef, on: bool) {
    let mut held = task_of(&d.engines[who], &d.dbs[who], task).contexts;
    let changed = if on {
        held.insert(ctx)
    } else {
        held.remove(&ctx)
    };
    if changed {
        d.edit(
            who,
            task,
            TaskPatch {
                contexts: Some(held.into_iter().collect()),
                ..Default::default()
            },
        );
    }
}

/// Retitle both `tasks` in one command: a group of two members (D14).
fn group_retitle(d: &mut Devices, who: usize, tasks: [EntityRef; 2], n: usize) {
    use crate::commands::{BulkTaskAction, BulkTaskItem};
    let items = tasks
        .iter()
        .enumerate()
        .map(|(i, id)| BulkTaskItem {
            id: *id,
            action: BulkTaskAction::Update(Box::new(TaskPatch {
                title: Some(format!("g{n}.{i}")),
                ..Default::default()
            })),
        })
        .collect();
    d.engines[who]
        .apply(&mut d.dbs[who], Command::BulkTasks(items))
        .unwrap();
}

#[derive(Debug, Clone)]
enum Step {
    Title(usize),
    Priority(usize),
    /// Add the context at this index to the first task.
    Tag(usize, usize),
    /// Take the context at this index off the first task.
    Untag(usize, usize),
    /// Retitle both tasks in one group.
    Group(usize),
    /// Write this device's snapshots of the vault-meta stream and the
    /// Inbox, for a replica to join at the end.
    Snapshot(usize),
    Sync {
        from: usize,
        to: usize,
    },
    Digest(usize),
    Compact(usize),
    /// Every device catches up and publishes its digest, which is what lets
    /// a later compaction prune at all.
    Settle,
}

/// How many contexts the property tags the task with.
const CONTEXTS: usize = 3;

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0usize..3).prop_map(Step::Title),
        1 => (0usize..3).prop_map(Step::Priority),
        2 => (0usize..3, 0usize..CONTEXTS).prop_map(|(who, c)| Step::Tag(who, c)),
        1 => (0usize..3, 0usize..CONTEXTS).prop_map(|(who, c)| Step::Untag(who, c)),
        1 => (0usize..3).prop_map(Step::Group),
        1 => (0usize..3).prop_map(Step::Snapshot),
        4 => (0usize..3, 0usize..3)
            .prop_filter("another device", |(a, b)| a != b)
            .prop_map(|(from, to)| Step::Sync { from, to }),
        2 => (0usize..3).prop_map(Step::Digest),
        2 => (0usize..3).prop_map(Step::Compact),
        1 => Just(Step::Settle),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        // docs/10-cross-cutting/testing.md §2: a shrunken counterexample is
        // committed, at a path named here.
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/prune.txt",
            ),
        )),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..ProptestConfig::default()
    })]

    /// Three devices edit one task's title and priority, tag and untag it
    /// with three contexts, retitle it in a group with a second task, catch
    /// up on each other, publish their digests, compact, pruning their
    /// version rows, and write snapshots, all at random. Every op any of
    /// them wrote is kept as it was written. Once they have all caught up,
    /// each holds the rows, registers, losers, conflicts and heads that a
    /// replica holds which never pruned and took every op in another order,
    /// and a rebuild of each lands on its rows (checked after every delivery
    /// and every compaction). So does a replica that held a random subset of
    /// the ops, joined one of the snapshots and then took every op: the
    /// state of a set of ops is the join of the states of any cover of it
    /// (ADR-0065 §8).
    #[test]
    fn pruning_at_any_point_changes_nothing_any_delivery_order_folds(
        patch in any::<bool>(),
        steps in proptest::collection::vec(arb_step(), 1..32),
        order in proptest::collection::vec(any::<prop::sample::Index>(), 0..48),
        joiner_held in proptest::collection::vec(any::<prop::sample::Index>(), 0..16),
        joined in any::<prop::sample::Index>(),
    ) {
        begin_case();
        let mut d = Devices::new(3, patch);
        let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
        let second = new_task(&d.engines[0], &mut d.dbs[0], "u");
        let contexts: Vec<EntityRef> = (0..CONTEXTS)
            .map(|c| new_context(&d.engines[0], &mut d.dbs[0], &format!("c{c}")))
            .collect();
        d.sync_all();
        for i in 0..3 {
            d.digest(i);
        }
        d.sync_all();

        let mut written: BTreeSet<Vec<u8>> = BTreeSet::new();
        let keep = |d: &Devices, written: &mut BTreeSet<Vec<u8>>| {
            for db in &d.dbs {
                written.extend(all_envs(db));
            }
        };
        keep(&d, &mut written);
        // Each snapshot record, beside the writer's prefix it states.
        let mut snapshots: Vec<[(Vec<u8>, Prefix); 2]> = Vec::new();
        for (n, step) in steps.iter().enumerate() {
            let ms = T0 + 1_000 + u64::try_from(n).unwrap() * 7;
            match step {
                Step::Title(who) => {
                    d.at(*who, ms);
                    d.edit(*who, task, title(n));
                }
                Step::Priority(who) => {
                    d.at(*who, ms);
                    d.edit(*who, task, priority(n));
                }
                Step::Tag(who, c) | Step::Untag(who, c) => {
                    d.at(*who, ms);
                    tag(&mut d, *who, task, contexts[*c], matches!(step, Step::Tag(..)));
                }
                Step::Group(who) => {
                    d.at(*who, ms);
                    group_retitle(&mut d, *who, [task, second], n);
                }
                Step::Snapshot(who) => {
                    d.at(*who, ms);
                    let mut write = |stream: &[u8; 16]| {
                        let record = d.engines[*who]
                            .write_stream_snapshot(&mut d.dbs[*who], stream)
                            .unwrap();
                        record.map(|r| (r, prefix(&d.dbs[*who], stream)))
                    };
                    if let (Some(meta), Some(inbox)) = (write(&META_STREAM), write(&INBOX)) {
                        snapshots.push([meta, inbox]);
                    }
                }
                Step::Sync { from, to } => d.sync(*from, *to),
                Step::Digest(who) => {
                    d.at(*who, ms);
                    d.digest(*who);
                }
                Step::Compact(who) => {
                    d.at(*who, ms);
                    d.compact(*who);
                }
                Step::Settle => {
                    d.sync_all();
                    for who in 0..3 {
                        d.at(who, ms);
                        d.digest(who);
                    }
                    d.sync_all();
                }
            }
            keep(&d, &mut written);
        }
        d.sync_all();

        let written: Vec<Vec<u8>> = written.into_iter().collect();
        let (r, mut rdb) = receiver(&d.engines);
        for ix in &order {
            r.apply_remote_all(&mut rdb, &written[ix.index(written.len())]).unwrap();
        }
        for env in written.iter().rev() {
            r.apply_remote_all(&mut rdb, env).unwrap();
        }
        let want = outcome(&rdb);
        let want_conflicts = conflicts_of(&r, &rdb, true);
        for (e, db) in d.engines.iter().zip(&d.dbs) {
            prop_assert_eq!(&outcome(db), &want);
            prop_assert_eq!(&conflicts_of(e, db, true), &want_conflicts);
            e.assert_rebuild_identity(db);
        }

        // A snapshot taken at a random point, joined into a replica that
        // holds a random subset of the ops, then the tail.
        if !snapshots.is_empty() {
            let records = &snapshots[joined.index(snapshots.len())];
            let (j, mut jdb) = receiver(&d.engines);
            for ix in &joiner_held {
                j.apply_remote_all(&mut jdb, &written[ix.index(written.len())]).unwrap();
            }
            for ((record, stated), stream) in records.iter().zip([META_STREAM, INBOX]) {
                // The join is applied exactly when the record states an op
                // past the joiner's own prefix, and covered otherwise.
                let held = prefix(&jdb, &stream);
                let adds = stated
                    .iter()
                    .any(|(device, seq)| held.get(device).copied().unwrap_or(0) < *seq);
                let (applied, _) = j.apply_snapshot(&mut jdb, record).unwrap();
                if adds {
                    prop_assert!(
                        matches!(applied, crate::engine::SnapshotApplied::Applied { .. }),
                        "a record past the joiner's prefix was {:?}",
                        applied
                    );
                } else {
                    prop_assert_eq!(applied, crate::engine::SnapshotApplied::Covered);
                }
            }
            for env in &written {
                j.apply_remote_all(&mut jdb, env).unwrap();
            }
            prop_assert_eq!(&outcome(&jdb), &want);
            prop_assert_eq!(&conflicts_of(&j, &jdb, true), &want_conflicts);
            j.assert_rebuild_identity(&jdb);
        }
    }
}

/// The property above prunes, rather than passing because nothing ever met
/// the rule: a fixed run of it that compacts after every device has caught
/// up and acknowledged.
#[test]
fn the_property_prunes() {
    let mut d = Devices::new(3, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    for n in 0..12 {
        let who = n % 3;
        d.at(who, T0 + 1_000 + u64::try_from(n).unwrap() * 7);
        d.edit(who, task, title(n));
        d.sync_all();
    }
    for i in 0..3 {
        d.digest(i);
    }
    d.sync_all();
    d.compact(0);
    d.compact(1);
    // Not 16: the create always writes `stream_id` (ADR-0046 C1), so it is
    // that register's winner and is kept.
    assert_eq!(d.pruned, 15);
}

/// After `n` edits of one task, alternating between `devices` devices that
/// catch up after each, acknowledge and compact, the task keeps a bounded
/// number of version rows: its heads, each device's newest, its registers'
/// winners and the uncompacted tip, and no more as `n` grows. Without
/// pruning it keeps `n + 1`.
#[test]
fn version_rows_stay_bounded_however_many_edits() {
    for devices in 1u8..=3 {
        let mut bound = None;
        // Equal modulo every device count and the edit pattern's period, so
        // the same kinds of version are each one's newest.
        for n in [12usize, 48] {
            let mut d = Devices::new(devices, true);
            // A rebuild after every delivery makes this quadratic in the
            // edits; the compaction below checks it once.
            crate::engine::rebuild::check::disable();
            let k = usize::from(devices);
            let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
            d.sync_all();
            for i in 0..n {
                let who = i % k;
                d.at(who, T0 + 1_000 + u64::try_from(i).unwrap() * 7);
                let patch = if i % 4 == 3 { priority(i) } else { title(i) };
                d.edit(who, task, patch);
                d.sync_all();
            }
            assert_eq!(
                versions_of(&d.dbs[0], task).0,
                i64::try_from(n).unwrap() + 1
            );
            for i in 0..k {
                d.digest(i);
            }
            d.sync_all();
            d.compact(0);
            let (rows, frontier) = versions_of(&d.dbs[0], task);
            // Heads (one), each device's newest, the winners of `title`,
            // `priority` and the create's other registers, and the versions
            // whose ops are each device's uncompacted tip.
            assert!(
                rows <= 2 * i64::from(devices) + 2,
                "{devices} devices, {n} edits: {rows}"
            );
            assert!(frontier <= rows * i64::from(devices));
            match bound {
                None => bound = Some(rows),
                Some(b) => assert_eq!(rows, b, "{devices} devices: rows grew with the edits"),
            }
        }
    }
}

/// A device that has not acknowledged holds pruning back, however long it
/// is silent; its late edit, written against what it had, then folds on the
/// replica that compacted exactly as on one that never did.
#[test]
fn a_silent_device_holds_pruning_back_and_its_late_edit_folds_exactly() {
    let mut d = Devices::new(3, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    d.at(0, T0 + 1_000);
    d.edit(0, task, title(1));
    d.sync_all();
    // C goes silent here, having applied `t1`, and edits offline.
    d.at(2, T0 + 1_500);
    d.edit(2, task, title(99));
    let late = all_envs(&d.dbs[2])
        .into_iter()
        .filter(|e| {
            let env = decode_envelope(e).unwrap();
            env.device_id == d.engines[2].keychain.device_id() && env.stream_id == INBOX
        })
        .max_by_key(|e| decode_envelope(e).unwrap().seq)
        .unwrap();
    for n in 2..10 {
        let who = n % 2;
        d.at(who, T0 + 2_000 + u64::try_from(n).unwrap() * 7);
        d.edit(who, task, title(n));
        d.sync(0, 1);
        d.sync(1, 0);
    }
    // A long silence: past the op-log floor's known-device window.
    for who in 0..2 {
        d.at(who, T0 + 40 * DAY);
        d.digest(who);
    }
    d.sync(0, 1);
    d.sync(1, 0);
    let report = d.compact(0);
    assert_eq!(
        report.ops_removed, 10,
        "C no longer holds the op log back: {report:?}"
    );
    assert_eq!(
        report.versions_pruned, 0,
        "but C may still name anything it held"
    );

    d.engines[0].apply_remote_all(&mut d.dbs[0], &late).unwrap();
    d.engines[1].apply_remote_all(&mut d.dbs[1], &late).unwrap();
    assert_eq!(outcome(&d.dbs[0]), outcome(&d.dbs[1]));
    assert_eq!(
        conflicts_of(&d.engines[0], &d.dbs[0], true),
        conflicts_of(&d.engines[1], &d.dbs[1], true)
    );
    assert!(
        conflicts_of(&d.engines[0], &d.dbs[0], false)
            .iter()
            .any(|c| c.entity == task && c.field == "title"),
        "C's offline title is concurrent with A's and B's later ones"
    );
    d.engines[0].assert_rebuild_identity(&d.dbs[0]);
}

/// The rule's stated limit: a device revoked longer ago than the retention
/// window, whose uploads the relay refuses and no longer holds, does not
/// hold pruning back. A version of it that arrives anyway and names a pruned
/// parent folds as one naming a parent never applied: the parent adds
/// nothing to its frontier. Here that parent reached the version that set
/// the priority, so where a replica that kept it lets the late priority
/// replace that one, the replica that pruned keeps both in the register's
/// unresolved set. Both read the same priority, nothing is lost, and the
/// next priority edit, naming the heads, settles both alike.
#[test]
fn a_late_version_naming_a_pruned_parent_folds_by_the_documented_rule() {
    let mut d = Devices::new(3, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    d.at(0, T0 + 1_007);
    d.edit(0, task, title(1));
    d.at(0, T0 + 1_014);
    d.edit(0, task, priority(2));
    d.at(0, T0 + 1_021);
    d.edit(0, task, title(3));
    d.sync_all();
    // C sets the priority having seen `t3`, and the op is held back.
    d.at(2, T0 + 1_100);
    d.edit(2, task, priority(4));
    let c_id = d.engines[2].keychain.device_id();
    let late = all_envs(&d.dbs[2])
        .into_iter()
        .filter(|e| {
            let env = decode_envelope(e).unwrap();
            env.device_id == c_id && env.stream_id == INBOX
        })
        .max_by_key(|e| decode_envelope(e).unwrap().seq)
        .unwrap();
    for n in 5..9 {
        d.at(0, T0 + 1_200 + u64::try_from(n).unwrap() * 7);
        d.edit(0, task, title(n));
    }
    d.sync(0, 1);
    for who in 0..2 {
        d.at(who, T0 + 2 * DAY);
        revoke(
            &d.engines[who],
            &mut d.dbs[who],
            &d.engines[0],
            c_id,
            T0 + 2 * DAY,
        );
    }
    for who in 0..2 {
        d.at(who, T0 + 5 * DAY);
        d.digest(who);
    }
    d.sync(0, 1);
    d.sync(1, 0);
    let short = CompactionPolicy {
        retention_ms: DAY,
        ..policy()
    };
    ack_outbox(&mut d.dbs[0]);
    let report = d.engines[0].compact_op_log(&mut d.dbs[0], &short).unwrap();
    assert_eq!(report.versions_pruned, 5, "{report:?}");

    for who in 0..2 {
        d.engines[who]
            .apply_remote_all(&mut d.dbs[who], &late)
            .unwrap();
    }
    let quiet = |d: &Devices, who: usize| -> Vec<String> {
        conflicts_of(&d.engines[who], &d.dbs[who], true)
            .iter()
            .filter(|c| c.entity == task)
            .map(|c| c.field.clone())
            .collect()
    };
    assert_eq!(
        quiet(&d, 1),
        Vec::<String>::new(),
        "B kept `t3`: C's priority replaces A's"
    );
    assert_eq!(
        quiet(&d, 0),
        vec!["priority".to_owned()],
        "A pruned it: both stay"
    );
    assert_eq!(
        task_of(&d.engines[0], &d.dbs[0], task),
        task_of(&d.engines[1], &d.dbs[1], task),
        "and the task reads the same on both"
    );
    d.engines[0].assert_rebuild_identity(&d.dbs[0]);

    d.at(0, T0 + 6 * DAY);
    d.at(1, T0 + 6 * DAY);
    d.edit(0, task, priority(5));
    d.sync(0, 1);
    assert_eq!(quiet(&d, 0), Vec::<String>::new());
    assert_eq!(outcome(&d.dbs[0]), outcome(&d.dbs[1]));
}

/// (a) reads the log, not the floor alone: a version below its floor whose
/// op compaction kept, here one the relay has not acknowledged, stays, since
/// a rebuild folds it again from the log.
#[test]
fn a_version_whose_op_compaction_kept_is_not_pruned() {
    let mut d = Devices::new(2, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    for n in 0..6 {
        d.at(0, T0 + 1_000 + u64::try_from(n).unwrap() * 7);
        d.edit(0, task, title(n));
    }
    d.sync_all();
    for i in 0..2 {
        d.digest(i);
    }
    d.sync_all();
    let report = d.engines[0]
        .compact_op_log(&mut d.dbs[0], &policy())
        .unwrap();
    assert_eq!(report.floors_raised, 1, "{report:?}");
    assert_eq!(report.versions_pruned, 0, "every op of A's is still unsent");
    d.engines[0].assert_rebuild_identity(&d.dbs[0]);
}

/// (b) keeps a register's winner even when it is no head and not its
/// device's newest: the conflicts query orders the heads against the
/// winning delete, which needs its row.
#[test]
fn the_winning_delete_stays_for_the_conflicts_it_explains() {
    let mut d = Devices::new(2, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    d.at(0, T0 + 1_000);
    d.engines[0]
        .apply(&mut d.dbs[0], Command::DeleteTask(task))
        .unwrap();
    d.at(1, T0 + 1_001);
    d.edit(1, task, title(1));
    // A edits inside the tombstone, after its own delete, before it hears
    // of B's edit: the delete is neither a head nor A's newest.
    d.at(0, T0 + 1_002);
    d.edit(0, task, priority(2));
    d.sync_all();
    for i in 0..2 {
        d.digest(i);
    }
    d.sync_all();
    let before = conflicts_of(&d.engines[0], &d.dbs[0], true);
    assert!(
        before.iter().any(|c| !c.edited_while_deleted.is_empty()),
        "{before:?}"
    );
    d.compact(0);
    assert_eq!(conflicts_of(&d.engines[0], &d.dbs[0], true), before);
    assert_eq!(
        conflicts_of(&d.engines[0], &d.dbs[0], true),
        conflicts_of(&d.engines[1], &d.dbs[1], true)
    );
}

/// A rebuild of a pruned vault lands where a rebuild of it before the prune
/// did, and so does a rebuild of a replica that never pruned.
#[test]
fn a_rebuild_after_pruning_equals_one_before() {
    let mut d = Devices::new(2, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    for n in 0..10 {
        let who = n % 2;
        d.at(who, T0 + 1_000 + u64::try_from(n).unwrap() * 7);
        d.edit(who, task, if n % 3 == 0 { priority(n) } else { title(n) });
        if n % 4 != 3 {
            d.sync_all();
        }
    }
    d.sync_all();
    for i in 0..2 {
        d.digest(i);
    }
    d.sync_all();
    d.engines[0].rebuild_projection(&mut d.dbs[0]).unwrap();
    let before = outcome(&d.dbs[0]);
    let report = d.compact(0);
    assert_eq!(report.versions_pruned, 7, "{report:?}");
    assert_eq!(outcome(&d.dbs[0]), before);
    d.engines[0].rebuild_projection(&mut d.dbs[0]).unwrap();
    assert_eq!(outcome(&d.dbs[0]), before);
    d.engines[1].rebuild_projection(&mut d.dbs[1]).unwrap();
    assert_eq!(outcome(&d.dbs[1]), before);
}

/// A device that joins the snapshot of a replica that pruned, then takes
/// the ops above it, holds what a replica that replayed every op holds; and
/// its next edit, naming the heads the snapshot carried, folds alike
/// everywhere.
#[test]
fn a_snapshot_from_a_pruned_replica_bootstraps_a_full_replay() {
    let mut d = Devices::new(2, true);
    let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
    d.sync_all();
    for n in 0..8 {
        let who = n % 2;
        d.at(who, T0 + 1_000 + u64::try_from(n).unwrap() * 7);
        d.edit(who, task, title(n));
        if n != 5 {
            d.sync_all();
        }
    }
    d.sync_all();
    for i in 0..2 {
        d.digest(i);
    }
    d.sync_all();
    let report = d.compact(0);
    assert_eq!(report.versions_pruned, 5, "{report:?}");
    let meta = d.engines[0]
        .write_stream_snapshot(&mut d.dbs[0], &META_STREAM)
        .unwrap()
        .unwrap();
    let inbox = d.engines[0]
        .write_stream_snapshot(&mut d.dbs[0], &INBOX)
        .unwrap()
        .unwrap();
    // The history goes on above the snapshot.
    d.at(1, T0 + 2_000);
    d.edit(1, task, title(50));
    d.sync_all();

    let (j, mut jdb) = receiver(&d.engines);
    for record in [&meta, &inbox] {
        let (outcome, _) = j.apply_snapshot(&mut jdb, record).unwrap();
        assert!(
            matches!(outcome, crate::engine::SnapshotApplied::Applied { .. }),
            "{outcome:?}"
        );
    }
    for env in all_envs(&d.dbs[0]) {
        j.apply_remote_all(&mut jdb, &env).unwrap();
    }
    assert_eq!(outcome(&jdb), outcome(&d.dbs[1]));
    assert_eq!(
        conflicts_of(&j, &jdb, true),
        conflicts_of(&d.engines[1], &d.dbs[1], true)
    );
    j.assert_rebuild_identity(&jdb);
}

/// A routine whose occurrences are completed one after another keeps a
/// bounded number of versions, though each completion wrote one, and each
/// was a member of a group with its occurrence's completion (D22). A device
/// that joins the vault-meta snapshot written after the prune, and then
/// takes the occurrences' stream as ops, completes each group: the
/// snapshot names the pruned members present (D14). Without them, the
/// completions would wait for members that never come.
#[test]
fn a_pruned_group_member_is_still_present_to_a_joiner() {
    let mut d = Devices::new(2, true);
    for who in 0..2 {
        d.at(who, u64::try_from(NOW).unwrap());
    }
    let (rid, occ) = seed_routine(&d.engines[0], &mut d.dbs[0]);
    d.sync_all();
    let mut last = 0;
    for (task, due) in &occ {
        last = u64::try_from(*due + 600_000).unwrap();
        for who in 0..2 {
            d.at(who, last);
        }
        d.engines[0]
            .apply(&mut d.dbs[0], Command::CompleteTask(*task))
            .unwrap();
        d.sync_all();
    }
    // An edit after the last completion: the newest `Patch` that wrote the
    // keys is then neither a head nor its device's newest, and is kept as
    // what the derived streak reads (D17).
    d.engines[0]
        .apply(
            &mut d.dbs[0],
            Command::UpdateRoutine {
                scope: sunrise_domain::EditScope::All,
                id: rid,
                patch: RoutinePatch {
                    grace_window_s: Some(Some(7_200)),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    d.sync_all();
    let occurrences = *stream_ref(5).bytes();
    for who in 0..2 {
        for stream in [META_STREAM, occurrences] {
            assert!(d.engines[who]
                .publish_stream_digest(&mut d.dbs[who], &stream)
                .unwrap());
        }
    }
    d.sync_all();
    let before = versions_of(&d.dbs[0], rid).0;
    d.compact(0);
    let after = versions_of(&d.dbs[0], rid).0;
    assert_eq!((before, after), (6, 3), "the routine's versions");

    let meta = d.engines[0]
        .write_stream_snapshot(&mut d.dbs[0], &META_STREAM)
        .unwrap()
        .unwrap();
    let doc = d.dbs[0]
        .with_tx(|tx| crate::engine::merge::dump_stream_state(tx, &META_STREAM))
        .unwrap();
    let members = |doc: &Value| -> usize {
        doc.as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("members"))
            .map_or(0, |(_, v)| v.as_array().unwrap().len())
    };
    assert_eq!(members(&doc), 3, "{doc:?}");
    let stream_ops = |db: &Db| -> Vec<Vec<u8>> {
        all_envs(db)
            .into_iter()
            .filter(|e| decode_envelope(e).unwrap().stream_id == occurrences)
            .collect()
    };
    let held = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM pending_groups WHERE applied = 0",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };

    let (j, mut jdb) = receiver_at(&d.engines, last);
    let (outcome_j, _) = j.apply_snapshot(&mut jdb, &meta).unwrap();
    assert!(
        matches!(outcome_j, crate::engine::SnapshotApplied::Applied { .. }),
        "{outcome_j:?}"
    );
    for env in stream_ops(&d.dbs[1]) {
        j.apply_remote_all(&mut jdb, &env).unwrap();
    }
    assert_eq!(held(&jdb), 0, "every completion found its routine member");
    for (task, _) in &occ {
        assert_eq!(
            task_of(&j, &jdb, *task).state,
            task_of(&d.engines[1], &d.dbs[1], *task).state
        );
        assert_eq!(task_of(&j, &jdb, *task).state, TaskState::Done);
    }
    let (joined, replayed) = (
        routine_of(&j, &jdb, rid),
        routine_of(&d.engines[1], &d.dbs[1], rid),
    );
    assert_eq!(joined.streak_keys, replayed.streak_keys);
    assert_eq!(joined.streak_counter, replayed.streak_counter);
    assert_eq!(replayed.streak_counter, 4);
    // The row a joined state first projects carries the routine's last
    // edit, not its create: the compacted base and a snapshot both project
    // a routine with no row yet.
    assert_eq!(joined.updated_at, replayed.updated_at);
    j.assert_rebuild_identity(&jdb);

    // The same state without `members`, as a build that does not write it
    // would carry it: the completions whose routine member was pruned wait.
    let Value::Map(mut top) = doc else {
        panic!("a map")
    };
    top.retain(|(k, _)| k.as_text() != Some("members"));
    let bare = Value::Map(top);
    let (k, mut kdb) = receiver_at(&d.engines, last);
    crate::engine::rebuild::check::disable();
    kdb.with_tx(|tx| {
        crate::engine::merge::join_stream_state(tx, &bare)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    })
    .unwrap();
    for env in stream_ops(&d.dbs[1]) {
        k.apply_remote_all(&mut kdb, &env).unwrap();
    }
    assert_eq!(held(&kdb), 3);
}

// ---- a hostile doc_state ----

/// Every row of every table, each table's rows sorted.
fn every_row(db: &Db) -> Vec<(String, Vec<String>)> {
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    tables
        .into_iter()
        .map(|t| {
            let mut stmt = db
                .conn()
                .prepare(&format!("SELECT * FROM \"{t}\""))
                .unwrap();
            let n = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|cols| cols.join(" | "))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.sort();
            (t, rows)
        })
        .collect()
}

/// The lists of an entity that `doc_state` version 2 carries.
const ENTITY_LISTS: [&str; 8] = [
    "stamps",
    "registers",
    "losers",
    "maps",
    "adds",
    "removes",
    "deltas",
    "versions",
];

/// An entry no list of `doc_state` accepts: not an array, an empty one, or
/// one whose every item is an index past any stamp or version.
fn arb_hostile_entry() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Bool(true)),
        Just(Value::Array(Vec::new())),
        Just(Value::Array(vec![Value::Integer(u64::MAX.into()); 8])),
    ]
}

#[derive(Debug, Clone)]
enum Hostile {
    /// Put `entry` into the list `ENTITY_LISTS[list]` of one entity, or its
    /// `frontier`, at a position in it.
    Entry {
        entity: prop::sample::Index,
        list: Option<usize>,
        at: prop::sample::Index,
        entry: Value,
    },
    /// Name, as an entity's kind, a tag this build does not merge.
    Kind(prop::sample::Index),
    /// A `members` entry that is not `[group, size, stamp]`.
    Member(Value),
}

fn arb_hostile() -> impl Strategy<Value = Hostile> {
    prop_oneof![
        6 => (
            any::<prop::sample::Index>(),
            proptest::option::of(0..ENTITY_LISTS.len()),
            any::<prop::sample::Index>(),
            arb_hostile_entry(),
        )
            .prop_map(|(entity, list, at, entry)| Hostile::Entry { entity, list, at, entry }),
        1 => any::<prop::sample::Index>().prop_map(Hostile::Kind),
        1 => arb_hostile_entry().prop_map(Hostile::Member),
    ]
}

/// `doc` with `h` applied to it.
fn corrupt(doc: &Value, h: &Hostile) -> Value {
    let Value::Map(mut top) = doc.clone() else {
        panic!("a map")
    };
    let field = |entries: &mut Vec<(Value, Value)>, name: &str| -> usize {
        entries
            .iter()
            .position(|(k, _)| k.as_text() == Some(name))
            .unwrap_or_else(|| {
                entries.push((Value::Text(name.into()), Value::Array(Vec::new())));
                entries.len() - 1
            })
    };
    match h {
        Hostile::Member(entry) => {
            let i = field(&mut top, "members");
            top[i].1.as_array_mut().unwrap().push(entry.clone());
        }
        Hostile::Entry { entity, .. } | Hostile::Kind(entity) => {
            let i = field(&mut top, "entities");
            let entities = top[i].1.as_array_mut().unwrap();
            let at_entity = entity.index(entities.len());
            let Value::Map(e) = &mut entities[at_entity] else {
                panic!("an entity is a map")
            };
            if let Hostile::Entry {
                list, at, entry, ..
            } = h
            {
                let name = list.map_or("frontier", |l| ENTITY_LISTS[l]);
                let j = field(e, name);
                let items = e[j].1.as_array_mut().unwrap();
                items.insert(at.index(items.len() + 1), entry.clone());
            } else {
                let j = field(e, "kind");
                e[j].1 = Value::Text("no such kind".into());
            }
        }
    }
    Value::Map(top)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/prune.txt",
            ),
        )),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..ProptestConfig::default()
    })]

    /// A `doc_state` made hostile in one place, anywhere in it, is refused,
    /// and the refused join leaves no row behind once the snapshot apply
    /// path's transaction rolls back, however much of the document was
    /// joined before the refusal and whatever the replica held. The same
    /// document unharmed joins.
    #[test]
    fn a_refused_join_leaves_no_trace(
        patch in any::<bool>(),
        meta in any::<bool>(),
        held in proptest::collection::vec(any::<prop::sample::Index>(), 0..12),
        hostile in arb_hostile(),
    ) {
        begin_case();
        let mut d = Devices::new(2, patch);
        let task = new_task(&d.engines[0], &mut d.dbs[0], "t");
        let second = new_task(&d.engines[0], &mut d.dbs[0], "u");
        let ctx = new_context(&d.engines[0], &mut d.dbs[0], "c");
        new_context(&d.engines[0], &mut d.dbs[0], "d");
        d.sync_all();
        // Concurrent writes, so the document carries losers and a version
        // antichain; a tag; and a group.
        d.at(0, T0 + 1_000);
        d.edit(0, task, title(1));
        d.at(1, T0 + 1_001);
        d.edit(1, task, title(2));
        tag(&mut d, 1, task, ctx, true);
        group_retitle(&mut d, 0, [task, second], 3);
        d.sync_all();

        let stream = if meta { META_STREAM } else { INBOX };
        let doc = d.dbs[0]
            .with_tx(|tx| crate::engine::merge::dump_stream_state(tx, &stream))
            .unwrap();
        let bad = corrupt(&doc, &hostile);
        let written = all_envs(&d.dbs[0]);
        let (r, mut rdb) = receiver(&d.engines);
        for ix in &held {
            r.apply_remote_all(&mut rdb, &written[ix.index(written.len())]).unwrap();
        }

        let before = every_row(&rdb);
        let refused = crate::engine::snapshot::in_tx(&mut rdb, |tx| {
            crate::engine::merge::join_stream_state(tx, &bad)
        });
        prop_assert!(matches!(refused, Err(EngineError::Invalid(_))), "{:?}", refused);
        prop_assert_eq!(every_row(&rdb), before);
        let unharmed = crate::engine::snapshot::in_tx(&mut rdb, |tx| {
            crate::engine::merge::join_stream_state(tx, &doc)
        });
        prop_assert!(unharmed.is_ok(), "{:?}", unharmed);
    }
}
