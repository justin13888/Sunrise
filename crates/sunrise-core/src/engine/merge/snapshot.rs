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
//! doc-state = { "v": 2, "entities": [* entity],        ; sorted by id
//!               ? "members": [* member] }              ; in stamp order
//! member = [group: bstr .size 16, size: uint, stamp]
//! entity = {
//!   "id": bstr .size 16, "kind": tstr,                  ; registry tag
//!   "stamps": [* stamp],                                ; distinct, ascending
//!   "created": bool, "create": s / null, "legacy": s / null,
//!   "patch_ms": uint / null, "head": s,
//!   "registers": [* [field, bstr / null, origin, s]],   ; each register's winner
//!   "losers":    [* [field, bstr / null, origin, s]],   ; the rest of its antichain
//!   "maps":      [* [field, key, bstr, origin, s]],
//!   "adds":      [* [field, bstr, s]],                  ; the tag's stamp
//!   "removes":   [* [field, bstr, bstr .size 16, bstr .size 16, uint]],
//!   "deltas":    [* [field, int, s]],
//!   "versions":  [* version],                           ; in stamp order
//!   "frontier":  [* [v, s]],                            ; F(versions[v]) reaches s
//! }
//! version = [s, kind: "legacy" / "patch", origin, create: bool,
//!            parents: bstr / null, group: [bstr .size 16, uint] / null,
//!            fields: bstr, head: bool]
//! s = uint                                              ; index into "stamps"
//! v = uint                                              ; index into "versions"
//! stamp = [hlc_ms, hlc_logical, device: bstr .size 16, seq, stream: bstr .size 16]
//! ```
//!
//! `parents` and `fields` are the canonical CBOR `entity_versions` stores:
//! the op-refs the op carried, and the sorted names of the fields it wrote.
//! Version 1 had no `losers`, `versions` or `frontier`, and is still read:
//! its registers join as writes that reach only themselves.
//!
//! `versions` carries the versions the writer still holds. One it pruned
//! (ADR-0065 D25, [`super::prune`]) is read by no version a joiner can still
//! fold, except as a member of a group: `members` names each group member of
//! the stream whose version was pruned, so a joiner that holds the rest of
//! the group counts it present (D14). The key is absent when there is none,
//! and a build that does not read it ignores it.
//!
//! Values are the canonical CBOR bytes the state tables hold. A stamp is
//! written once per entity and named by its index: one legacy op writes every
//! field of an entity at one stamp, and repeating it on each register made a
//! record three times the size of the state it carried.
//!
//! # Why a join reproduces a full replay
//!
//! Every write in [`super::state`] and [`super::register`] is idempotent and
//! order-independent: a register keeps the antichain of its writes, a map
//! entry the greatest stamp, an add, a remove and a delta are keyed by the op
//! that made them, and the legacy floor only rises. The version DAG joins the
//! same way (ADR-0065 §8): a version is keyed by its op-ref, a frontier only
//! grows, so the larger of a held and a joined one is kept, and the heads are
//! recomputed from the result. Versions and frontiers join before the
//! registers and losers, whose antichains are read through them, so a
//! version above the snapshot whose parents it compacted away still orders
//! against them, and a conflict outlives the ops that caused it. So the state of
//! a set of ops is the join of the states of any cover of it, and
//! [`join_stream_state`] folds each snapshot row in through exactly the write
//! an op carrying it would have used. A replica that joins a snapshot and then
//! applies the ops above its frontier holds the state a replica that replayed
//! every op holds, whatever it held before.

