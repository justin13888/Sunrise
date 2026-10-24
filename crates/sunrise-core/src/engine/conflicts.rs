//! Conflicts (ADR-0065 §4): the query that reports them, and the two
//! commands that settle them, [`Command::ResolveConflict`] and
//! [`Command::Restore`].
//!
//! # What is reported
//!
//! The fold keeps, for every register, the antichain of its writes: the
//! winner in `merge_registers` and the rest in `register_losers`
//! (`merge::register`). A field is **in conflict** when a loser:
//!
//! - was written by a `Patch`. A full-state op's write is kept and never
//!   surfaced (clarification D4): such an op cannot tell a field it changed
//!   from one it carried, so its losers would be conflicts nobody made;
//! - holds a value other than the winner's, by canonical CBOR (D5): two
//!   devices completing one task agree;
//! - is of a field whose class is tracked, state or tombstone. A quiet
//!   field's losers are reported only when asked for.
//!
//! A `deleted` register whose winner is `true` also reports **edited while
//! deleted**: every head of the entity the delete does not precede, which is
//! a branch whose writer had not seen the delete (§3, tombstone).
//!
//! Everything read here is a function of the versions applied, so every
//! replica that has applied the same versions reports the same conflicts.
//!
//! # How a conflict is settled
//!
//! A write that descends from every member of a field's set clears it. With
//! `Patch` writes that is a version whose parents are all the entity's heads
//! (§4). Until the vault requires `core.field_ops`, commands write
//! full-state ops (clarification D9), and both commands here do too: the op
//! is the entity as it projects, with the one field set, stamped after every
//! version this device has applied. It becomes the entity's legacy floor, so
//! every write below it leaves its antichain (D4), on every replica that
//! applies it. That settles every field of the entity at once, on the values
//! this device shows, which is what any edit in that mode already does.
//!
//! [`Command::ResolveConflict`]: crate::Command::ResolveConflict
//! [`Command::Restore`]: crate::Command::Restore

use super::ids::{ms_to_ts, require_writable_stream};
use super::merge::{
    check_register_set, class_of, kept_registers, legacy_floor, projected, set_field,
    unmerged_heads, Member, GENERATED,
};
use super::snapshot::in_tx;
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::conflicts::{
    ConflictClass, ConflictWrite, DeletedEdit, EntityConflict, VersionRef, WriteOrigin,
};
use crate::inner_op::InnerOp;
use crate::queries::QueryResult;
use ciborium::value::Value;
use rusqlite::{params, Transaction};
use sunrise_cbor::CborValue;
use sunrise_domain::{Block, Context, Routine, Stream, Task};
use sunrise_id::registry::Conflict;
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The registry kind whose op-log tag is `tag`.
fn kind_of_tag(tag: &str) -> Option<EntityKind> {
    EntityKind::all().into_iter().find(|k| k.tag() == tag)
}

fn write_of(m: &Member) -> ConflictWrite {
    let op = m.at.op_ref();
    ConflictWrite {
        version: VersionRef {
            stream: op.stream,
            device: op.device,
            seq: op.seq,
        },
        hlc_ms: m.at.hlc.physical_ms,
        hlc_logical: m.at.hlc.logical,
        device: m.at.device,
        origin: if m.origin == GENERATED {
            WriteOrigin::Generated
        } else {
            WriteOrigin::User
        },
        value: m
            .value
            .as_deref()
            .map(|b| CborValue(super::merge::decode_value(b))),
    }
}

/// Whether a field of `class` is reported without being asked for.
const fn surfaced(class: Conflict) -> bool {
    matches!(
        class,
        Conflict::Tracked | Conflict::State | Conflict::Tombstone
    )
}

