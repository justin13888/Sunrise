//! Per-field merge (ADR-0044): every field of an entity merges by its own CRDT
//! type, and the entity row is the read projection of that state.
//!
//! [`merge_op`] is the one entry point. It folds one op into the entity's
//! field state (migration 0033's `merge_*` tables) and then writes the merged
//! entity back to its row through the same row writers the command paths use.
//!
//! # The four field types
//!
//! | Type | State | Value |
//! |---|---|---|
//! | register | `(value, stamp, origin)` | the write with the greatest stamp |
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
//! matters. That second property is what makes seeding from a row exact: see
//! below.
//!
//! The OR-set rule removes *every* add below `L`, including an add of an
//! element `L` itself carries, where ADR-0044 §7 removes only the adds of
//! elements `L` does not carry. The value is the same either way, because `L`
//! re-adds its own elements. What differs is what a later `remove` that
//! observed only `L`'s tag does, and only under this rule is a replica that
//! seeded from its row (and so never saw the older tags) in agreement with
//! one that replayed every op. `docs/05-sync/crdt-design.md` records it.
//!
//! # Seeding, and local full-state writes
//!
//! An entity a vault held before migration 0033 has a row and no field state.
//! The first time an op touches it, [`sync_from_row`] reads the row back as the
//! legacy op that wrote it, at the row's stamp. By the rule above that is
//! exactly what replaying every earlier op would have produced, up to what the
//! row cannot represent, and the row then projects identically.
//!
//! The command paths still write rows directly and log a full-state op. Each
//! such write leaves the row with a stamp the merge did not write, which
//! [`sync_from_row`] notices the next time an op touches the entity, and folds
//! in the same way. The local op always holds the greatest stamp this replica
//! has seen, so folding the row is folding that op.
//!
//! # Visibility, delete, and derived fields
//!
//! An entity is projected only once a create has been applied: a `Patch` with
//! `create`, or any legacy op, since every legacy op wrote a whole row.
//! `deleted` is a register like any other, so an edit that arrives after a
//! delete is kept inside the tombstoned entity and shows again on a restore
//! (ADR-0044 §5). `created_at` is the legacy register where one wrote it, and
//! the least create stamp's physical time otherwise; `updated_at` is the later
//! of the legacy register and the newest `Patch`'s physical time (§10). A
//! `Patch` may write neither, nor `id`, nor a field the registry declares
//! derived or nested.

mod patch;
mod project;
mod state;

pub(super) use patch::{check_patch, PatchProblem};

use super::ids::blob16;
use super::lww::LwwStamp;
use super::EngineError;
use crate::inner_op::InnerOp;
use ciborium::value::Value;
use patch::FieldOp;
use project::{logged_stream, project, read_row_state, row_stamp, write_projection};
use rusqlite::Transaction;
use state::{
    add_delta, add_element, read_meta, remove_element, write_map_entry, write_meta, write_register,
    Meta,
};
use sunrise_cbor::hlc::Hlc;
use sunrise_id::registry::{Crdt, EntitySpec};
use sunrise_id::EntityRef;

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

    const fn op_ref(&self) -> OpRef {
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
    fn from_value(v: &Value) -> Option<Self> {
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
    #[cfg(test)]
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
const GENERATED: &str = "generated";

/// Fold a legacy full-state op, given as its payload's CBOR map (§7).
fn fold_legacy(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    id: &[u8; 16],
    state: &[(Value, Value)],
    at: Stamp,
    origin: &str,
    meta: &mut Meta,
) -> rusqlite::Result<()> {
    meta.note(at);
    meta.note_create(at);
    meta.legacy = Some(meta.legacy.map_or(at, |l| l.max(at)));
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
                write_register(tx, id, field.name, value.as_ref(), at, origin)?;
            }
            Crdt::OrSet => {
                for element in value
                    .as_ref()
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add_element(tx, id, field.name, element, at)?;
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
                    write_register(
                        tx,
                        id,
                        &name,
                        get(inner, sub_field.name).as_ref(),
                        at,
                        origin,
                    )?;
                }
                for (k, v) in inner {
                    let Some(k) = k.as_text() else { continue };
                    if sub.is_some_and(|s| s.fields.iter().any(|f| f.name == k)) {
                        continue;
                    }
                    write_register(tx, id, &format!("{}.{k}", field.name), Some(v), at, origin)?;
                }
            }
        }
    }
    // Every key a newer build wrote, as a register under its own name.
    for (k, v) in state {
        let Some(k) = k.as_text() else { continue };
        if record.fields.iter().any(|f| f.name == k) {
            continue;
        }
        write_register(tx, id, k, Some(v), at, origin)?;
    }
    Ok(())
}