use super::prune::pruned_members;
use super::register::Registers;
use super::state::{
    add_delta, add_element, dec, prune_below_floor, read_meta, remove_element, write_map_entry,
    write_meta, Meta,
};
use super::versions::recompute;
use super::{project_and_write, OpRef, Stamp};
use crate::engine::groups::record_joined;
use crate::engine::ids::blob16;
use crate::engine::EngineError;
use ciborium::value::Value;
use rusqlite::{params, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_cbor::hlc::Hlc;
use sunrise_id::registry::{EntitySpec, Merge};
use sunrise_id::{EntityKind, EntityRef};

/// `doc_state`'s own format version: 2 adds the register losers, the
/// versions and their frontiers (ADR-0065 §8). A version-1 document is still
/// joined.
const DOC_STATE_V: u64 = 2;

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

/// One version as `entity_versions` holds it: its stamp, and the items
/// after the stamp's index.
struct VersionRow {
    at: Stamp,
    items: Vec<Value>,
}

/// One entity's rows, gathered from the state and version tables.
#[derive(Default)]
struct Rows {
    registers: Vec<Item>,
    losers: Vec<Item>,
    maps: Vec<Item>,
    adds: Vec<Item>,
    removes: Vec<Item>,
    deltas: Vec<Item>,
    versions: Vec<VersionRow>,
    /// `(version, a stamp its frontier reaches)`.
    frontier: Vec<(OpRef, Stamp)>,
}

impl Rows {
    fn stamps(&self) -> impl Iterator<Item = Stamp> + '_ {
        [
            &self.registers,
            &self.losers,
            &self.maps,
            &self.adds,
            &self.removes,
            &self.deltas,
        ]
        .into_iter()
        .flatten()
        .filter_map(|(_, s)| *s)
        .chain(self.versions.iter().map(|v| v.at))
        .chain(self.frontier.iter().map(|(_, s)| *s))
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
/// `out` under the list `pick` selects. `sql` names the scope's stream as
/// `?1`, or takes no parameter at all.
fn gather(
    tx: &Transaction<'_>,
    scope: Scope<'_>,
    sql: &str,
    out: &mut BTreeMap<[u8; 16], Rows>,
    pick: fn(&mut Rows) -> &mut Vec<Item>,
    row: fn(&rusqlite::Row<'_>) -> rusqlite::Result<Item>,
) -> rusqlite::Result<()> {
    gather_with(tx, scope, sql, out, |rows, r| {
        pick(rows).push(row(r)?);
        Ok(())
    })
}

/// [`gather`], with `push` filing each row into its entity's [`Rows`].
fn gather_with(
    tx: &Transaction<'_>,
    scope: Scope<'_>,
    sql: &str,
    out: &mut BTreeMap<[u8; 16], Rows>,
    push: impl Fn(&mut Rows, &rusqlite::Row<'_>) -> rusqlite::Result<()>,
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(sql)?;
    let mut rows = match scope {
        Scope::Stream(stream) => stmt.query(params![&stream[..]])?,
        Scope::Compacted => stmt.query([])?,
    };
    while let Some(r) = rows.next()? {
        let id = blob16(&r.get::<_, Vec<u8>>(0)?);
        push(out.entry(id).or_default(), r)?;
    }
    Ok(())
}

/// A register's row, winner or loser: `[field, value, origin]` and its stamp.
fn register_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
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
}

/// Which state rows a dump carries.
#[derive(Debug, Clone, Copy)]
enum Scope<'a> {
    /// The rows a stream's ops wrote: a snapshot's `doc_state`.
    Stream(&'a [u8; 16]),
    /// The rows written by ops a compaction floor covers, in every stream:
    /// the base [`dump_compacted_base`] hands a projection rebuild.
    Compacted,
}

impl Scope<'_> {
    /// The `WHERE` predicate over a state table aliased `m` whose rows are
    /// attributed by the op-ref `(stream, device, seq)` in those columns.
    fn written_by(self, stream: &str, device: &str, seq: &str) -> String {
        match self {
            Scope::Stream(_) => format!("m.{stream} = ?1"),
            Scope::Compacted => format!(
                "EXISTS (SELECT 1 FROM compaction_floor f
                         WHERE f.stream_id = m.{stream} AND f.device_id = m.{device}
                           AND m.{seq} <= f.seq)"
            ),
        }
    }

    /// The predicate over `merge_orset_removes`. A remove holds no stamp of
    /// its own, only the tag it removes, so a rebuild's base keeps every one:
    /// a remove is keyed by what it names, and folding it twice is a no-op.
    fn removes(self) -> String {
        match self {
            Scope::Stream(_) => "m.tag_stream = ?1".to_owned(),
            Scope::Compacted => "1".to_owned(),
        }
    }

    /// The predicate over `merge_entities` naming each entity whose
    /// bookkeeping the scope carries even when none of its field rows is.
    fn bookkeeping(self) -> String {
        match self {
            Scope::Stream(_) => {
                "m.create_stream = ?1 OR m.legacy_stream = ?1 OR m.head_stream = ?1".to_owned()
            }
            Scope::Compacted => ["create", "legacy", "head"]
                .map(|s| {
                    self.written_by(
                        &format!("{s}_stream"),
                        &format!("{s}_device"),
                        &format!("{s}_seq"),
                    )
                })
                .join(" OR "),
        }
    }
}

/// `doc_state` for `stream`: every entity a row of the stream's ops wrote, in
/// id order, with only the rows those ops wrote. See the module docs.
pub(in crate::engine) fn dump_stream_state(
    tx: &Transaction<'_>,
    stream: &[u8; 16],
) -> rusqlite::Result<Value> {
    dump(tx, Scope::Stream(stream))
}

/// The state a projection rebuild cannot re-derive from `ops`, as one
/// `doc_state`: every row an op at or below a `compaction_floor` wrote, in
/// every stream, every OR-set remove, and the whole bookkeeping of each entity
/// those rows, or a covered create, legacy or head stamp, belong to.
///
/// Compaction deleted those ops after folding them (ADR-0059), and a replica
/// that joined a snapshot never held them, so this state is the only record
/// of what they wrote. Joining it into cleared state with
/// [`join_stream_state`] and then folding every op still in the log is the
/// full replay: each write here is idempotent and order-independent (see the
/// module docs), so a row the base carries that an op still in the log also
/// wrote folds to itself.
pub(in crate::engine) fn dump_compacted_base(tx: &Transaction<'_>) -> rusqlite::Result<Value> {
    dump(tx, Scope::Compacted)
}

/// `doc_state` for the rows `scope` selects.
fn dump(tx: &Transaction<'_>, scope: Scope<'_>) -> rusqlite::Result<Value> {
    let mut rows: BTreeMap<[u8; 16], Rows> = BTreeMap::new();
    gather(
        tx,
        scope,
        &format!(
            "SELECT entity_id, field, value, hlc_ms, hlc_logical, device, seq, stream, origin
             FROM merge_registers m WHERE {} ORDER BY entity_id, field",
            scope.written_by("stream", "device", "seq")
        ),
        &mut rows,
        |r| &mut r.registers,
        register_item,
    )?;
    gather(
        tx,
        scope,
        &format!(
            "SELECT entity_id, field, value, hlc_ms, hlc_logical, device, seq, stream, origin
             FROM register_losers m WHERE {}
             ORDER BY entity_id, field, hlc_ms, hlc_logical, device, seq, stream",
            scope.written_by("stream", "device", "seq")
        ),
        &mut rows,
        |r| &mut r.losers,
        register_item,
    )?;
    gather_with(
        tx,
        scope,
        &format!(
            "SELECT entity_id, hlc_ms, hlc_logical, device, seq, stream, kind, origin,
                    is_create, parents, group_id, group_size, fields, is_head
             FROM entity_versions m WHERE {}
             ORDER BY entity_id, hlc_ms, hlc_logical, device, seq, stream",
            scope.written_by("stream", "device", "seq")
        ),
        &mut rows,
        |rows, r| {
            let at = stamp_cols(
                r.get(1)?,
                r.get(2)?,
                &r.get::<_, Vec<u8>>(3)?,
                r.get(4)?,
                &r.get::<_, Vec<u8>>(5)?,
            );
            let group = match (
                r.get::<_, Option<Vec<u8>>>(10)?,
                r.get::<_, Option<i64>>(11)?,
            ) {
                (Some(id), Some(size)) => Value::Array(vec![Value::Bytes(id), int(u64_col(size))]),
                _ => Value::Null,
            };
            rows.versions.push(VersionRow {
                at,
                items: vec![
                    Value::Text(r.get(6)?),
                    Value::Text(r.get(7)?),
                    Value::Bool(r.get::<_, i64>(8)? != 0),
                    r.get::<_, Option<Vec<u8>>>(9)?
                        .map_or(Value::Null, Value::Bytes),
                    group,
                    Value::Bytes(r.get(12)?),
                    Value::Bool(r.get::<_, i64>(13)? != 0),
                ],
            });
            Ok(())
        },
    )?;
    gather_with(
        tx,
        scope,
        &format!(
            "SELECT entity_id, v_stream, v_device, v_seq,
                    hlc_ms, hlc_logical, device, seq, stream
             FROM entity_frontier m WHERE {}
             ORDER BY entity_id, v_stream, v_device, v_seq, device",
            scope.written_by("v_stream", "v_device", "v_seq")
        ),
        &mut rows,
        |rows, r| {
            let v = OpRef {
                stream: blob16(&r.get::<_, Vec<u8>>(1)?),
                device: blob16(&r.get::<_, Vec<u8>>(2)?),
                seq: u64_col(r.get(3)?),
            };
            let reached = stamp_cols(
                r.get(4)?,
                r.get(5)?,
                &r.get::<_, Vec<u8>>(6)?,
                r.get(7)?,
                &r.get::<_, Vec<u8>>(8)?,
            );
            rows.frontier.push((v, reached));
            Ok(())
        },
    )?;
    gather(
        tx,
        scope,
        &format!(
            "SELECT entity_id, field, map_key, value, hlc_ms, hlc_logical, device, seq, stream,
                    origin
             FROM merge_map_entries m WHERE {} ORDER BY entity_id, field, map_key",
            scope.written_by("stream", "device", "seq")
        ),
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
        scope,
        &format!(
            "SELECT entity_id, field, element, hlc_ms, hlc_logical, tag_device, tag_seq,
                    tag_stream
             FROM merge_orset_adds m WHERE {}
             ORDER BY entity_id, field, element, tag_stream, tag_device, tag_seq",
            scope.written_by("tag_stream", "tag_device", "tag_seq")
        ),
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
        scope,
        &format!(
            "SELECT entity_id, field, element, tag_stream, tag_device, tag_seq
             FROM merge_orset_removes m WHERE {}
             ORDER BY entity_id, field, element, tag_stream, tag_device, tag_seq",
            scope.removes()
        ),
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
        scope,
        &format!(
            "SELECT entity_id, field, delta, hlc_ms, hlc_logical, op_device, op_seq, op_stream
             FROM merge_counter_deltas m WHERE {}
             ORDER BY entity_id, field, op_stream, op_device, op_seq",
            scope.written_by("op_stream", "op_device", "op_seq")
        ),
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
        let mut stmt = tx.prepare(&format!(
            "SELECT entity_id FROM merge_entities m WHERE {}",
            scope.bookkeeping()
        ))?;
        let mut ids = match scope {
            Scope::Stream(stream) => stmt.query(params![&stream[..]])?,
            Scope::Compacted => stmt.query([])?,
        };
        while let Some(r) = ids.next()? {
            rows.entry(blob16(&r.get::<_, Vec<u8>>(0)?)).or_default();
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
        let version_index: BTreeMap<OpRef, u64> =
            r.versions.iter().map(|v| v.at.op_ref()).zip(0..).collect();
        let versions = Value::Array(
            r.versions
                .into_iter()
                .map(|v| {
                    let mut items = vec![int(index[&v.at])];
                    items.extend(v.items);
                    Value::Array(items)
                })
                .collect(),
        );
        // A frontier row names a version the same scope selected, so its
        // index is always there; one that is not would be a damaged table,
        // and is left out rather than written as a wrong index.
        let frontier = Value::Array(
            r.frontier
                .into_iter()
                .filter_map(|(v, reached)| {
                    let v = version_index.get(&v)?;
                    Some(Value::Array(vec![int(*v), int(index[&reached])]))
                })
                .collect(),
        );
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
            (Value::Text("losers".into()), indexed(r.losers, &index)),
            (Value::Text("maps".into()), indexed(r.maps, &index)),
            (Value::Text("adds".into()), indexed(r.adds, &index)),
            (Value::Text("removes".into()), indexed(r.removes, &index)),
            (Value::Text("deltas".into()), indexed(r.deltas, &index)),
            (Value::Text("versions".into()), versions),
            (Value::Text("frontier".into()), frontier),
        ]));
    }
    let mut top = vec![
        (Value::Text("v".into()), int(DOC_STATE_V)),
        (Value::Text("entities".into()), Value::Array(entities)),
    ];
    // A group member whose version was pruned (ADR-0065 D25) is still
    // present to a joiner holding the rest of its group (D14). A rebuild's
    // base needs none: it keeps those `pending_groups` rows.
    if let Scope::Stream(stream) = scope {
        let members = pruned_members(tx, stream)?;
        if !members.is_empty() {
            top.push((
                Value::Text("members".into()),
                Value::Array(
                    members
                        .into_iter()
                        .map(|(id, size, at)| {
                            Value::Array(vec![bytes(&id), int(size), stamp_value(at)])
                        })
                        .collect(),
                ),
            ));
        }
    }
    Ok(Value::Map(top))
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

