//! Pruning the version DAG below what every device has passed (ADR-0065
//! clarification D25, issue #581).
//!
//! # What a version row is still for
//!
//! A version's row and its frontier rows (`entity_versions`,
//! `entity_frontier`, [`super::versions`]) are read for five things:
//!
//! 1. **Admitting a version that names it.** A version's frontier is the
//!    join of the frontiers of the parents it names and of its device's
//!    previous version (D2), read off their rows when it is admitted, and
//!    again by every recompute after a late arrival.
//! 2. **Ordering a write against it.** A register keeps the writes no other
//!    write descends from, and reads each kept write's own frontier to tell
//!    ([`super::register`]); the heads are each device's newest version, and
//!    the conflicts query reads them and the winning delete.
//! 3. **A late ancestor.** A version that arrives after one that reaches it
//!    makes the stored frontiers between them stale, and the recompute walks
//!    the rows to bring them up.
//! 4. **Projection.** A routine derives its streak once a `Patch` above the
//!    legacy floor wrote its keys (D17), which it reads off the rows.
//! 5. **A rebuild or a joiner.** A rebuild refolds every op still in the log
//!    over the compacted base, and a replica that joins this one's
//!    `doc_state` folds the ops it receives over the rows the snapshot
//!    carried, so each folds an op's version by (1) again.
//!
//! A version none of the five can reach again is dead weight: one row, and a
//! frontier row per device it reaches, for as long as the vault lives (D10).
//!
//! # The rule
//!
//! A version `p` of an entity is **passed**, and its rows are deleted, when
//! all of this holds ([`passed`]):
//!
//! - **(a) Compacted.** Its op is at or below its prefix's compaction floor
//!   and no longer in the log. No delivery applies it again, since a
//!   delivery at a covered position is a duplicate (ADR-0059 §2), and no
//!   rebuild refolds it.
//! - **(b) Not read.** It is not a head, not its device's newest version of
//!   the entity, not a register's winner or loser, and not the newest
//!   `Patch` that wrote `streak_keys` or `skipped_keys`. That is (2) and (4).
//! - **(c) Its readers went with it.** Every version that names it as a
//!   parent, and its device's next version of the entity, is in its stream
//!   and is itself compacted by (a). Those are the only versions whose
//!   stored frontier was computed from its own (1), and none of them is
//!   folded again: not by a rebuild, whose base carries their stored
//!   frontiers, and not by a replica that joins this stream's snapshot,
//!   whose floor the snapshot raises past them.
//! - **(d) The entity is closed.** Every op-ref a version of the entity
//!   names as a parent is a version here, or is at or below its prefix's
//!   floor. So no ancestor of a version here is still to arrive (3) through
//!   a parent edge.
//! - **(e) Every device that can still write has passed it.** For every
//!   device in `devices` other than this replica that is not revoked, or
//!   was revoked less than a retention window ago (the relay still holding
//!   what it uploaded before), some version `q` with `p ≺ q` is
//!   **acknowledged** by it: it wrote `q`, or its published frontier in
//!   `q`'s stream reaches `q`; and this replica has applied that device's
//!   own prefix in `q`'s stream as far as `q`, or as far as that frontier
//!   states it. A device with an op this replica holds unapplied (waiting
//!   for its key, parked, or held in a group) passes nothing. This replica
//!   passes every version that is not a head, since it names only heads.
//!
//! # Why the fold does not change
//!
//! Take a version `x` that this replica applies after the prune, written by
//! a device `d`. By (e), `d` acknowledged a descendant `q` of `p` before
//! writing `x`, or wrote `x` before that and `x` is in `q`'s stream below
//! the prefix this replica holds, and so was applied already. Having
//! applied `q`, `d` did not hold `p` as a head, so `x` does not name `p`.
//!
//! Nor is `p` the previous version of `x`'s device (D2). If `x` is newer
//! than `d`'s newest version here, that newest is its previous one, and (b)
//! keeps it. If it is older, and `p` were its previous one, `p`'s next
//! version here would be newer than `x` and compacted in `p`'s stream (c),
//! so `x`, sealed before it in that stream, would be below its floor and
//! applied already.
//!
//! Nor is `x` an ancestor of `p`. Through a parent edge, (d) has every
//! ancestor here or covered, and a covered op is never delivered again.
//! Through `p`'s device's own chain, an earlier version in `p`'s stream is
//! below `p`, which is covered, and so applied.
//!
//! So `x` reads no frontier but the ones still stored, its own frontier is
//! the one a replica that kept `p` computes, and every later comparison
//! reads the same frontiers there and here. A frontier entry names a stamp,
//! not a row, so the frontiers kept still reach what `p` reached. The heads,
//! the registers, their losers and the conflicts are functions of those
//! frontiers and of the rows (b) keeps, and are the same.
//!
//! A rebuild and a joiner are the same argument with (c): every op either
//! refolds is a version that does not read `p`.
//!
//! # What it does not cover
//!
//! The rule reads what this replica knows of the devices that can write,
//! and three things it cannot know about:
//!
//! - a device paired by another, whose cert has not reached this replica;
//! - an op a device sealed in another stream than the one this rule read
//!   its acknowledgement or its chain in, before that point, and that this
//!   replica has not received at all;
//! - a writer that names a parent it did not hold as a head.
//!
//! A version of those that names a pruned parent folds as one naming a
//! parent never applied does: the parent contributes nothing to its
//! frontier. It then reads as concurrent with the writes it had seen
//! through that parent, and keeps them in the register's unresolved set
//! beside its own, where a replica that kept the parent lets it replace
//! them. No write is lost, and the next write that names the entity's
//! heads settles both replicas alike (§4).