fn fold_patch(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    create: bool,
    ops: &[(String, FieldOp)],
    at: Stamp,
    origin: &str,
    meta: &mut Meta,
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
            FieldOp::Set(v) => write_register(tx, id, field, Some(v), at, origin)?,
            FieldOp::Edit { add, remove } => {
                for element in add {
                    add_element(tx, id, field, element, at)?;
                }
                for (element, tags) in remove {
                    for tag in tags {
                        remove_element(tx, id, field, element, *tag)?;
                    }
                }
            }
            FieldOp::Inc(n) => add_delta(tx, id, field, *n, at)?,
            FieldOp::Map(entries) => {
                for (k, v) in entries {
                    write_map_entry(tx, id, field, k, v, at, origin)?;
                }
            }
        }
    }
    Ok(())
}

/// Fold the row in as a legacy op when it was written by something other
/// than this merge: by a vault older than migration 0033, or by a local
/// full-state command. See the module docs.
fn sync_from_row(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    target: EntityRef,
) -> Result<Option<Meta>, EngineError> {
    let id = target.bytes();
    let mut meta = read_meta(tx, id)?;
    let Some(row) = row_stamp(tx, spec, id)? else {
        return Ok(meta);
    };
    // A placeholder stream row no op has stamped is not an op.
    let Some(device) = row.device.as_deref() else {
        return Ok(meta);
    };
    let seen = (row.hlc, blob16(device), row.seq);
    if meta.as_ref().and_then(|m| m.row) == Some(seen) {
        return Ok(meta);
    }
    let Some(state) = read_row_state(tx, target.kind(), id)? else {
        return Ok(meta);
    };
    let stream = logged_stream(tx, spec, id, device, row.seq, &state)?;
    let at = Stamp {
        hlc: row.hlc,
        device: blob16(device),
        seq: row.seq,
        stream,
    };
    let mut m = meta.take().unwrap_or_else(|| Meta::fresh(at));
    let entries = state.as_map().map_or(&[][..], Vec::as_slice);
    fold_legacy(tx, spec, id, entries, at, USER, &mut m)?;
    m.row = Some(seen);
    Ok(Some(m))
}

/// Re-project one entity from its state, folding nothing new.
fn refresh(tx: &Transaction<'_>, target: EntityRef) -> Result<(), EngineError> {
    let spec = target.kind().spec();
    let Some(mut meta) = sync_from_row(tx, spec, target)? else {
        return Ok(());
    };
    project_and_write(tx, spec, target, &mut meta)?;
    write_meta(tx, target.bytes(), spec.tag, &meta)?;
    Ok(())
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
    let head = meta.head.lww();
    if write_projection(tx, spec, target, value, &head)? {
        meta.row = Some((head.hlc, head.device, head.seq));
    }
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

/// Fold one entity op into its entity's field state and project the entity
/// to its row (ADR-0044).
///
/// `inner` is a legacy full-state op of an entity that merges as
/// [`sunrise_id::registry::Merge::Lww`], or a `Patch` that [`check_patch`]
/// accepted. `lww` is the
/// sender's stamp and `stream` the stream the op was sealed under, which
/// together are the op's merge stamp and its OR-set tag.
pub(super) fn merge_op(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
    stream: [u8; 16],
) -> rusqlite::Result<()> {
    merge_op_inner(tx, inner, lww, stream).map_err(to_sql)
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
) -> Result<(), EngineError> {
    let target = inner.target_ref();
    let spec = target.kind().spec();
    let id = target.bytes();
    let at = Stamp::of(lww, stream);
    let mut meta = sync_from_row(tx, spec, target)?.unwrap_or_else(|| Meta::fresh(at));
    match inner {
        InnerOp::Patch(p) => {
            let ops = check_patch(p)
                .map_err(|problem| EngineError::RemoteOpInvalid(format!("patch: {problem:?}")))?;
            let origin = if p.is_generated() { GENERATED } else { USER };
            fold_patch(tx, id, p.create, &ops, at, origin, &mut meta)?;
        }
        other => {
            let Some(state) = legacy_state(other) else {
                debug_assert!(false, "{} is not an entity write", other.inner_kind());
                return Ok(());
            };
            let state = state.map_err(|e| EngineError::Cbor(e.to_string()))?;
            // ADR-0044 §3: a legacy op is a user write, except the routine
            // engine's own occurrence creates.
            let origin = match other {
                InnerOp::TaskCreate(t) if t.routine_id.is_some() => GENERATED,
                _ => USER,
            };
            let entries = state.as_map().map_or(&[][..], Vec::as_slice);
            fold_legacy(tx, spec, id, entries, at, origin, &mut meta)?;
        }
    }
    project_and_write(tx, spec, target, &mut meta)?;
    write_meta(tx, id, spec.tag, &meta)?;
    Ok(())
}
