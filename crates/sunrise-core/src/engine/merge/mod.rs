//! Per-field merge (ADR-0044): every field of an entity merges by its own CRDT
//! type, and the entity row is the read projection of that state.
//!
//! [`merge_member`] is the one entry point. It folds one op into the entity's
//! field state (migration 0033's `merge_*` tables) and then writes the merged
//! entity back to its row. The row writers are private to this module
//! (`rows`): a row is only ever the fold's output, for a local command's op
//! as for a received one (ADR-0065 §5).
//!
//! # The four field types
//!
//! | Type | State | Value |
//! |---|---|---|
//! | register | the concurrent `(value, stamp, origin)` writes | the one its conflict class ranks highest ([`register`]) |
//! | map | one register per key | every key whose register is not null |
//! | OR-set | `(element, tag)` adds and removed `(element, tag)` pairs | every element with an add whose tag is not removed |
//! | counter | the `inc` deltas, once per op | the sum, plus the legacy base |
//!
//! A stamp is the envelope's `(hlc, device, seq)`, with the op's stream last:
//! `seq` counts per `(stream, device)`, so the stream is what tells apart two
//! ops one device made in two streams. An OR-set add's tag is the op-ref
//! `(stream, device, seq)`, which the replay invariant makes unique per op.
//!
//! # Legacy full-state ops (ADR-0044 §7)
//!
//! A `TaskUpdate` and its siblings carry the whole entity. The merge reads one
//! as a write, at its own stamp, to every field this build registers for the
//! entity, and to every unknown key it carries. A registered field the op
//! leaves out (serde skips an empty list, a `None`) is written as *absent*,
//! which reads as the field's default. Every rule that involves a legacy op is
//! evaluated against `L`, the greatest stamp among the legacy ops applied to
//! the entity:
//!
//! - a register, or a map key, stamped below `L` reads as absent;
//! - an OR-set add stamped below `L` reads as removed, and `L` re-adds every
//!   element of its own value under its own tag;
//! - a counter reads as the base `L` carried, plus every `inc` stamped after
//!   `L`.
//!
//! So a history made only of legacy ops projects exactly as entity-level
//! last-writer-wins did, whatever order its ops arrive in, and only `L`
//! matters.
//!
//! The OR-set rule removes *every* add below `L`, including an add of an
//! element `L` itself carries, where ADR-0044 §7 removes only the adds of
//! elements `L` does not carry. The value is the same either way, because `L`
//! re-adds its own elements. What differs is what a later `remove` that
//! observed only `L`'s tag does, and only under this rule is a replica that
//! never saw the older tags (one that seeded an entity from its row, before
//! the rebuild at open replaced that) in agreement with one that replayed
//! every op. `docs/05-sync/crdt-design.md` records it.
//!
//! What the floor makes unreadable is deleted when the floor rises: the adds
//! below it, the removes that named them, and the counter deltas at or below
//! it (`state::prune_below_floor`). A set written only by full-state ops so
//! holds one add per element, not one per element per op.
//!
//! # Local writes, and rows written another way
//!
//! A command appends its op to the log and folds it here, through the call a
//! received op takes (`Engine::append_and_fold`), so a writer and its peers
//! derive the row from one function of the same ops.
//!
//! Two kinds of row were written another way: an entity a vault held before
//! migration 0033 has a row and no field state, and a build before ADR-0065
//! PR 3 wrote a command's row directly and left folding it to the next op.
//! Neither is read back here. A vault that may hold one is rebuilt from its
//! op log before anything folds into it (`Engine::rebuild_if_stale`, which
//! `Core::open` runs whenever the vault was last rebuilt at a [`FOLD_V`]
//! other than this build's), and the log holds the op behind every such row.
//!
//! # Versions
//!
//! Every op folded here is first admitted as a version of its entity
//! ([`versions`], ADR-0065 §1–§2): its parents, its causal frontier, and the
//! entity's heads. The register fold reads them ([`register`], §3–§4): a
//! register keeps the writes no other write descends from, picks its winner
//! by the field's conflict class, and keeps the rest as losers, which
//! `Query::Conflicts` reports. Two full-state ops are concurrent, but only the
//! newer survives the legacy floor, so a legacy-only history keeps no loser.
//!
//! Compaction prunes the versions no fold can read again ([`prune`],
//! ADR-0065 D25): those every device that can still write has passed, whose
//! ops and readers are folded away, and that no head, register or projection
//! reads.
//!
//! # Visibility, delete, and derived fields
//!
//! An entity is projected only once a create has been applied: a `Patch` with
//! `create`, or any legacy op, since every legacy op wrote a whole row.
//! `deleted` is a register of the tombstone class: a delete beats a
//! concurrent restore, and an edit concurrent with a delete is kept inside the
//! tombstoned entity and shows again on a restore (ADR-0044 §5, ADR-0065 §3). `created_at` is the legacy register where one wrote it, and
//! the least create stamp's physical time otherwise; `updated_at` is the later
//! of the legacy register and the newest `Patch`'s physical time (§10). A
//! `Patch` may write neither, nor `id`, nor a field the registry declares
//! derived or nested.