/// Join one entity's `versions` and `frontier` (version 2) into
/// `entity_versions` and `entity_frontier`: a version keyed by its op-ref,
/// each frontier entry the greater of the held and the joined one. The
/// entity's frontiers and heads are then recomputed, keeping what was joined
/// as their floor. A version of a group records the member as present
/// (`engine::groups`). Returns whether any version was joined.
fn join_versions(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    stamps: &[Stamp],
    versions: &[Value],
    frontier: &[Value],
) -> Result<bool, EngineError> {
    let stamp = |v: Option<&Value>, what: &str| stamp_at(stamps, v, what);
    let mut joined: Vec<OpRef> = Vec::with_capacity(versions.len());
    for v in versions {
        let items = v.as_array().map_or(&[][..], Vec::as_slice);
        let at = stamp(items.first(), "version stamp")?;
        let kind = text(items.get(1), "version kind")?;
        if !matches!(kind.as_str(), "legacy" | "patch") {
            return Err(malformed("version kind"));
        }
        let origin = text(items.get(2), "version origin")?;
        let create = items
            .get(3)
            .and_then(Value::as_bool)
            .ok_or_else(|| malformed("version create"))?;
        let parents = match items.get(4) {
            Some(Value::Null) => None,
            v => Some(blob(v, "version parents")?),
        };
        let group = match items.get(5) {
            Some(Value::Null) => None,
            Some(Value::Array(g)) => match g.as_slice() {
                [gid, size] => Some((
                    as_16(gid).ok_or_else(|| malformed("version group"))?,
                    as_u64(size).ok_or_else(|| malformed("version group"))?,
                )),
                _ => return Err(malformed("version group")),
            },
            _ => return Err(malformed("version group")),
        };
        let fields = blob(items.get(6), "version fields")?;
        let head = items
            .get(7)
            .and_then(Value::as_bool)
            .ok_or_else(|| malformed("version head"))?;
        tx.prepare(
            "INSERT OR IGNORE INTO entity_versions
             (entity_id, stream, device, seq, hlc_ms, hlc_logical, kind, origin, is_create,
              parents, group_id, group_size, fields, is_head)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            &id[..],
            &at.stream[..],
            &at.device[..],
            at.seq,
            at.hlc.physical_ms,
            at.hlc.logical,
            kind,
            origin,
            i64::from(create),
            parents,
            group.map(|(gid, _)| gid.to_vec()),
            group.map(|(_, size)| size),
            fields,
            i64::from(head),
        ])?;
        if let Some(group) = group {
            record_joined(tx, group, at.op_ref(), at.hlc)?;
        }
        joined.push(at.op_ref());
    }
    for f in frontier {
        let items = f.as_array().map_or(&[][..], Vec::as_slice);
        let v = items
            .first()
            .and_then(as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| joined.get(i))
            .ok_or_else(|| malformed("frontier version"))?;
        let reached = stamp(items.get(1), "frontier stamp")?;
        tx.prepare(
            "INSERT INTO entity_frontier
             (entity_id, v_stream, v_device, v_seq, device, hlc_ms, hlc_logical, seq, stream)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (entity_id, v_stream, v_device, v_seq, device) DO UPDATE SET
                hlc_ms = excluded.hlc_ms, hlc_logical = excluded.hlc_logical,
                seq = excluded.seq, stream = excluded.stream
             WHERE (excluded.hlc_ms, excluded.hlc_logical, excluded.seq, excluded.stream)
                 > (entity_frontier.hlc_ms, entity_frontier.hlc_logical,
                    entity_frontier.seq, entity_frontier.stream)",
        )?
        .execute(params![
            &id[..],
            &v.stream[..],
            &v.device[..],
            v.seq,
            &reached.device[..],
            reached.hlc.physical_ms,
            reached.hlc.logical,
            reached.seq,
            &reached.stream[..],
        ])?;
    }
    if joined.is_empty() {
        return Ok(false);
    }
    recompute(tx, id)?;
    Ok(true)
}