/// Every unresolved conflict, or one entity's. See the module docs.
fn conflicts_in(
    tx: &Transaction<'_>,
    entity: Option<EntityRef>,
    include_quiet: bool,
) -> rusqlite::Result<Vec<EntityConflict>> {
    let only = entity.map(|e| *e.bytes());
    let mut out: Vec<EntityConflict> = Vec::new();
    for k in kept_registers(tx, only.as_ref())? {
        let Some(kind) = kind_of_tag(&k.tag) else {
            continue;
        };
        let class = class_of(kind.spec(), &k.field);
        if !(include_quiet || surfaced(class)) {
            continue;
        }
        let losers: Vec<ConflictWrite> = k
            .losers
            .iter()
            .filter(|l| !l.legacy && l.member.value != k.winner.value)
            .map(|l| write_of(&l.member))
            .collect();
        if losers.is_empty() {
            continue;
        }
        out.push(EntityConflict {
            entity: EntityRef::new(kind, k.entity),
            field: k.field,
            class: class.into(),
            winner: write_of(&k.winner),
            losers,
            edited_while_deleted: Vec::new(),
        });
    }

    // A winning delete, and the branches that never saw it. An entity with
    // one head has none: every version precedes some head, so the delete
    // precedes the only one, or is it.
    let deleted = {
        let mut stmt = tx.prepare(
            "SELECT r.entity_id, e.kind, r.value, r.origin,
                    r.hlc_ms, r.hlc_logical, r.device, r.seq, r.stream
             FROM merge_registers r JOIN merge_entities e ON e.entity_id = r.entity_id
             WHERE r.field = 'deleted' AND r.value = ?1
               AND (?2 IS NULL OR r.entity_id = ?2)
               AND (SELECT COUNT(*) FROM entity_versions v
                    WHERE v.entity_id = r.entity_id AND v.is_head = 1) > 1
             ORDER BY r.entity_id",
        )?;
        let rows = stmt.query_map(
            params![
                super::merge::encode_value(&Value::Bool(true))?,
                only.map(|o| o.to_vec())
            ],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?,
                    r.get::<_, String>(3)?,
                    super::merge::stamp_of_columns(
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ),
                ))
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, tag, value, origin, at) in deleted {
        let (Some(kind), Some(at)) = (kind_of_tag(&tag), at) else {
            continue;
        };
        let id = super::ids::blob16(&id);
        let branches = unmerged_heads(tx, &id, at, legacy_floor(tx, &id)?)?;
        if branches.is_empty() {
            continue;
        }
        let edits: Vec<DeletedEdit> = branches
            .into_iter()
            .map(|h| {
                let op = h.at.op_ref();
                DeletedEdit {
                    version: VersionRef {
                        stream: op.stream,
                        device: op.device,
                        seq: op.seq,
                    },
                    device: op.device,
                    fields: h.fields,
                }
            })
            .collect();
        let target = EntityRef::new(kind, id);
        if let Some(row) = out
            .iter_mut()
            .find(|c| c.entity == target && c.field == "deleted")
        {
            row.edited_while_deleted = edits;
        } else {
            out.push(EntityConflict {
                entity: target,
                field: "deleted".to_owned(),
                class: ConflictClass::Tombstone,
                winner: write_of(&Member { at, value, origin }),
                losers: Vec::new(),
                edited_while_deleted: edits,
            });
        }
    }
    out.sort_by(|a, b| (a.entity.bytes(), &a.field).cmp(&(b.entity.bytes(), &b.field)));
    Ok(out)
}

