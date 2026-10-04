//! The merge state as a snapshot's `doc_state` (ADR-0059): written out for
//! one stream, and joined back into another replica's state.
//!
//! # What `doc_state` is
//!
//! The canonical per-field CRDT state of ADR-0044, which is what the
//! compaction document's first blocker asked for in place of a Loro document.
//! For each entity a stream's ops wrote to: its merge bookkeeping, and every
//! register, map entry, OR-set add, OR-set remove and counter delta that an op
//! sealed in the stream wrote. A row is attributed by the stream in its own
//! stamp (an add and a remove by their tag's), so a stream's snapshot carries
//! nothing another stream's ops wrote, and a stream whose key a reader lacks
//! reveals nothing to it. The bookkeeping is carried whole: it holds only
//! stamps.
//!
//! ```cddl
//! doc-state = { "v": 1, "entities": [* entity] }       ; sorted by id
//! entity = {
//!   "id": bstr .size 16, "kind": tstr,                  ; registry tag
//!   "stamps": [* stamp],                                ; distinct, ascending
//!   "created": bool, "create": s / null, "legacy": s / null,
//!   "patch_ms": uint / null, "head": s,
//!   "registers": [* [field, bstr / null, origin, s]],
//!   "maps":      [* [field, key, bstr, origin, s]],
//!   "adds":      [* [field, bstr, s]],                  ; the tag's stamp
//!   "removes":   [* [field, bstr, bstr .size 16, bstr .size 16, uint]],
//!   "deltas":    [* [field, int, s]],
//! }
//! s = uint                                              ; index into "stamps"
//! stamp = [hlc_ms, hlc_logical, device: bstr .size 16, seq, stream: bstr .size 16]
//! ```
//!
//! Values are the canonical CBOR bytes the state tables hold. A stamp is
//! written once per entity and named by its index: one legacy op writes every
//! field of an entity at one stamp, and repeating it on each register made a
//! record three times the size of the state it carried.
//!
//! # Why a join reproduces a full replay
//!
//! Every write in [`super::state`] is idempotent and order-independent: a
//! register keeps the greatest stamp, an add, a remove and a delta are keyed
//! by the op that made them, and the legacy floor only rises. So the state of
//! a set of ops is the join of the states of any cover of it, and
//! [`join_stream_state`] folds each snapshot row in through exactly the write
//! an op carrying it would have used. A replica that joins a snapshot and then
//! applies the ops above its frontier holds the state a replica that replayed
//! every op holds, whatever it held before.

use super::state::{
    add_delta, add_element, dec, prune_below_floor, read_meta, remove_element, write_map_entry,
    write_meta, write_register, Meta,
};
use super::{project_and_write, sync_from_row, to_sql, OpRef, Stamp};
use crate::engine::ids::blob16;
use crate::engine::EngineError;
use ciborium::value::Value;
use rusqlite::{params, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_cbor::hlc::Hlc;
use sunrise_id::registry::{EntitySpec, Merge};
use sunrise_id::{EntityKind, EntityRef};

/// `doc_state`'s own format version.
const DOC_STATE_V: u64 = 1;

/// The registry kind whose op-log tag is `tag`.
fn kind_of_tag(tag: &str) -> Option<EntityKind> {
    EntityKind::all().into_iter().find(|k| k.tag() == tag)
}

/// The registry entry of a kind whose ops merge field by field into a row,
/// the only kinds a snapshot carries.
fn merged_spec(kind: EntityKind) -> Option<&'static EntitySpec> {
    let spec = kind.spec();
    (spec.merge == Merge::Lww && spec.storage().is_some()).then_some(spec)
}