/// One `[group id, size, stamp]` of `members`: a group member whose version
/// the writer pruned.
fn read_member(v: &Value) -> Result<(([u8; 16], u64), Stamp), EngineError> {
    let bad = || malformed("member");
    let [id, size, at] = v.as_array().map(Vec::as_slice).ok_or_else(bad)? else {
        return Err(bad());
    };
    Ok((
        (as_16(id).ok_or_else(bad)?, as_u64(size).ok_or_else(bad)?),
        read_stamp(at).ok_or_else(bad)?,
    ))
}

/// `local` joined with `snap`: the bookkeeping of the union of the ops each
/// covers.
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
/// It writes into `tx` as it goes: the group members are recorded before the
/// per-entity loop, and a malformed entity can still be refused inside that
/// loop after earlier entities were joined. A malformed document leaves
/// nothing behind only because the caller rolls `tx` back on the error, as
/// the snapshot apply path's `in_tx` and the rebuild's transaction do.
///
/// # Errors
/// [`EngineError::Invalid`] for a document that does not parse, or names an
/// entity kind this build does not merge; storage failures.
pub(in crate::engine) fn join_stream_state(
    tx: &Transaction<'_>,
    doc: &Value,
) -> Result<Vec<EntityRef>, EngineError> {
    let top = doc.as_map().ok_or_else(|| malformed("not a map"))?;
    let version = get(top, "v").and_then(as_u64);
    if !matches!(version, Some(1 | DOC_STATE_V)) {
        return Err(malformed("unknown version"));
    }
    // Version 1 carried no losers and no version DAG.
    let v2 = version == Some(DOC_STATE_V);
    let entities = list(top, "entities")?
        .iter()
        .map(read_entity)
        .collect::<Result<Vec<_>, _>>()?;
    let members = match get(top, "members") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .ok_or_else(|| malformed("members"))?
            .iter()
            .map(read_member)
            .collect::<Result<Vec<_>, _>>()?,
    };
    for (group, at) in members {
        record_joined(tx, group, at.op_ref(), at.hlc)?;
    }

    for e in &entities {
        let id = e.target.bytes();
        let stamp = |v: Option<&Value>, what: &str| stamp_at(&e.stamps, v, what);
        let since = |name: &str| -> Result<&[Value], EngineError> {
            if v2 {
                list(e.entries, name)
            } else {
                Ok(&[])
            }
        };
        // The DAG first: the registers below read the order through it.
        let joined = join_versions(tx, id, &e.stamps, since("versions")?, since("frontier")?)?;
        let local = read_meta(tx, id)?;
        let before = local.as_ref().and_then(|m| m.legacy);
        let meta = join_meta(local, &e.meta);
        let mut regs = Registers::load(tx, e.spec, id, before)?;
        if joined {
            regs.refilter(tx)?;
        }
        if let Some(floor) = meta.legacy.filter(|l| before.is_none_or(|b| *l > b)) {
            prune_below_floor(tx, id, floor, &mut regs)?;
        }
        for r in list(e.entries, "registers")?.iter().chain(since("losers")?) {
            let items = r.as_array().map_or(&[][..], Vec::as_slice);
            let value = match items.get(1) {
                Some(Value::Null) => None,
                v => Some(dec(&blob(v, "register value")?)),
            };
            regs.offer(
                tx,
                &text(items.first(), "register field")?,
                value.as_ref(),
                stamp(items.get(3), "register stamp")?,
                &text(items.get(2), "register origin")?,
            )?;
        }
        regs.flush(tx)?;
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