mod patch;
mod project;
mod prune;
mod register;
mod rows;
mod snapshot;
mod state;
mod versions;

pub(super) use patch::{
    check_patch, check_register_set, round_trip, set_field, FieldOp, PatchProblem,
};
pub(super) use project::field_default;
#[cfg(test)]
pub(super) use project::map_over_whole;
#[cfg(test)]
pub(super) use prune::passed;
pub(super) use prune::{prune_versions, Passage};
#[cfg(test)]
pub(super) use versions::Node;
// The search indexes' rows are the row writers'; `engine::search` builds them
// from the rows when the layout changes.
pub(super) use register::{class_of, kept_registers, Member};
pub(super) use rows::{create_words_sql, rebuild_from_rows as rebuild_search_from_rows};
pub(super) use snapshot::{dump_compacted_base, dump_stream_state, join_stream_state};
pub(super) use state::{dec as decode_value, enc as encode_value, stamp_cols as stamp_of_columns};

/// The heads of entity `id` as this replica has applied them, in the wire
/// form a `Patch`'s `parents` carries (ADR-0065 §2): what a writer that has
/// seen every version names.
pub(super) fn parents_of(
    tx: &Transaction<'_>,
    id: &[u8; 16],
) -> rusqlite::Result<Vec<crate::inner_op::OpRefWire>> {
    // A head at seq 0 is no op (the chain's empty position): only a row
    // folded without an op is one, and no `Patch` may name it.
    Ok(versions::heads(tx, id)?
        .into_iter()
        .filter(|h| h.seq != 0)
        .map(|h| crate::inner_op::OpRefWire(h.stream, h.device, h.seq))
        .collect())
}
pub(super) use versions::unmerged_heads;
#[cfg(test)]
pub(super) use versions::{heads, precedes};

/// The version of the fold: what a projection rebuilt from the log holds.
///
/// It moves when a build folds the same ops into different rows or merge
/// state, and a vault last rebuilt at another version is rebuilt once, at
/// open (`Engine::rebuild_if_stale`). `1` adds the version DAG (ADR-0065
/// §1–§2), whose tables a vault before migration 0039 does not have. `2`
/// folds registers by their conflict classes (§3) and keeps the losers (§4),
/// which a vault folded at `1` holds none of. `3` writes every
/// `SunriseTime`'s civil column and the routine and stream time sidecars
/// (migration 0041, #506), which no row folded before it holds. `4` writes
/// `Task.sort_order` to its own column (migration 0042, #340), where a build
/// before it kept a key a newer device wrote in `extra`, as an unknown field.
/// `5` derives a routine's streak fields from its keys once a `Patch` has
/// written them (ADR-0065 §5, #331), where a routine folded at `4` holds the
/// registers.
pub(crate) const FOLD_V: u32 = 5;

use super::ids::blob16;
use super::lww::LwwStamp;
use super::EngineError;
use crate::inner_op::{InnerOp, FULL_STATE_GROUP_KEY};
use ciborium::value::Value;
use project::{project, write_projection};
use register::Registers;
use rusqlite::Transaction;
use state::{
    add_delta, add_element, prune_below_floor, read_meta, remove_element, write_map_entry,
    write_meta, Meta,
};
use std::collections::BTreeSet;
use sunrise_cbor::hlc::Hlc;
use sunrise_id::registry::{Crdt, EntitySpec};
use sunrise_id::EntityRef;
use versions::{admit_version, Admission, Version, VersionKind};

// ---- stamps and op-refs ----