/// Fold each of `targets`' rows into its field state, where a local command
/// wrote the row and the merge has not folded it yet.
///
/// `targets` are `(target_kind, target_id)` pairs as the op log stores them;
/// one whose kind does not merge into a row is skipped. The fold is the one
/// the merge makes before any op touches an entity: see the merge module's
/// docs on seeding and local full-state writes.
pub(in crate::engine) fn fold_rows(
    tx: &Transaction<'_>,
    targets: &[(String, Vec<u8>)],
) -> rusqlite::Result<()> {
    for (tag, raw) in targets {
        let (Some(kind), Ok(id)) = (kind_of_tag(tag), <[u8; 16]>::try_from(raw.as_slice())) else {
            continue;
        };
        let Some(spec) = merged_spec(kind) else {
            continue;
        };
        if let Some(meta) = sync_from_row(tx, spec, EntityRef::new(kind, id)).map_err(to_sql)? {
            write_meta(tx, &id, spec.tag, &meta)?;
        }
    }
    Ok(())
}

// ---- CBOR shapes ----

fn int(n: u64) -> Value {
    Value::Integer(n.into())
}

fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec())
}

fn stamp_value(s: Stamp) -> Value {
    Value::Array(vec![
        int(s.hlc.physical_ms),
        int(u64::from(s.hlc.logical)),
        bytes(&s.device),
        int(s.seq),
        bytes(&s.stream),
    ])
}

fn as_u64(v: &Value) -> Option<u64> {
    u64::try_from(v.as_integer()?).ok()
}

fn as_16(v: &Value) -> Option<[u8; 16]> {
    let b = v.as_bytes()?;
    (b.len() == 16).then(|| blob16(b))
}

fn read_stamp(v: &Value) -> Option<Stamp> {
    let [ms, logical, device, seq, stream] = v.as_array()?.as_slice() else {
        return None;
    };
    Some(Stamp {
        hlc: Hlc {
            physical_ms: as_u64(ms)?,
            logical: u32::try_from(as_u64(logical)?).ok()?,
        },
        device: as_16(device)?,
        seq: as_u64(seq)?,
        stream: as_16(stream)?,
    })
}

/// The stamp an index names, in an entity's `stamps`.
fn stamp_at(stamps: &[Stamp], v: Option<&Value>, what: &str) -> Result<Stamp, EngineError> {
    v.and_then(as_u64)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| stamps.get(i).copied())
        .ok_or_else(|| malformed(what))
}

/// A stamp index or `null`; anything else is malformed.
fn opt_stamp_at(stamps: &[Stamp], v: &Value, what: &str) -> Result<Option<Stamp>, EngineError> {
    if v.is_null() {
        Ok(None)
    } else {
        stamp_at(stamps, Some(v), what).map(Some)
    }
}

fn get<'a>(entries: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn u64_col(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn stamp_cols(ms: i64, logical: i64, device: &[u8], seq: i64, stream: &[u8]) -> Stamp {
    Stamp {
        hlc: Hlc {
            physical_ms: u64_col(ms),
            logical: u32::try_from(logical).unwrap_or(0),
        },
        device: blob16(device),
        seq: u64_col(seq),
        stream: blob16(stream),
    }
}

// ---- dump ----

/// One row as it is gathered: its items, and the stamp that goes last as an
/// index once the entity's stamps are known.
type Item = (Vec<Value>, Option<Stamp>);

/// One entity's rows, gathered from the six state tables.
#[derive(Default)]
struct Rows {
    registers: Vec<Item>,
    maps: Vec<Item>,
    adds: Vec<Item>,
    removes: Vec<Item>,
    deltas: Vec<Item>,
}

impl Rows {
    fn stamps(&self) -> impl Iterator<Item = Stamp> + '_ {
        [
            &self.registers,
            &self.maps,
            &self.adds,
            &self.removes,
            &self.deltas,
        ]
        .into_iter()
        .flatten()
        .filter_map(|(_, s)| *s)
    }
}

/// `items` as one CBOR array, with the stamp's index appended.
fn indexed(rows: Vec<Item>, index: &BTreeMap<Stamp, u64>) -> Value {
    Value::Array(
        rows.into_iter()
            .map(|(mut items, s)| {
                if let Some(s) = s {
                    items.push(int(index[&s]));
                }
                Value::Array(items)
            })
            .collect(),
    )
}

