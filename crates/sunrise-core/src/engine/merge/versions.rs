//! The version DAG of ADR-0065 §1–§2: every applied op of a mergeable entity
//! is a version, and an entity's versions are ordered by the parents they
//! name (migration 0039's `entity_versions` and `entity_frontier`).
//!
//! # The order
//!
//! A version's *frontier* `F(v)` maps each device to the greatest stamp of
//! that device's versions of the entity that `v` reaches, `v` itself
//! included. `v` reaches what its `parents` name and, by clarification D2,
//! the same device's previous version: a device's own ops are a chain
//! (ADR-0043), so two consecutive full-state edits from one device are
//! ordered although neither names the other. A parent whose stamp is not
//! below the child's is ignored (D3), so every edge points to a strictly
//! smaller stamp and the order is acyclic.
//!
//! `a` precedes `b` exactly when `a ≠ b` and `F(b)` at `a`'s device is at
//! least `a`'s stamp. Two versions neither of which precedes the other are
//! concurrent. A version with no `parents` key, which is every legacy
//! full-state op, is so concurrent with every version of another device
//! that does not name it.
//!
//! An entity's *heads* are its versions no other version precedes. A
//! device's later version always precedes its earlier one, so the heads are
//! drawn from each device's latest version.
//!
//! # Why arrival order does not matter
//!
//! `F` is defined by recursion over the edges, each of which points to a
//! smaller stamp, so it is one function of the set of versions applied. A
//! version folded after the versions that reach it is computed from their
//! frontiers, which are already final. A version that arrives *late*, after
//! a version that names it as a parent or after a later version of its own
//! device, makes those frontiers stale: [`admit_version`] then recomputes
//! every frontier of the entity in stamp order, one pass, since no edge
//! points up ([`recompute`]). So after each admission the stored frontiers
//! and heads are exactly those of the versions applied, whatever order they
//! came in, and a projection rebuild, which folds in stamp order, lands on
//! the same rows.
//!
//! A parent that has not been applied contributes nothing until it is. That
//! under-approximates `F`, and only ever omits an order, never invents one.
//!
//! The register fold reads the order ([`super::register`]): two writes of
//! one field that neither precedes the other are a conflict, decided by the
//! field's class (ADR-0065 §3), and the heads tell the conflicts query which
//! branches never saw a delete (§4).

use super::state::{enc, hlc_of};
use super::{OpRef, Stamp};
use crate::engine::ids::blob16;
use ciborium::value::Value;
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};

/// For each device, the greatest stamp of its versions a version reaches.
pub(super) type Frontier = BTreeMap<[u8; 16], Stamp>;

/// Whether an op is a full-state op or a `Patch` (`entity_versions.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VersionKind {
    /// A full-state `*Create`, `*Update` or `*Delete` (ADR-0044 §7).
    Legacy,
    /// A `Patch` (ADR-0044 §1).
    Patch,
}

impl VersionKind {
    const fn column(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Patch => "patch",
        }
    }
}

/// One version as the fold hands it over: what the op said about itself.
#[derive(Debug, Clone)]
pub(super) struct Version {
    /// The op's merge stamp; its op-ref is the version's id.
    pub(super) at: Stamp,
    pub(super) kind: VersionKind,
    /// `user` or `generated`, as the merge reads it.
    pub(super) origin: &'static str,
    /// A `*Create` op, or a `Patch` with `create`.
    pub(super) create: bool,
    /// The `parents` the op carried; `None` when it carried no key.
    pub(super) parents: Option<Vec<OpRef>>,
    /// The op's group, `(id, size)`.
    pub(super) group: Option<([u8; 16], u64)>,
    /// The names of the fields it wrote.
    pub(super) fields: BTreeSet<String>,
}

/// What [`admit_version`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Admission {
    /// The version was new. A version admitted before is left as it is.
    pub(super) admitted: bool,
    /// It arrived after a version that reaches it, and the entity's
    /// frontiers and heads were recomputed.
    pub(super) refined: bool,
}

// ---- reads ----