/// The merge's comparison key: `(hlc, device, seq, stream)`, in that order.
///
/// The first three are [`LwwStamp`]'s, compared as `lww_wins` always compared
/// them: a higher device id breaks a cross-device tie, and a device's own ops
/// are ordered by `seq`. The stream is last, and only separates two ops one
/// device made in two streams with an equal clock reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Stamp {
    pub(super) hlc: Hlc,
    pub(super) device: [u8; 16],
    pub(super) seq: u64,
    pub(super) stream: [u8; 16],
}

impl Stamp {
    pub(super) const fn of(lww: &LwwStamp, stream: [u8; 16]) -> Self {
        Self {
            hlc: lww.hlc,
            device: lww.device,
            seq: lww.seq,
            stream,
        }
    }

    const fn lww(&self) -> LwwStamp {
        LwwStamp {
            hlc: self.hlc,
            device: self.device,
            seq: self.seq,
        }
    }

    pub(super) const fn op_ref(&self) -> OpRef {
        OpRef {
            stream: self.stream,
            device: self.device,
            seq: self.seq,
        }
    }
}

/// An op's identity, `(stream_id, device_id, seq)`: the tag of an OR-set add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct OpRef {
    pub(super) stream: [u8; 16],
    pub(super) device: [u8; 16],
    pub(super) seq: u64,
}

impl OpRef {
    /// `[bstr .size 16, bstr .size 16, uint]`.
    pub(super) fn from_value(v: &Value) -> Option<Self> {
        let items = v.as_array()?;
        let [stream, device, seq] = items.as_slice() else {
            return None;
        };
        let bytes16 = |v: &Value| -> Option<[u8; 16]> {
            let b = v.as_bytes()?;
            (b.len() == 16).then(|| blob16(b))
        };
        Some(Self {
            stream: bytes16(stream)?,
            device: bytes16(device)?,
            seq: u64::try_from(seq.as_integer()?).ok()?,
        })
    }

    /// The CBOR form [`Self::from_value`] reads.
    pub(super) fn to_value(self) -> Value {
        Value::Array(vec![
            Value::Bytes(self.stream.to_vec()),
            Value::Bytes(self.device.to_vec()),
            Value::Integer(self.seq.into()),
        ])
    }
}

// ---- folds ----

const USER: &str = "user";
pub(super) const GENERATED: &str = "generated";

/// The origin of a legacy full-state op, from its op-log `inner_kind` and its
/// payload's CBOR map. ADR-0044 §3: a legacy op is a user write, except the
/// routine engine's own occurrence creates, a `task.create` that names its
/// routine.
fn legacy_origin(inner_kind: &str, state: &[(Value, Value)]) -> &'static str {
    let names_routine = state
        .iter()
        .any(|(k, v)| k.as_text() == Some("routine_id") && !v.is_null());
    if inner_kind == "task.create" && names_routine {
        GENERATED
    } else {
        USER
    }
}