/// Gather `sql`'s rows, each mapped by `row` to `(entity_id, item)`, into
/// `out` under the list `pick` selects.
fn gather(
    tx: &Transaction<'_>,
    stream: &[u8; 16],
    sql: &str,
    out: &mut BTreeMap<[u8; 16], Rows>,
    pick: fn(&mut Rows) -> &mut Vec<Item>,
    row: fn(&rusqlite::Row<'_>) -> rusqlite::Result<Item>,
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(sql)?;
    let mut rows = stmt.query(params![&stream[..]])?;
    while let Some(r) = rows.next()? {
        let id = blob16(&r.get::<_, Vec<u8>>(0)?);
        let item = row(r)?;
        pick(out.entry(id).or_default()).push(item);
    }
    Ok(())
}

/// `doc_state` for `stream`: every entity a row of the stream's ops wrote, in
/// id order, with only the rows those ops wrote. See the module docs.
///
/// The caller folds pending local rows first ([`fold_rows`]), so that a
/// command's write the merge has not seen yet is in the state dumped.
pub(in crate::engine) fn dump_stream_state(
    tx: &Transaction<'_>,
    stream: &[u8; 16],
) -> rusqlite::Result<Value> {
    let mut rows: BTreeMap<[u8; 16], Rows> = BTreeMap::new();
    gather(
        tx,
        stream,
        "SELECT entity_id, field, value, hlc_ms, hlc_logical, device, seq, stream, origin
         FROM merge_registers WHERE stream = ?1 ORDER BY entity_id, field",
        &mut rows,
        |r| &mut r.registers,
        |r| {
            Ok((
                vec![
                    Value::Text(r.get(1)?),
                    r.get::<_, Option<Vec<u8>>>(2)?
                        .map_or(Value::Null, Value::Bytes),
                    Value::Text(r.get(8)?),
                ],
                Some(stamp_cols(
                    r.get(3)?,
                    r.get(4)?,
                    &r.get::<_, Vec<u8>>(5)?,
                    r.get(6)?,
                    &r.get::<_, Vec<u8>>(7)?,
                )),
            ))
        },
    )?;
    gather(
        tx,
        stream,
        "SELECT entity_id, field, map_key, value, hlc_ms, hlc_logical, device, seq, stream, origin
         FROM merge_map_entries WHERE stream = ?1 ORDER BY entity_id, field, map_key",
        &mut rows,
        |r| &mut r.maps,
        |r| {
            Ok((
                vec![
                    Value::Text(r.get(1)?),
                    Value::Text(r.get(2)?),
                    Value::Bytes(r.get(3)?),
                    Value::Text(r.get(9)?),
                ],
                Some(stamp_cols(
                    r.get(4)?,
                    r.get(5)?,
                    &r.get::<_, Vec<u8>>(6)?,
                    r.get(7)?,
                    &r.get::<_, Vec<u8>>(8)?,
                )),
            ))
        },
    )?;
    gather(
        tx,
        stream,
        "SELECT entity_id, field, element, hlc_ms, hlc_logical, tag_device, tag_seq, tag_stream
         FROM merge_orset_adds WHERE tag_stream = ?1
         ORDER BY entity_id, field, element, tag_stream, tag_device, tag_seq",
        &mut rows,
        |r| &mut r.adds,
        |r| {
            Ok((
                vec![Value::Text(r.get(1)?), Value::Bytes(r.get(2)?)],
                Some(stamp_cols(
                    r.get(3)?,
                    r.get(4)?,
                    &r.get::<_, Vec<u8>>(5)?,
                    r.get(6)?,
                    &r.get::<_, Vec<u8>>(7)?,
                )),
            ))
        },
    )?;
    gather(
        tx,
        stream,
        "SELECT entity_id, field, element, tag_stream, tag_device, tag_seq
         FROM merge_orset_removes WHERE tag_stream = ?1
         ORDER BY entity_id, field, element, tag_stream, tag_device, tag_seq",
        &mut rows,
        |r| &mut r.removes,
        |r| {
            Ok((
                vec![
                    Value::Text(r.get(1)?),
                    Value::Bytes(r.get(2)?),
                    Value::Bytes(r.get(3)?),
                    Value::Bytes(r.get(4)?),
                    int(u64_col(r.get(5)?)),
                ],
                None,
            ))
        },
    )?;
    gather(
        tx,
        stream,
        "SELECT entity_id, field, delta, hlc_ms, hlc_logical, op_device, op_seq, op_stream
         FROM merge_counter_deltas WHERE op_stream = ?1
         ORDER BY entity_id, field, op_stream, op_device, op_seq",
        &mut rows,
        |r| &mut r.deltas,
        |r| {
            Ok((
                vec![
                    Value::Text(r.get(1)?),
                    Value::Integer(r.get::<_, i64>(2)?.into()),
                ],
                Some(stamp_cols(
                    r.get(3)?,
                    r.get(4)?,
                    &r.get::<_, Vec<u8>>(5)?,
                    r.get(6)?,
                    &r.get::<_, Vec<u8>>(7)?,
                )),
            ))
        },
    )?;
    // An entity whose bookkeeping names the stream but whose every field row
    // came from another one still has its creation in this stream.
    {
        let mut stmt = tx.prepare(
            "SELECT entity_id FROM merge_entities
             WHERE create_stream = ?1 OR legacy_stream = ?1 OR head_stream = ?1",
        )?;
        let ids = stmt.query_map(params![&stream[..]], |r| r.get::<_, Vec<u8>>(0))?;
        for id in ids {
            rows.entry(blob16(&id?)).or_default();
        }
    }

    let mut entities = Vec::with_capacity(rows.len());
    for (id, r) in rows {
        let Some((kind, meta)) = read_meta_with_kind(tx, &id)? else {
            continue;
        };
        let distinct: BTreeSet<Stamp> = r
            .stamps()
            .chain(meta.create)
            .chain(meta.legacy)
            .chain([meta.head])
            .collect();
        let index: BTreeMap<Stamp, u64> = distinct.iter().copied().zip(0..).collect();
        let at = |s: Option<Stamp>| s.map_or(Value::Null, |s| int(index[&s]));
        entities.push(Value::Map(vec![
            (Value::Text("id".into()), bytes(&id)),
            (Value::Text("kind".into()), Value::Text(kind)),
            (
                Value::Text("stamps".into()),
                Value::Array(distinct.iter().copied().map(stamp_value).collect()),
            ),
            (Value::Text("created".into()), Value::Bool(meta.created)),
            (Value::Text("create".into()), at(meta.create)),
            (Value::Text("legacy".into()), at(meta.legacy)),
            (
                Value::Text("patch_ms".into()),
                meta.patch_ms.map_or(Value::Null, int),
            ),
            (Value::Text("head".into()), at(Some(meta.head))),
            (
                Value::Text("registers".into()),
                indexed(r.registers, &index),
            ),
            (Value::Text("maps".into()), indexed(r.maps, &index)),
            (Value::Text("adds".into()), indexed(r.adds, &index)),
            (Value::Text("removes".into()), indexed(r.removes, &index)),
            (Value::Text("deltas".into()), indexed(r.deltas, &index)),
        ]));
    }
    Ok(Value::Map(vec![
        (Value::Text("v".into()), int(DOC_STATE_V)),
        (Value::Text("entities".into()), Value::Array(entities)),
    ]))
}

fn read_meta_with_kind(
    tx: &Transaction<'_>,
    id: &[u8; 16],
) -> rusqlite::Result<Option<(String, Meta)>> {
    use rusqlite::OptionalExtension;
    let kind: Option<String> = tx
        .query_row(
            "SELECT kind FROM merge_entities WHERE entity_id = ?",
            params![&id[..]],
            |r| r.get(0),
        )
        .optional()?;
    let Some(kind) = kind else { return Ok(None) };
    Ok(read_meta(tx, id)?.map(|m| (kind, m)))
}

// ---- join ----

fn malformed(what: &str) -> EngineError {
    EngineError::Invalid(format!("snapshot doc_state: {what}"))
}

/// One entity read back out of `doc_state`.
struct Entity<'a> {
    target: EntityRef,
    spec: &'static EntitySpec,
    meta: Meta,
    stamps: Vec<Stamp>,
    entries: &'a [(Value, Value)],
}