fn load_stamp(r: &rusqlite::Row<'_>, from: usize, op: OpRef) -> rusqlite::Result<Stamp> {
    Ok(Stamp {
        hlc: hlc_of(r.get(from)?, r.get(from + 1)?),
        device: op.device,
        seq: op.seq,
        stream: op.stream,
    })
}

/// The stamp of `op`, when it is an applied version of `id`.
fn stamp_of(tx: &Transaction<'_>, id: &[u8; 16], op: OpRef) -> rusqlite::Result<Option<Stamp>> {
    tx.query_row(
        "SELECT hlc_ms, hlc_logical FROM entity_versions
         WHERE entity_id = ? AND stream = ? AND device = ? AND seq = ?",
        params![&id[..], &op.stream[..], &op.device[..], op.seq],
        |r| load_stamp(r, 0, op),
    )
    .optional()
}

/// `F(v)` as stored: empty for a version that is not applied.
pub(super) fn frontier_of(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    v: OpRef,
) -> rusqlite::Result<Frontier> {
    let mut stmt = tx.prepare(
        "SELECT device, hlc_ms, hlc_logical, seq, stream FROM entity_frontier
         WHERE entity_id = ? AND v_stream = ? AND v_device = ? AND v_seq = ?",
    )?;
    let rows = stmt.query_map(params![&id[..], &v.stream[..], &v.device[..], v.seq], |r| {
        let op = OpRef {
            device: blob16(&r.get::<_, Vec<u8>>(0)?),
            seq: u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
            stream: blob16(&r.get::<_, Vec<u8>>(4)?),
        };
        Ok((op.device, load_stamp(r, 1, op)?))
    })?;
    rows.collect()
}

/// `F(v)` at one device: the greatest stamp of that device's versions `v`
/// reaches.
fn frontier_at(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    v: OpRef,
    device: &[u8; 16],
) -> rusqlite::Result<Option<Stamp>> {
    tx.prepare(
        "SELECT hlc_ms, hlc_logical, seq, stream FROM entity_frontier
         WHERE entity_id = ? AND v_stream = ? AND v_device = ? AND v_seq = ? AND device = ?",
    )?
    .query_row(
        params![&id[..], &v.stream[..], &v.device[..], v.seq, &device[..]],
        |r| {
            let op = OpRef {
                device: *device,
                seq: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                stream: blob16(&r.get::<_, Vec<u8>>(3)?),
            };
            load_stamp(r, 0, op)
        },
    )
    .optional()
}

/// The version of `id` by `at`'s device with the greatest stamp below `at`.
fn previous_of_device(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    at: Stamp,
) -> rusqlite::Result<Option<OpRef>> {
    tx.prepare(
        "SELECT stream, seq FROM entity_versions
         WHERE entity_id = ?1 AND device = ?2
           AND (hlc_ms, hlc_logical, seq, stream) < (?3, ?4, ?5, ?6)
         ORDER BY hlc_ms DESC, hlc_logical DESC, seq DESC, stream DESC LIMIT 1",
    )?
    .query_row(
        params![
            &id[..],
            &at.device[..],
            at.hlc.physical_ms,
            at.hlc.logical,
            at.seq,
            &at.stream[..],
        ],
        |r| {
            Ok(OpRef {
                stream: blob16(&r.get::<_, Vec<u8>>(0)?),
                device: at.device,
                seq: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            })
        },
    )
    .optional()
}

/// `[* [stream, device, seq]]`, as `entity_versions.parents` stores it.
fn decode_parents(bytes: &[u8]) -> Vec<OpRef> {
    super::state::dec(bytes)
        .as_array()
        .map(|items| items.iter().filter_map(OpRef::from_value).collect())
        .unwrap_or_default()
}

fn encode_parents(parents: &[OpRef]) -> rusqlite::Result<Vec<u8>> {
    enc(&Value::Array(
        parents.iter().map(|p| p.to_value()).collect::<Vec<_>>(),
    ))
}