/// Fold a legacy full-state op, given as its payload's CBOR map (§7).
#[allow(clippy::too_many_arguments)]
fn fold_legacy(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    id: &[u8; 16],
    state: &[(Value, Value)],
    at: Stamp,
    origin: &str,
    meta: &mut Meta,
    regs: &mut Registers,
) -> rusqlite::Result<()> {
    meta.note(at);
    meta.note_create(at);
    if meta.legacy.is_none_or(|l| at > l) {
        meta.legacy = Some(at);
        prune_below_floor(tx, id, at, regs)?;
    }
    let floor = meta.legacy;
    let Some(record) = spec.records.first() else {
        return Ok(());
    };
    let get = |entries: &'_ [(Value, Value)], name: &str| -> Option<Value> {
        entries
            .iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, v)| v.clone())
    };
    for field in record.fields {
        let value = get(state, field.name);
        match field.crdt {
            Crdt::Register | Crdt::Derived | Crdt::Counter => {
                regs.offer(tx, field.name, value.as_ref(), at, origin)?;
            }
            Crdt::OrSet => {
                for element in value
                    .as_ref()
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add_element(tx, id, field.name, element, at, floor)?;
                }
            }
            Crdt::Map => {
                for (k, v) in value.as_ref().and_then(Value::as_map).into_iter().flatten() {
                    if let Some(k) = k.as_text() {
                        write_map_entry(tx, id, field.name, k, v, at, origin)?;
                    }
                }
            }
            Crdt::Nested => {
                let inner: &[(Value, Value)] = value
                    .as_ref()
                    .and_then(Value::as_map)
                    .map_or(&[], Vec::as_slice);
                let sub = spec.record(field.value_type);
                for sub_field in sub.map_or(&[][..], |s| s.fields) {
                    let name = format!("{}.{}", field.name, sub_field.name);
                    regs.offer(tx, &name, get(inner, sub_field.name).as_ref(), at, origin)?;
                }
                for (k, v) in inner {
                    let Some(k) = k.as_text() else { continue };
                    if sub.is_some_and(|s| s.fields.iter().any(|f| f.name == k)) {
                        continue;
                    }
                    regs.offer(tx, &format!("{}.{k}", field.name), Some(v), at, origin)?;
                }
            }
        }
    }
    // Every key a newer build wrote, as a register under its own name. A
    // group's mark is the op's, not the entity's (D13), and is not folded.
    for (k, v) in state {
        let Some(k) = k.as_text() else { continue };
        if k == FULL_STATE_GROUP_KEY || record.fields.iter().any(|f| f.name == k) {
            continue;
        }
        regs.offer(tx, k, Some(v), at, origin)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fold_patch(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    create: bool,
    ops: &[(String, FieldOp)],
    at: Stamp,
    origin: &str,
    meta: &mut Meta,
    regs: &mut Registers,
) -> rusqlite::Result<()> {
    meta.note(at);
    if create {
        meta.note_create(at);
    }
    meta.patch_ms = Some(
        meta.patch_ms
            .map_or(at.hlc.physical_ms, |ms| ms.max(at.hlc.physical_ms)),
    );
    for (field, op) in ops {
        match op {
            FieldOp::Set(v) => regs.offer(tx, field, Some(v), at, origin)?,
            FieldOp::Edit { add, remove } => {
                for element in add {
                    add_element(tx, id, field, element, at, meta.legacy)?;
                }
                for (element, tags) in remove {
                    for tag in tags {
                        remove_element(tx, id, field, element, *tag)?;
                    }
                }
            }
            FieldOp::Inc(n) => add_delta(tx, id, field, *n, at, meta.legacy)?,
            FieldOp::Map(entries) => {
                for (k, v) in entries {
                    write_map_entry(tx, id, field, k, v, at, origin)?;
                }
            }
        }
    }
    Ok(())
}

/// Re-project one entity from its state, folding nothing new.
fn refresh(tx: &Transaction<'_>, target: EntityRef) -> Result<(), EngineError> {
    let spec = target.kind().spec();
    let Some(mut meta) = read_meta(tx, target.bytes())? else {
        return Ok(());
    };
    project_and_write(tx, spec, target, &mut meta)?;
    write_meta(tx, target.bytes(), spec.tag, &meta)?;
    Ok(())
}

/// The entity as the fold projects it, as a CBOR map: its row before the
/// row writers decode it. `None` for an entity with no merge state, or none
/// created yet.
pub(super) fn projected(
    tx: &Transaction<'_>,
    target: EntityRef,
) -> rusqlite::Result<Option<Value>> {
    let Some(meta) = read_meta(tx, target.bytes())? else {
        return Ok(None);
    };
    project(tx, target.kind().spec(), target, &meta)
}

/// The legacy floor of entity `id` (the module docs' `L`), if a full-state
/// op has been applied to it.
pub(super) fn legacy_floor(tx: &Transaction<'_>, id: &[u8; 16]) -> rusqlite::Result<Option<Stamp>> {
    Ok(read_meta(tx, id)?.and_then(|m| m.legacy))
}

/// `(entity, element's canonical CBOR)` to the stamp of its oldest live add.
pub(super) type OldestAdds = std::collections::BTreeMap<([u8; 16], Vec<u8>), Stamp>;

/// One register's winner: `(entity, value, stamp)`.
pub(super) type Winner = ([u8; 16], Option<Value>, Stamp);

/// For every element of OR-set `field` on every entity, the stamp of its
/// oldest surviving add: an add whose tag no remove names. Adds below an
/// entity's legacy floor are not stored (`state::prune_below_floor`), so
/// every add read here is one the projection counts.
///
/// Read-time rules order an entity's elements by it (ADR-0065 §5): it is a
/// fact of the merged set, the same on every replica that holds it.
pub(super) fn oldest_live_adds(
    conn: &rusqlite::Connection,
    field: &str,
) -> rusqlite::Result<OldestAdds> {
    let mut stmt = conn.prepare(
        "SELECT a.entity_id, a.element, a.hlc_ms, a.hlc_logical, a.tag_device, a.tag_seq,
                a.tag_stream
         FROM merge_orset_adds a
         WHERE a.field = ?1
           AND NOT EXISTS (
               SELECT 1 FROM merge_orset_removes r
               WHERE r.entity_id = a.entity_id AND r.field = a.field
                 AND r.element = a.element AND r.tag_stream = a.tag_stream
                 AND r.tag_device = a.tag_device AND r.tag_seq = a.tag_seq)",
    )?;
    let rows = stmt.query_map([field], |r| {
        Ok((
            blob16(&r.get::<_, Vec<u8>>(0)?),
            r.get::<_, Vec<u8>>(1)?,
            state::stamp_cols(r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?),
        ))
    })?;
    let mut out = OldestAdds::new();
    for row in rows {
        let (entity, element, at) = row?;
        let Some(at) = at else { continue };
        out.entry((entity, element))
            .and_modify(|held| *held = (*held).min(at))
            .or_insert(at);
    }
    Ok(out)
}

/// The stamp of the winning write of register `field` of every entity that
/// holds one: `(entity, value, stamp)`.
pub(super) fn register_winners(
    conn: &rusqlite::Connection,
    field: &str,
) -> rusqlite::Result<Vec<Winner>> {
    let mut stmt = conn.prepare(
        "SELECT entity_id, value, hlc_ms, hlc_logical, device, seq, stream
         FROM merge_registers WHERE field = ?1",
    )?;
    let rows = stmt.query_map([field], |r| {
        Ok((
            blob16(&r.get::<_, Vec<u8>>(0)?),
            r.get::<_, Option<Vec<u8>>>(1)?,
            state::stamp_cols(r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (entity, value, at) = row?;
        if let Some(at) = at {
            out.push((entity, value.as_deref().map(state::dec), at));
        }
    }
    Ok(out)
}

fn project_and_write(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    target: EntityRef,
    meta: &mut Meta,
) -> Result<(), EngineError> {
    let Some(value) = project(tx, spec, target, meta)? else {
        return Ok(());
    };
    write_projection(tx, spec, target, value, &meta.head.lww())?;
    Ok(())
}

// ---- entry point ----

/// The full-state value a legacy entity op carries, or `None` for an op that
/// is not one.
fn legacy_state(inner: &InnerOp) -> Option<Result<Value, ciborium::value::Error>> {
    Some(match inner {
        InnerOp::TaskCreate(t) | InnerOp::TaskUpdate(t) | InnerOp::TaskDelete(t) => {
            Value::serialized(t)
        }
        InnerOp::StreamCreate(s) | InnerOp::StreamUpdate(s) | InnerOp::StreamDelete(s) => {
            Value::serialized(s)
        }
        InnerOp::ContextCreate(c) | InnerOp::ContextUpdate(c) | InnerOp::ContextDelete(c) => {
            Value::serialized(c)
        }
        InnerOp::RoutineCreate(r) | InnerOp::RoutineUpdate(r) | InnerOp::RoutineDelete(r) => {
            Value::serialized(&**r)
        }
        InnerOp::BlockCreate(b) | InnerOp::BlockUpdate(b) | InnerOp::BlockDelete(b) => {
            Value::serialized(&**b)
        }
        InnerOp::AttachmentCreate(a) | InnerOp::AttachmentDelete(a) => Value::serialized(&**a),
        _ => return None,
    })
}

/// [`merge_member`] for an op of no group, as the tests fold one.
#[cfg(test)]
pub(super) fn merge_op(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
    stream: [u8; 16],
) -> rusqlite::Result<()> {
    merge_member(tx, inner, lww, stream, None)
}

/// Fold one entity op into its entity's field state and project the entity
/// to its row (ADR-0044).
///
/// `inner` is a legacy full-state op of an entity that merges as
/// [`sunrise_id::registry::Merge::Lww`], or a `Patch` that [`check_patch`]
/// accepted. `lww` is the sender's stamp and `stream` the stream the op was
/// sealed under, which together are the op's merge stamp and its OR-set tag.
///
/// The op may be one member of a group (ADR-0065 §6), which its version
/// records: a `Patch` names its group itself, and `group` is the group of a
/// full-state member, whose mark `engine::groups` read off it
/// (clarification D13).
pub(super) fn merge_member(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
    stream: [u8; 16],
    group: Option<([u8; 16], u64)>,
) -> rusqlite::Result<()> {
    merge_op_inner(tx, inner, lww, stream, group).map_err(to_sql)
}

/// Carry an engine error out through a transaction closure, which speaks
/// `rusqlite::Error`.
fn to_sql(e: EngineError) -> rusqlite::Error {
    match e {
        EngineError::Sqlite(e) => e,
        other => rusqlite::Error::ToSqlConversionFailure(Box::new(other)),
    }
}

fn merge_op_inner(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
    stream: [u8; 16],
    legacy_group: Option<([u8; 16], u64)>,
) -> Result<(), EngineError> {
    let target = inner.target_ref();
    let spec = target.kind().spec();
    let id = target.bytes();
    let at = Stamp::of(lww, stream);
    let mut meta = read_meta(tx, id)?.unwrap_or_else(|| Meta::fresh(at));
    match inner {
        InnerOp::Patch(p) => {
            // Checked before the transaction opened (`apply_remote_all`), and
            // the check is a pure function of the op. A refusal here, below
            // the idempotence gate, would roll the op row back with it.
            let Ok(ops) = check_patch(p) else {
                debug_assert!(false, "an unchecked patch reached the merge");
                return Ok(());
            };
            let origin = if p.is_generated() { GENERATED } else { USER };
            let parents = p.parents.as_ref().map(|parents| {
                parents
                    .iter()
                    .map(|w| OpRef {
                        stream: w.0,
                        device: w.1,
                        seq: w.2,
                    })
                    .collect()
            });
            let version = Version {
                at,
                kind: VersionKind::Patch,
                origin,
                create: p.create,
                parents,
                group: p.group.as_ref().map(|g| (g.id, g.size)),
                fields: ops.iter().map(|(name, _)| name.clone()).collect(),
            };
            let admitted = admit_version(tx, id, &version)?;
            let mut regs = registers_after(tx, spec, id, &meta, admitted)?;
            fold_patch(tx, id, p.create, &ops, at, origin, &mut meta, &mut regs)?;
            regs.flush(tx)?;
        }
        other => {
            let Some(state) = legacy_state(other) else {
                debug_assert!(false, "{} is not an entity write", other.inner_kind());
                return Ok(());
            };
            let state = state.map_err(|e| EngineError::Cbor(e.to_string()))?;
            let entries = state.as_map().map_or(&[][..], Vec::as_slice);
            let origin = legacy_origin(other.inner_kind(), entries);
            let version = Version {
                at,
                kind: VersionKind::Legacy,
                origin,
                create: other.inner_kind().ends_with(".create"),
                parents: None,
                group: legacy_group,
                fields: legacy_fields(spec, entries),
            };
            let admitted = admit_version(tx, id, &version)?;
            let mut regs = registers_after(tx, spec, id, &meta, admitted)?;
            fold_legacy(tx, spec, id, entries, at, origin, &mut meta, &mut regs)?;
            regs.flush(tx)?;
        }
    }
    project_and_write(tx, spec, target, &mut meta)?;
    write_meta(tx, id, spec.tag, &meta)?;
    Ok(())
}

/// The entity's registers, loaded after its newest version was admitted.
/// A version that arrived late recomputed the entity's frontiers, which can
/// order writes the registers keep as concurrent, so they are filtered again
/// ([`Registers::refilter`]).
fn registers_after(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    id: &[u8; 16],
    meta: &Meta,
    admitted: Admission,
) -> rusqlite::Result<Registers> {
    let mut regs = Registers::load(tx, spec, id, meta.legacy)?;
    if admitted.refined {
        regs.refilter(tx)?;
    }
    Ok(regs)
}

/// The fields a legacy full-state op writes: every field this build
/// registers for the entity, whether the op carried it or left it out, and
/// every other key it carried (ADR-0044 §7).
fn legacy_fields(spec: &'static EntitySpec, state: &[(Value, Value)]) -> BTreeSet<String> {
    let mut fields: BTreeSet<String> = spec
        .records
        .first()
        .map(|r| r.fields.iter().map(|f| f.name.to_owned()).collect())
        .unwrap_or_default();
    fields.extend(
        state
            .iter()
            .filter_map(|(k, _)| k.as_text())
            .filter(|k| *k != FULL_STATE_GROUP_KEY)
            .map(str::to_owned),
    );
    fields
}