fn read_entity(v: &Value) -> Result<Entity<'_>, EngineError> {
    let entries = v
        .as_map()
        .ok_or_else(|| malformed("an entity is not a map"))?;
    let field = |name: &str| get(entries, name).ok_or_else(|| malformed(name));
    let id = as_16(field("id")?).ok_or_else(|| malformed("id"))?;
    let tag = field("kind")?.as_text().ok_or_else(|| malformed("kind"))?;
    // A kind this build does not merge cannot be joined, and dropping it
    // would lose what a newer build wrote: the whole snapshot is refused.
    let kind =
        kind_of_tag(tag).ok_or_else(|| malformed("an entity kind this build does not know"))?;
    let spec = merged_spec(kind).ok_or_else(|| malformed("an entity kind that does not merge"))?;
    let stamps = list(entries, "stamps")?
        .iter()
        .map(|s| read_stamp(s).ok_or_else(|| malformed("stamp")))
        .collect::<Result<Vec<_>, _>>()?;
    let meta = Meta {
        created: field("created")?
            .as_bool()
            .ok_or_else(|| malformed("created"))?,
        create: opt_stamp_at(&stamps, field("create")?, "create")?,
        legacy: opt_stamp_at(&stamps, field("legacy")?, "legacy")?,
        patch_ms: match field("patch_ms")? {
            Value::Null => None,
            v => Some(as_u64(v).ok_or_else(|| malformed("patch_ms"))?),
        },
        head: stamp_at(&stamps, Some(field("head")?), "head")?,
        row: None,
    };
    Ok(Entity {
        target: EntityRef::new(kind, id),
        spec,
        meta,
        stamps,
        entries,
    })
}