use super::state::stamp_cols;
use super::versions::{forget, nodes, Node};
use super::{OpRef, Stamp};
use crate::engine::ids::blob16;
use rusqlite::{params, Transaction};
use std::collections::{BTreeMap, BTreeSet};

type Id = [u8; 16];

/// What this replica knows of the log and of the other devices, which rule
/// (a), (d) and (e) read. Built by compaction (`Engine::passage`).
#[derive(Debug, Clone, Default)]
pub(in crate::engine) struct Passage {
    /// `(stream, device)` to its compaction floor's seq.
    pub(in crate::engine) floors: BTreeMap<(Id, Id), u64>,
    /// `(stream, device)` to this replica's sync cursor.
    pub(in crate::engine) cursors: BTreeMap<(Id, Id), u64>,
    /// `(stream, peer, device)` to the seq of `device` that `peer`'s last
    /// published frontier in `stream` reaches.
    pub(in crate::engine) acks: BTreeMap<(Id, Id, Id), u64>,
    /// Every device other than this replica that must pass a version.
    pub(in crate::engine) writers: BTreeSet<Id>,
    /// The devices with an op this replica holds unapplied: they pass
    /// nothing.
    pub(in crate::engine) stuck: BTreeSet<Id>,
}

impl Passage {
    /// Whether `op` is at or below its prefix's floor.
    fn covered(&self, op: OpRef) -> bool {
        self.floors
            .get(&(op.stream, op.device))
            .is_some_and(|f| op.seq <= *f)
    }

    /// Whether `device` has acknowledged the version `q`, so that every op
    /// it sealed in `q`'s stream before that is applied here: rule (e).
    fn acknowledged(&self, device: &Id, q: OpRef) -> bool {
        if self.stuck.contains(device) {
            return false;
        }
        let held = self.cursors.get(&(q.stream, *device)).copied().unwrap_or(0);
        if q.device == *device {
            return held >= q.seq;
        }
        let reaches = self
            .acks
            .get(&(q.stream, *device, q.device))
            .is_some_and(|seq| *seq >= q.seq);
        let own = self
            .acks
            .get(&(q.stream, *device, *device))
            .copied()
            .unwrap_or(0);
        reaches && held >= own
    }
}

/// How far a set of versions reaches on one device: the greatest stamp, and
/// whether a version stamped exactly that is reached (a version does not
/// precede itself).
type Reach = BTreeMap<Id, (Stamp, bool)>;

fn widen(reach: &mut Reach, device: Id, at: Stamp, inclusive: bool) {
    match reach.get_mut(&device) {
        Some(held) if held.0 > at => {}
        Some(held) if held.0 == at => held.1 |= inclusive,
        _ => {
            reach.insert(device, (at, inclusive));
        }
    }
}

/// Whether some version whose reach is `reach` has `p` strictly below it.
fn below(reach: &Reach, p: Stamp) -> bool {
    reach
        .get(&p.device)
        .is_some_and(|(at, inclusive)| p < *at || (p == *at && *inclusive))
}