/// Whether a version applied after `at`, by `at`'s device or naming `at` as
/// a parent, could have been computed without it: `at` arrived late.
fn arrived_late(tx: &Transaction<'_>, id: &[u8; 16], at: Stamp) -> rusqlite::Result<bool> {
    let me = at.op_ref();
    let mut stmt = tx.prepare(
        "SELECT device, parents FROM entity_versions
         WHERE entity_id = ?1
           AND (hlc_ms, hlc_logical, device, seq, stream) > (?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut rows = stmt.query(params![
        &id[..],
        at.hlc.physical_ms,
        at.hlc.logical,
        &at.device[..],
        at.seq,
        &at.stream[..],
    ])?;
    while let Some(r) = rows.next()? {
        if blob16(&r.get::<_, Vec<u8>>(0)?) == at.device {
            return Ok(true);
        }
        if let Some(parents) = r.get::<_, Option<Vec<u8>>>(1)? {
            if decode_parents(&parents).contains(&me) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// ---- the order ----

/// Join `from` into `into`, device by device, keeping the greater stamp.
fn join(into: &mut Frontier, from: &Frontier) {
    for (device, at) in from {
        into.entry(*device)
            .and_modify(|held| *held = (*held).max(*at))
            .or_insert(*at);
    }
}

/// Whether the version stamped `a` precedes one whose frontier is `f`.
fn reached_by(a: Stamp, f: &Frontier) -> bool {
    f.get(&a.device).is_some_and(|reached| *reached >= a)
}

// ---- writes ----

fn insert_version(tx: &Transaction<'_>, id: &[u8; 16], v: &Version) -> rusqlite::Result<()> {
    let parents = v.parents.as_deref().map(encode_parents).transpose()?;
    let fields = enc(&Value::Array(
        v.fields.iter().map(|f| Value::Text(f.clone())).collect(),
    ))?;
    let (group_id, group_size) = match v.group {
        Some((gid, size)) => (Some(gid.to_vec()), Some(size)),
        None => (None, None),
    };
    tx.prepare(
        "INSERT INTO entity_versions
         (entity_id, stream, device, seq, hlc_ms, hlc_logical, kind, origin, is_create,
          parents, group_id, group_size, fields, is_head)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
    )?
    .execute(params![
        &id[..],
        &v.at.stream[..],
        &v.at.device[..],
        v.at.seq,
        v.at.hlc.physical_ms,
        v.at.hlc.logical,
        v.kind.column(),
        v.origin,
        i64::from(v.create),
        parents,
        group_id,
        group_size,
        fields,
    ])?;
    Ok(())
}

fn insert_frontier(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    v: OpRef,
    f: &Frontier,
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO entity_frontier
         (entity_id, v_stream, v_device, v_seq, device, hlc_ms, hlc_logical, seq, stream)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    for (device, at) in f {
        stmt.execute(params![
            &id[..],
            &v.stream[..],
            &v.device[..],
            v.seq,
            &device[..],
            at.hlc.physical_ms,
            at.hlc.logical,
            at.seq,
            &at.stream[..],
        ])?;
    }
    Ok(())
}

fn set_head(tx: &Transaction<'_>, id: &[u8; 16], v: OpRef, head: bool) -> rusqlite::Result<()> {
    tx.prepare(
        "UPDATE entity_versions SET is_head = ?
         WHERE entity_id = ? AND stream = ? AND device = ? AND seq = ?",
    )?
    .execute(params![
        i64::from(head),
        &id[..],
        &v.stream[..],
        &v.device[..],
        v.seq
    ])?;
    Ok(())
}

/// The current heads of `id`, with their stamps.
fn head_stamps(tx: &Transaction<'_>, id: &[u8; 16]) -> rusqlite::Result<Vec<Stamp>> {
    let mut stmt = tx.prepare(
        "SELECT stream, device, seq, hlc_ms, hlc_logical FROM entity_versions
         WHERE entity_id = ? AND is_head = 1",
    )?;
    let rows = stmt.query_map(params![&id[..]], |r| {
        let op = OpRef {
            stream: blob16(&r.get::<_, Vec<u8>>(0)?),
            device: blob16(&r.get::<_, Vec<u8>>(1)?),
            seq: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
        };
        load_stamp(r, 3, op)
    })?;
    rows.collect()
}

/// Admit the version `v` of entity `id`: store it, its frontier, and the
/// entity's heads with it. Idempotent: a version already admitted is left
/// as it is. See the [module docs](self) for the order and why the result
/// does not depend on arrival order.
pub(super) fn admit_version(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    v: &Version,
) -> rusqlite::Result<Admission> {
    let me = v.at.op_ref();
    if stamp_of(tx, id, me)?.is_some() {
        return Ok(Admission {
            admitted: false,
            refined: false,
        });
    }
    if arrived_late(tx, id, v.at)? {
        insert_version(tx, id, v)?;
        recompute(tx, id)?;
        return Ok(Admission {
            admitted: true,
            refined: true,
        });
    }

    // Every version `v` reaches is applied before it, so each frontier read
    // here is final.
    let mut f = Frontier::from([(v.at.device, v.at)]);
    for parent in v.parents.iter().flatten() {
        // D3: an edge that does not point to a smaller stamp is ignored.
        if stamp_of(tx, id, *parent)?.is_some_and(|p| p < v.at) {
            join(&mut f, &frontier_of(tx, id, *parent)?);
        }
    }
    // D2: the same device's previous version.
    if let Some(previous) = previous_of_device(tx, id, v.at)? {
        join(&mut f, &frontier_of(tx, id, previous)?);
    }
    insert_version(tx, id, v)?;
    insert_frontier(tx, id, me, &f)?;

    // `v` is a head: a version that reached it would have a greater stamp
    // and name it, or be its device's later version, and `v` would have
    // arrived late. The other heads are the old ones it does not reach,
    // since every other version is below an old head and `v`'s arrival
    // changed no frontier.
    for h in head_stamps(tx, id)?.iter().filter(|h| reached_by(**h, &f)) {
        set_head(tx, id, h.op_ref(), false)?;
    }
    set_head(tx, id, me, true)?;
    Ok(Admission {
        admitted: true,
        refined: false,
    })
}

/// Recompute every frontier and the heads of `id` from its versions alone.
///
/// One pass in stamp order is exact: every edge points to a smaller stamp, so
/// each version's parents and previous version are final when it is reached.
/// Each frontier starts from the one stored, which is never more than the
/// exact one and may reach versions this replica does not hold, when a
/// snapshot carried it. Called when a version arrived late, and after a
/// snapshot join brought versions in.
pub(in crate::engine) fn recompute(tx: &Transaction<'_>, id: &[u8; 16]) -> rusqlite::Result<()> {
    struct Row {
        at: Stamp,
        parents: Vec<OpRef>,
        head: bool,
    }
    let rows: Vec<Row> = {
        let mut stmt = tx.prepare(
            "SELECT stream, device, seq, hlc_ms, hlc_logical, parents, is_head
             FROM entity_versions WHERE entity_id = ?
             ORDER BY hlc_ms, hlc_logical, device, seq, stream",
        )?;
        let rows = stmt.query_map(params![&id[..]], |r| {
            let op = OpRef {
                stream: blob16(&r.get::<_, Vec<u8>>(0)?),
                device: blob16(&r.get::<_, Vec<u8>>(1)?),
                seq: u64::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
            };
            Ok(Row {
                at: load_stamp(r, 3, op)?,
                parents: r
                    .get::<_, Option<Vec<u8>>>(5)?
                    .as_deref()
                    .map(decode_parents)
                    .unwrap_or_default(),
                head: r.get::<_, i64>(6)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    let index: BTreeMap<OpRef, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.at.op_ref(), i))
        .collect();
    // What is stored already is a floor: a frontier only ever grows, and
    // one a joined snapshot carried reaches versions this replica may not
    // hold (ADR-0065 §8).
    let mut stored: BTreeMap<OpRef, Frontier> = BTreeMap::new();
    for row in &rows {
        let op = row.at.op_ref();
        stored.insert(op, frontier_of(tx, id, op)?);
    }
    let mut frontiers: Vec<Frontier> = Vec::with_capacity(rows.len());
    let mut latest: BTreeMap<[u8; 16], usize> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let mut f = stored.remove(&row.at.op_ref()).unwrap_or_default();
        join(&mut f, &Frontier::from([(row.at.device, row.at)]));
        for parent in &row.parents {
            if let Some(&p) = index.get(parent) {
                if rows[p].at < row.at {
                    join(&mut f, &frontiers[p]);
                }
            }
        }
        if let Some(&previous) = latest.get(&row.at.device) {
            join(&mut f, &frontiers[previous]);
        }
        latest.insert(row.at.device, i);
        frontiers.push(f);
    }

    tx.execute(
        "DELETE FROM entity_frontier WHERE entity_id = ?",
        params![&id[..]],
    )?;
    for (row, f) in rows.iter().zip(&frontiers) {
        insert_frontier(tx, id, row.at.op_ref(), f)?;
    }
    // A device's later version precedes its earlier ones, so only each
    // device's latest can be a head, and it is one unless another device's
    // latest reaches it.
    let heads: BTreeSet<usize> = latest
        .values()
        .copied()
        .filter(|&c| {
            !latest
                .values()
                .any(|&d| d != c && reached_by(rows[c].at, &frontiers[d]))
        })
        .collect();
    for (i, row) in rows.iter().enumerate() {
        let head = heads.contains(&i);
        if head != row.head {
            set_head(tx, id, row.at.op_ref(), head)?;
        }
    }
    Ok(())
}

/// The heads of `id` as op-refs, sorted: the `parents` a writer names
/// (ADR-0065 §2).
pub(in crate::engine) fn heads(
    tx: &Transaction<'_>,
    id: &[u8; 16],
) -> rusqlite::Result<Vec<OpRef>> {
    let mut heads: Vec<OpRef> = head_stamps(tx, id)?.iter().map(Stamp::op_ref).collect();
    heads.sort();
    Ok(heads)
}

/// Whether version `a` of `id` precedes version `b`. `false` when either is
/// not an applied version of `id`.
pub(in crate::engine) fn precedes(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    a: OpRef,
    b: OpRef,
) -> rusqlite::Result<bool> {
    if a == b || stamp_of(tx, id, b)?.is_none() {
        return Ok(false);
    }
    let Some(at) = stamp_of(tx, id, a)? else {
        return Ok(false);
    };
    Ok(frontier_at(tx, id, b, &a.device)?.is_some_and(|r| r >= at))
}

/// One head of an entity, as the conflicts query reports a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::engine) struct HeadVersion {
    pub(in crate::engine) at: Stamp,
    /// The fields it wrote, sorted.
    pub(in crate::engine) fields: Vec<String>,
}

/// The heads of `id` that `d` does not precede: the branches whose writer had
/// not applied `d` (ADR-0065 §4, "edited while deleted" when `d` is the
/// winning delete), oldest first. `d` itself is left out, and so is a head
/// below `floor` or written by a full-state op, which D4 never surfaces.
pub(in crate::engine) fn unmerged_heads(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    d: Stamp,
    floor: Option<Stamp>,
) -> rusqlite::Result<Vec<HeadVersion>> {
    let mut out = Vec::new();
    for h in heads(tx, id)? {
        if h == d.op_ref() || precedes(tx, id, d.op_ref(), h)? {
            continue;
        }
        let row: Option<(i64, i64, String, Vec<u8>)> = tx
            .prepare(
                "SELECT hlc_ms, hlc_logical, kind, fields FROM entity_versions
                 WHERE entity_id = ? AND stream = ? AND device = ? AND seq = ?",
            )?
            .query_row(params![&id[..], &h.stream[..], &h.device[..], h.seq], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((ms, logical, kind, fields)) = row else {
            continue;
        };
        let at = Stamp {
            hlc: hlc_of(ms, logical),
            device: h.device,
            seq: h.seq,
            stream: h.stream,
        };
        if kind != VersionKind::Patch.column() || floor.is_some_and(|l| at < l) {
            continue;
        }
        let fields = super::state::dec(&fields)
            .as_array()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_text().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        out.push(HeadVersion { at, fields });
    }
    out.sort_by_key(|h| h.at);
    Ok(out)
}