/// The full-state update op of `kind` carrying `value`, and the stream it
/// is sealed in: the entity's own stream for a task or block, the meta
/// stream otherwise.
fn full_state_update(
    kind: EntityKind,
    value: Value,
    field: &str,
) -> Result<(InnerOp, [u8; 16]), EngineError> {
    let unfit = |_| EngineError::Invalid(format!("`{field}` cannot hold that value"));
    Ok(match kind {
        EntityKind::Task => {
            let t: Task = value.deserialized().map_err(unfit)?;
            require_writable_stream(t.stream_id)?;
            let stream = *t.stream_id.bytes();
            (InnerOp::TaskUpdate(t), stream)
        }
        EntityKind::Stream => {
            let s: Stream = value.deserialized().map_err(unfit)?;
            (InnerOp::StreamUpdate(s), META_STREAM)
        }
        EntityKind::Context => {
            let c: Context = value.deserialized().map_err(unfit)?;
            (InnerOp::ContextUpdate(c), META_STREAM)
        }
        EntityKind::Routine => {
            let r: Routine = value.deserialized().map_err(unfit)?;
            (InnerOp::RoutineUpdate(Box::new(r)), META_STREAM)
        }
        EntityKind::Block => {
            let b: Block = value.deserialized().map_err(unfit)?;
            let stream = *b.stream_id.bytes();
            (InnerOp::BlockUpdate(Box::new(b)), stream)
        }
        other => {
            return Err(EngineError::Invalid(format!(
                "a {} is not restored or resolved",
                other.tag()
            )))
        }
    })
}

/// Which of the two commands a settle runs for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Settle {
    /// [`crate::Command::Restore`]: only of a deleted entity.
    Restore,
    /// [`crate::Command::ResolveConflict`].
    Resolve,
}

impl Engine {
    pub(super) fn query_conflicts(
        &self,
        db: &Db,
        entity: Option<EntityRef>,
        include_quiet: bool,
    ) -> Result<QueryResult, EngineError> {
        // A read transaction, so the registers, losers and heads read here
        // are of one state.
        let tx = db.conn().unchecked_transaction()?;
        let out = conflicts_in(&tx, entity, include_quiet)?;
        tx.rollback()?;
        Ok(QueryResult::Conflicts(out))
    }

    pub(super) fn resolve_conflict(
        &self,
        db: &mut Db,
        entity: EntityRef,
        field: &str,
        value: CborValue,
    ) -> Result<CommandResult, EngineError> {
        self.settle(db, entity, field, value.0, Settle::Resolve)
    }

    pub(super) fn restore(
        &self,
        db: &mut Db,
        entity: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        self.settle(db, entity, "deleted", Value::Bool(false), Settle::Restore)
    }

    /// Write `value` to `field` of `entity` as a full-state op of the entity
    /// as it projects (the module docs, §How a conflict is settled).
    fn settle(
        &self,
        db: &mut Db,
        entity: EntityRef,
        field: &str,
        value: Value,
        how: Settle,
    ) -> Result<CommandResult, EngineError> {
        let kind = entity.kind();
        check_register_set(kind, field, &value).map_err(EngineError::Invalid)?;
        // A saved view has no full-state op: it is settled by a `Patch`
        // descending from every head, ADR-0065 §4's form.
        if kind == EntityKind::SavedView {
            return self.settle_saved_view(db, entity, field, value, how == Settle::Restore);
        }
        let now_ms = self.clock.now_ms();
        in_tx(db, |tx| {
            let Some(Value::Map(mut entries)) = projected(tx, entity)? else {
                return Err(EngineError::NotFound(format!("{} {entity}", kind.tag())));
            };
            let deleted = entries
                .iter()
                .any(|(k, v)| k.as_text() == Some("deleted") && *v == Value::Bool(true));
            if how == Settle::Restore && !deleted {
                return Err(EngineError::Invalid(format!("{entity} is not deleted")));
            }
            set_field(kind, &mut entries, field, value);
            let now = Value::serialized(&ms_to_ts(i64::try_from(now_ms).unwrap_or(i64::MAX)))
                .map_err(|e| EngineError::Cbor(e.to_string()))?;
            set_field(kind, &mut entries, "updated_at", now);
            let (op, stream) = full_state_update(kind, Value::Map(entries), field)?;
            let state = match &op {
                InnerOp::TaskUpdate(t) => Some(t.state.clone()),
                _ => None,
            };
            let done = self.append_and_fold(tx, &op, &stream, now_ms)?;
            Ok(CommandResult::new(entity, state, done.op_id, done.seq))
        })
    }
}