fn list<'a>(entries: &'a [(Value, Value)], name: &str) -> Result<&'a [Value], EngineError> {
    get(entries, name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| malformed(name))
}

fn text(v: Option<&Value>, what: &str) -> Result<String, EngineError> {
    v.and_then(Value::as_text)
        .map(str::to_owned)
        .ok_or_else(|| malformed(what))
}

fn blob(v: Option<&Value>, what: &str) -> Result<Vec<u8>, EngineError> {
    v.and_then(Value::as_bytes)
        .cloned()
        .ok_or_else(|| malformed(what))
}

/// `local` joined with `snap`: the bookkeeping of the union of the ops each
/// covers. `row` stays local: it is this replica's own projection mark.
fn join_meta(local: Option<Meta>, snap: &Meta) -> Meta {
    let Some(mut m) = local else {
        return snap.clone();
    };
    m.created |= snap.created;
    m.create = match (m.create, snap.create) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    m.legacy = m.legacy.max(snap.legacy);
    m.patch_ms = m.patch_ms.max(snap.patch_ms);
    m.head = m.head.max(snap.head);
    m
}

/// The order entities are re-projected in after a join: a task's projection
/// reads its contexts' tombstones, and a context's restore re-projects its
/// tasks, so contexts go before tasks.
fn projection_rank(kind: EntityKind) -> u8 {
    match kind {
        EntityKind::Stream => 0,
        EntityKind::Context => 1,
        EntityKind::Routine => 2,
        EntityKind::Task => 3,
        EntityKind::Block => 4,
        _ => 5,
    }
}