/// The versions of one entity that the rule passes, from its `nodes` in
/// stamp order, the stamps of its registers' winners and losers
/// (`members`), and the op-refs among its versions whose ops are compacted
/// (at or below their floor and gone from the log). See the module docs.
pub(in crate::engine) fn passed(
    nodes: &[Node],
    members: &BTreeSet<OpRef>,
    compacted: &BTreeSet<OpRef>,
    passage: &Passage,
) -> BTreeSet<OpRef> {
    let held: BTreeSet<OpRef> = nodes.iter().map(|n| n.at.op_ref()).collect();
    // (d): every parent named is here or covered.
    if nodes
        .iter()
        .flat_map(|n| &n.parents)
        .any(|p| !held.contains(p) && !passage.covered(*p))
    {
        return BTreeSet::new();
    }

    // Each version's readers (c): those that name it, and its device's next.
    let mut readers: BTreeMap<OpRef, Vec<OpRef>> = BTreeMap::new();
    let mut previous: BTreeMap<Id, OpRef> = BTreeMap::new();
    for n in nodes {
        let me = n.at.op_ref();
        for p in &n.parents {
            readers.entry(*p).or_default().push(me);
        }
        if let Some(prev) = previous.insert(n.at.device, me) {
            readers.entry(prev).or_default().push(me);
        }
    }
    // (b): each device's newest is the last `previous` holds.
    let newest: BTreeSet<OpRef> = previous.into_values().collect();
    let witness = nodes.iter().rev().find(|n| n.keys).map(|n| n.at.op_ref());

    // (e): how far the versions each writer acknowledged reach.
    let reach: Vec<Reach> = passage
        .writers
        .iter()
        .map(|device| {
            let mut reach = Reach::new();
            for q in nodes
                .iter()
                .filter(|q| passage.acknowledged(device, q.at.op_ref()))
            {
                for (d, at) in &q.frontier {
                    widen(&mut reach, *d, *at, *d != q.at.device);
                }
                widen(&mut reach, q.at.device, q.at, false);
            }
            reach
        })
        .collect();

    nodes
        .iter()
        .filter(|p| {
            let me = p.at.op_ref();
            !p.head
                && !newest.contains(&me)
                && !members.contains(&me)
                && witness != Some(me)
                && compacted.contains(&me)
                && readers.get(&me).is_none_or(|rs| {
                    rs.iter()
                        .all(|r| r.stream == me.stream && compacted.contains(r))
                })
                && reach.iter().all(|r| below(r, p.at))
        })
        .map(|p| p.at.op_ref())
        .collect()
}

/// The op-refs of every register winner and loser of `id`.
fn members(tx: &Transaction<'_>, id: &Id) -> rusqlite::Result<BTreeSet<OpRef>> {
    let mut stmt = tx.prepare(
        "SELECT stream, device, seq FROM merge_registers WHERE entity_id = ?1
         UNION SELECT stream, device, seq FROM register_losers WHERE entity_id = ?1",
    )?;
    let rows = stmt.query_map(params![&id[..]], |r| {
        Ok(OpRef {
            stream: blob16(&r.get::<_, Vec<u8>>(0)?),
            device: blob16(&r.get::<_, Vec<u8>>(1)?),
            seq: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
        })
    })?;
    rows.collect()
}

/// Whether the op at `op` is still in the log.
fn logged(tx: &Transaction<'_>, op: OpRef) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM ops WHERE stream_id = ? AND device_id = ? AND seq = ?)",
        params![&op.stream[..], &op.device[..], op.seq],
        |r| r.get(0),
    )
}

/// Prune every entity's passed versions. Returns how many versions went.
///
/// Only an entity with a version below a floor that is not a head can have
/// one, so only those are read.
pub(in crate::engine) fn prune_versions(
    tx: &Transaction<'_>,
    passage: &Passage,
) -> rusqlite::Result<u64> {
    let ids: Vec<Id> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT v.entity_id FROM entity_versions v
             JOIN compaction_floor f ON f.stream_id = v.stream AND f.device_id = v.device
             WHERE v.is_head = 0 AND v.seq <= f.seq
             ORDER BY v.entity_id",
        )?;
        let rows = stmt.query_map([], |r| Ok(blob16(&r.get::<_, Vec<u8>>(0)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut gone = 0;
    for id in ids {
        let nodes = nodes(tx, &id)?;
        let mut compacted = BTreeSet::new();
        for n in &nodes {
            let op = n.at.op_ref();
            if passage.covered(op) && !logged(tx, op)? {
                compacted.insert(op);
            }
        }
        let pass = passed(&nodes, &members(tx, &id)?, &compacted, passage);
        if !pass.is_empty() {
            gone += forget(tx, &id, &pass)?;
        }
    }
    Ok(gone)
}

/// The group members of `stream` this replica folded whose versions it has
/// pruned: what a joiner of the stream's `doc_state` is told is present
/// (D14), since no version of theirs travels any more. Each is
/// `(group id, size, stamp)`, in stamp order.
pub(super) fn pruned_members(
    tx: &Transaction<'_>,
    stream: &Id,
) -> rusqlite::Result<Vec<(Id, u64, Stamp)>> {
    let mut stmt = tx.prepare(
        "SELECT g.group_id, g.group_size, g.hlc_ms, g.hlc_logical, g.device, g.seq, g.stream
         FROM pending_groups g
         WHERE g.stream = ?1 AND g.applied = 1
           AND NOT EXISTS (SELECT 1 FROM entity_versions v
                           WHERE v.stream = g.stream AND v.device = g.device AND v.seq = g.seq)
         ORDER BY g.hlc_ms, g.hlc_logical, g.device, g.seq, g.group_id, g.group_size",
    )?;
    let rows = stmt.query_map(params![&stream[..]], |r| {
        let at = stamp_cols(r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
        Ok((
            blob16(&r.get::<_, Vec<u8>>(0)?),
            u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            at,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, size, at) = row?;
        if let Some(at) = at {
            out.push((id, size, at));
        }
    }
    Ok(out)
}