/// Join `doc_state` into this replica's merge state and re-project every
/// entity it names. Returns those entities, in the order they were projected.
///
/// Each entity's pending local row is folded first, so the join is with
/// everything this replica holds. The whole document is read before anything
/// is written, so a malformed one writes nothing.
///
/// # Errors
/// [`EngineError::Invalid`] for a document that does not parse, or names an
/// entity kind this build does not merge; storage failures.
pub(in crate::engine) fn join_stream_state(
    tx: &Transaction<'_>,
    doc: &Value,
) -> Result<Vec<EntityRef>, EngineError> {
    let top = doc.as_map().ok_or_else(|| malformed("not a map"))?;
    if get(top, "v").and_then(as_u64) != Some(DOC_STATE_V) {
        return Err(malformed("unknown version"));
    }
    let entities = list(top, "entities")?
        .iter()
        .map(read_entity)
        .collect::<Result<Vec<_>, _>>()?;

    for e in &entities {
        let id = e.target.bytes();
        let stamp = |v: Option<&Value>, what: &str| stamp_at(&e.stamps, v, what);
        let local = sync_from_row(tx, e.spec, e.target)?;
        let before = local.as_ref().and_then(|m| m.legacy);
        let meta = join_meta(local, &e.meta);
        if let Some(floor) = meta.legacy.filter(|l| before.is_none_or(|b| *l > b)) {
            prune_below_floor(tx, id, floor)?;
        }
        for r in list(e.entries, "registers")? {
            let items = r.as_array().map_or(&[][..], Vec::as_slice);
            let value = match items.get(1) {
                Some(Value::Null) => None,
                v => Some(dec(&blob(v, "register value")?)),
            };
            write_register(
                tx,
                id,
                &text(items.first(), "register field")?,
                value.as_ref(),
                stamp(items.get(3), "register stamp")?,
                &text(items.get(2), "register origin")?,
            )?;
        }
        for r in list(e.entries, "maps")? {
            let items = r.as_array().map_or(&[][..], Vec::as_slice);
            write_map_entry(
                tx,
                id,
                &text(items.first(), "map field")?,
                &text(items.get(1), "map key")?,
                &dec(&blob(items.get(2), "map value")?),
                stamp(items.get(4), "map stamp")?,
                &text(items.get(3), "map origin")?,
            )?;
        }
        for r in list(e.entries, "adds")? {
            let items = r.as_array().map_or(&[][..], Vec::as_slice);
            add_element(
                tx,
                id,
                &text(items.first(), "add field")?,
                &dec(&blob(items.get(1), "add element")?),
                stamp(items.get(2), "add tag")?,
                meta.legacy,
            )?;
        }
        for r in list(e.entries, "removes")? {
            let items = r.as_array().map_or(&[][..], Vec::as_slice);
            let tag = OpRef {
                stream: items
                    .get(2)
                    .and_then(as_16)
                    .ok_or_else(|| malformed("remove tag"))?,
                device: items
                    .get(3)
                    .and_then(as_16)
                    .ok_or_else(|| malformed("remove tag"))?,
                seq: items
                    .get(4)
                    .and_then(as_u64)
                    .ok_or_else(|| malformed("remove tag"))?,
            };
            remove_element(
                tx,
                id,
                &text(items.first(), "remove field")?,
                &dec(&blob(items.get(1), "remove element")?),
                tag,
            )?;
        }
        for r in list(e.entries, "deltas")? {
            let items = r.as_array().map_or(&[][..], Vec::as_slice);
            let delta = items
                .get(1)
                .and_then(Value::as_integer)
                .and_then(|n| i64::try_from(n).ok())
                .ok_or_else(|| malformed("delta"))?;
            add_delta(
                tx,
                id,
                &text(items.first(), "delta field")?,
                delta,
                stamp(items.get(2), "delta stamp")?,
                meta.legacy,
            )?;
        }
        write_meta(tx, id, e.spec.tag, &meta)?;
    }

    let mut targets: Vec<(EntityRef, &'static EntitySpec)> =
        entities.iter().map(|e| (e.target, e.spec)).collect();
    targets.sort_by_key(|(t, _)| (projection_rank(t.kind()), *t.bytes()));
    for (target, spec) in &targets {
        let id = target.bytes();
        let Some(mut meta) = read_meta(tx, id)? else {
            continue;
        };
        project_and_write(tx, spec, *target, &mut meta)?;
        write_meta(tx, id, spec.tag, &meta)?;
    }
    Ok(targets.into_iter().map(|(t, _)| t).collect())
}
