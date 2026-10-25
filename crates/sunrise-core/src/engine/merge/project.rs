//! Reading the merge state back as an entity, and writing that entity to its
//! row through the row writers the command paths use.
//!
//! [`project`] is a pure function of the state: it applies every rule of the
//! module docs (the legacy floor, OR-set removes, counter bases) and returns
//! the entity as a CBOR map. [`write_projection`] decodes it and writes it.

use super::patch::nested_record;
use super::rows::{
    ftsr_delete_task, ftsr_upsert_task, insert_context_row, insert_routine_row, insert_stream_row,
    insert_task_contexts, insert_task_row, purge_context_from_tasks, replace_block_tasks,
    replace_task_blockers, replace_task_contexts, update_context_row, update_routine_row,
    update_stream_row, update_task_row, upsert_attachment_row, upsert_block_row,
    upsert_preferences_row, upsert_saved_view_row,
};
use super::state::{dec, enc, hlc_of, Meta};
use super::{refresh, to_sql, OpRef, Stamp};
use crate::engine::ids::{blob16, ms_to_ts};
use crate::engine::lww::{read_row_lww, LwwStamp, RowLww};
use crate::engine::stream::ensure_stream_row;
use ciborium::value::Value;
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::{Attachment, Block, Context, Preferences, Routine, SavedView, Stream, Task};
use sunrise_id::registry::{Crdt, EntitySpec};
use sunrise_id::{EntityKind, EntityRef};

/// What a field reads as when no write has reached it, for the fields whose
/// domain type has no serde default: the default the registry declares for
/// it (`sunrise_id::registry` §Defaults), as CBOR. Only these are given one:
/// an entity created by a `Patch` that leaves out any other required field is
/// not projected until a write supplies it (ADR-0044 §4, "nothing is dropped
/// while it waits").
pub(in crate::engine) fn field_default(kind: EntityKind, field: &str) -> Option<Value> {
    let json = kind
        .spec()
        .records
        .first()?
        .fields
        .iter()
        .find(|f| f.name == field)?
        .default?;
    let json: serde_json::Value = serde_json::from_str(json).ok()?;
    Value::serialized(&json).ok()
}

/// The fields of `kind`'s own record that declare a default.
fn defaulted(kind: EntityKind) -> impl Iterator<Item = &'static str> {
    kind.spec()
        .records
        .first()
        .into_iter()
        .flat_map(|r| r.fields.iter())
        .filter(|f| f.default.is_some())
        .map(|f| f.name)
}

/// The order a merged set's elements are written in: text by its string, so
/// a `Vec<String>` set reads back sorted (the streak code binary-searches
/// `streak_keys`), and anything else after it by its canonical bytes.
fn element_order(bytes: &[u8]) -> (u8, Vec<u8>) {
    match dec(bytes) {
        Value::Text(s) => (0, s.into_bytes()),
        _ => (1, bytes.to_vec()),
    }
}

// ---- projection ----

/// Below the legacy floor: written before the greatest legacy op, which
/// rewrote the whole entity.
fn superseded(at: Stamp, legacy: Option<Stamp>) -> bool {
    legacy.is_some_and(|l| at < l)
}

fn load_stamp(r: &rusqlite::Row<'_>, from: usize) -> rusqlite::Result<Stamp> {
    Ok(Stamp {
        hlc: hlc_of(r.get(from)?, r.get(from + 1)?),
        device: blob16(&r.get::<_, Vec<u8>>(from + 2)?),
        seq: u64::try_from(r.get::<_, i64>(from + 3)?.max(0)).unwrap_or(0),
        stream: blob16(&r.get::<_, Vec<u8>>(from + 4)?),
    })
}

/// The merged entity as a CBOR map, or `None` while it is not created.
#[allow(clippy::too_many_lines)]
pub(super) fn project(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    target: EntityRef,
    meta: &Meta,
) -> rusqlite::Result<Option<Value>> {
    if !meta.created {
        return Ok(None);
    }
    let id = target.bytes();
    let kind = target.kind();
    let legacy = meta.legacy;
    let record = spec.records.first();
    let crdt_of =
        |name: &str| record.and_then(|r| r.fields.iter().find(|f| f.name == name).map(|f| f.crdt));

    let mut top: BTreeMap<String, Value> = BTreeMap::new();
    let mut nested: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut registers: BTreeMap<String, (Option<Value>, Stamp)> = BTreeMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT field, value, hlc_ms, hlc_logical, device, seq, stream
             FROM merge_registers WHERE entity_id = ?",
        )?;
        let rows = stmt.query_map(params![&id[..]], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<Vec<u8>>>(1)?,
                load_stamp(r, 2)?,
            ))
        })?;
        for row in rows {
            let (field, value, at) = row?;
            registers.insert(field, (value.as_deref().map(dec), at));
        }
    }
    for (field, (value, at)) in &registers {
        if superseded(*at, legacy) || crdt_of(field) == Some(Crdt::Counter) {
            continue;
        }
        let Some(value) = value else { continue };
        match field.split_once('.') {
            Some((parent, child)) if nested_record(spec, parent).is_some() => {
                nested
                    .entry(parent.to_owned())
                    .or_default()
                    .insert(child.to_owned(), value.clone());
            }
            _ => {
                top.insert(field.clone(), value.clone());
            }
        }
    }
    for (parent, children) in nested {
        top.insert(
            parent,
            Value::Map(
                children
                    .into_iter()
                    .map(|(k, v)| (Value::Text(k), v))
                    .collect(),
            ),
        );
    }

    // OR-sets.
    let mut removed: BTreeSet<(String, Vec<u8>, OpRef)> = BTreeSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT field, element, tag_stream, tag_device, tag_seq
             FROM merge_orset_removes WHERE entity_id = ?",
        )?;
        let rows = stmt.query_map(params![&id[..]], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                OpRef {
                    stream: blob16(&r.get::<_, Vec<u8>>(2)?),
                    device: blob16(&r.get::<_, Vec<u8>>(3)?),
                    seq: u64::try_from(r.get::<_, i64>(4)?.max(0)).unwrap_or(0),
                },
            ))
        })?;
        for row in rows {
            removed.insert(row?);
        }
    }
    let mut sets: BTreeMap<String, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for f in record.map_or(&[][..], |r| r.fields) {
        if f.crdt == Crdt::OrSet {
            sets.insert(f.name.to_owned(), BTreeSet::new());
        }
    }
    {
        let mut stmt = tx.prepare(
            "SELECT field, element, hlc_ms, hlc_logical, tag_device, tag_seq, tag_stream
             FROM merge_orset_adds WHERE entity_id = ?",
        )?;
        let rows = stmt.query_map(params![&id[..]], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                load_stamp(r, 2)?,
            ))
        })?;
        for row in rows {
            let (field, element, at) = row?;
            let entry = sets.entry(field.clone()).or_default();
            if superseded(at, legacy) || removed.contains(&(field, element.clone(), at.op_ref())) {
                continue;
            }
            entry.insert(element);
        }
    }
    for (field, elements) in sets {
        let mut elements: Vec<Vec<u8>> = elements.into_iter().collect();
        elements.sort_by_key(|e| element_order(e));
        top.insert(
            field,
            Value::Array(elements.iter().map(|e| dec(e)).collect()),
        );
    }

    // Counters: the legacy base, plus every delta after it.
    let mut counters: BTreeMap<String, i64> = BTreeMap::new();
    for f in record.map_or(&[][..], |r| r.fields) {
        if f.crdt == Crdt::Counter {
            let base = registers
                .get(f.name)
                .filter(|(_, at)| !superseded(*at, legacy))
                .and_then(|(v, _)| v.as_ref())
                .and_then(Value::as_integer)
                .and_then(|n| i64::try_from(n).ok())
                .unwrap_or(0);
            counters.insert(f.name.to_owned(), base);
        }
    }
    {
        let mut stmt = tx.prepare(
            "SELECT field, delta, hlc_ms, hlc_logical, op_device, op_seq, op_stream
             FROM merge_counter_deltas WHERE entity_id = ?",
        )?;
        let rows = stmt.query_map(params![&id[..]], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                load_stamp(r, 2)?,
            ))
        })?;
        for row in rows {
            let (field, delta, at) = row?;
            let entry = counters.entry(field.clone()).or_insert_with(|| {
                // A counter this build does not know: its base is whatever a
                // legacy op carried under its name.
                registers
                    .get(&field)
                    .filter(|(_, s)| !superseded(*s, legacy))
                    .and_then(|(v, _)| v.as_ref())
                    .and_then(Value::as_integer)
                    .and_then(|n| i64::try_from(n).ok())
                    .unwrap_or(0)
            });
            if legacy.is_none_or(|l| at > l) {
                *entry = entry.saturating_add(delta);
            }
        }
    }
    for (field, n) in counters {
        top.insert(field, Value::Integer(n.into()));
    }

    // Maps: every key whose register is not a tombstone.
    let mut maps: BTreeMap<String, Vec<(String, Value, Stamp)>> = BTreeMap::new();
    for f in record.map_or(&[][..], |r| r.fields) {
        if f.crdt == Crdt::Map {
            maps.insert(f.name.to_owned(), Vec::new());
        }
    }
    {
        let mut stmt = tx.prepare(
            "SELECT field, map_key, value, hlc_ms, hlc_logical, device, seq, stream
             FROM merge_map_entries WHERE entity_id = ? ORDER BY field, map_key",
        )?;
        let rows = stmt.query_map(params![&id[..]], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                load_stamp(r, 3)?,
            ))
        })?;
        for row in rows {
            let (field, key, value, at) = row?;
            let entry = maps.entry(field).or_default();
            if superseded(at, legacy) {
                continue;
            }
            entry.push((key, dec(&value), at));
        }
    }
    for (field, entries) in maps {
        // A field this build does not register as a map can also hold a
        // register: a full-state op writes every unknown key it carries as one
        // (ADR-0044 §7), and cannot tell a map from any other value.
        let whole = registers
            .get(&field)
            .filter(|(v, at)| v.is_some() && !superseded(*at, legacy) && crdt_of(&field).is_none())
            .and_then(|(v, at)| v.clone().map(|v| (v, *at)));
        if let Some(value) = map_over_whole(whole, entries) {
            top.insert(field, value);
        }
    }

    // Identity and the two derived timestamps.
    let to_value = |t: jiff::Timestamp| Value::serialized(&t).unwrap_or(Value::Null);
    top.insert(
        "id".into(),
        Value::serialized(&target).unwrap_or(Value::Null),
    );
    if !top.contains_key("created_at") {
        let ms = meta.create.map_or(0, |c| c.hlc.physical_ms);
        top.insert(
            "created_at".into(),
            to_value(ms_to_ts(i64::try_from(ms).unwrap_or(i64::MAX))),
        );
    }
    let written = top
        .get("updated_at")
        .and_then(|v| v.deserialized::<jiff::Timestamp>().ok());
    let patched = meta
        .patch_ms
        .map(|ms| ms_to_ts(i64::try_from(ms).unwrap_or(i64::MAX)));
    let updated = match (written, patched) {
        (Some(w), Some(p)) => Some(w.max(p)),
        (w, p) => w.or(p),
    };
    if let Some(updated) = updated {
        top.insert("updated_at".into(), to_value(updated));
    } else if let Some(created) = top.get("created_at").cloned() {
        top.insert("updated_at".into(), created);
    }
    for f in defaulted(kind) {
        if !top.contains_key(f) {
            if let Some(v) = field_default(kind, f) {
                top.insert(f.to_owned(), v);
            }
        }
    }
    if kind == EntityKind::Routine && keys_written_by_a_patch(tx, id, legacy)? {
        derive_streak(&mut top);
    }
    Ok(Some(Value::Map(
        top.into_iter().map(|(k, v)| (Value::Text(k), v)).collect(),
    )))
}

/// The routine fields [`derive_streak`] computes (ADR-0044 §3).
const STREAK_FIELDS: [&str; 4] = [
    "streak_counter",
    "last_completed_at",
    "streak_started_at",
    "forgivenesses_in_window",
];

/// Whether a `Patch` stamped above the legacy floor `legacy` wrote the
/// routine's `streak_keys` or `skipped_keys`.
///
/// Until one has, the streak fields are the registers the newest full-state
/// op carried, as they always were, so a history of full-state ops alone
/// projects exactly as before (the legacy guard, ADR-0065 §5). Once one has,
/// the keys hold a write that op never saw, and only a derivation from the
/// keys counts it.
fn keys_written_by_a_patch(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    legacy: Option<Stamp>,
) -> rusqlite::Result<bool> {
    let mut stmt = tx.prepare(
        "SELECT fields, hlc_ms, hlc_logical, device, seq, stream FROM entity_versions
         WHERE entity_id = ? AND kind = 'patch'",
    )?;
    let rows = stmt.query_map(params![&id[..]], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, load_stamp(r, 1)?))
    })?;
    for row in rows {
        let (fields, at) = row?;
        if legacy.is_some_and(|l| at <= l) {
            continue;
        }
        let writes_keys = dec(&fields).as_array().is_some_and(|names| {
            names
                .iter()
                .any(|n| matches!(n.as_text(), Some("streak_keys" | "skipped_keys")))
        });
        if writes_keys {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Replace the routine's streak fields in `top` with the ones its keys add up
/// to ([`Routine::derived_streak`]). A routine that does not decode yet is
/// left as it is: it is not projected until it does.
fn derive_streak(top: &mut BTreeMap<String, Value>) {
    let map = Value::Map(
        top.iter()
            .map(|(k, v)| (Value::Text(k.clone()), v.clone()))
            .collect(),
    );
    let Ok(mut routine) = map.deserialized::<Routine>() else {
        return;
    };
    routine.derived_streak().apply_to(&mut routine);
    let Ok(Value::Map(written)) = Value::serialized(&routine) else {
        return;
    };
    for name in STREAK_FIELDS {
        // A field serde leaves out at its default is left out here too.
        match written.iter().find(|(k, _)| k.as_text() == Some(name)) {
            Some((_, v)) => {
                top.insert(name.to_owned(), v.clone());
            }
            None => {
                top.remove(name);
            }
        }
    }
}

/// A map field's value from its entries, all at or above the legacy floor,
/// and from `whole`, the register a write of the whole field left, if any.
///
/// The whole write is the field's value as of its stamp, the way a legacy op
/// is the entity's: an entry stamped before it is superseded, and one stamped
/// after it is applied on top, setting its key or, with `null`, removing it.
/// So a full-state op that carried an unknown map (the map a newer build's
/// `Patch` wrote) keeps every key it carried, and a later `Patch` still edits
/// it. Where the whole write is not a map, the newer entries replace it, and
/// with none it stands. `None` leaves the register's value where it is.
pub(in crate::engine) fn map_over_whole(
    whole: Option<(Value, Stamp)>,
    entries: Vec<(String, Value, Stamp)>,
) -> Option<Value> {
    let Some((value, at)) = whole else {
        let live = entries
            .into_iter()
            .filter(|(_, v, _)| !v.is_null())
            .map(|(k, v, _)| (Value::Text(k), v))
            .collect();
        return Some(Value::Map(live));
    };
    let newer: Vec<(String, Value)> = entries
        .into_iter()
        .filter(|(_, _, s)| *s > at)
        .map(|(k, v, _)| (k, v))
        .collect();
    let mut base = match value {
        Value::Map(base) => base,
        _ if newer.is_empty() => return None,
        _ => Vec::new(),
    };
    for (key, v) in newer {
        base.retain(|(k, _)| k.as_text() != Some(key.as_str()));
        if !v.is_null() {
            base.push((Value::Text(key), v));
        }
    }
    Some(Value::Map(base))
}

// ---- row I/O ----

/// The row stamp of `id` in its table, `None` when there is no row.
fn row_stamp(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    id: &[u8; 16],
) -> rusqlite::Result<Option<RowLww>> {
    let Some(storage) = spec.storage() else {
        return Ok(None);
    };
    read_row_lww(tx, storage.table, storage.key, id)
}

/// Whether `table` holds a row for `id`.
fn row_exists(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    id: &[u8; 16],
) -> rusqlite::Result<bool> {
    Ok(row_stamp(tx, spec, id)?.is_some())
}

/// Write the merged entity to its row. Returns `false` when the merged state
/// does not yet deserialize as the entity (a `Patch` create that left out a
/// required field), in which case nothing is written.
#[allow(clippy::too_many_lines)]
pub(super) fn write_projection(
    tx: &Transaction<'_>,
    spec: &'static EntitySpec,
    target: EntityRef,
    value: Value,
    head: &LwwStamp,
) -> rusqlite::Result<bool> {
    let id = target.bytes();
    let present = row_exists(tx, spec, id)?;
    macro_rules! decode {
        ($ty:ty) => {
            match value.deserialized::<$ty>() {
                Ok(v) => v,
                // The decode error is not logged: it can quote the plaintext
                // value it failed on.
                Err(_) => {
                    tracing::debug!(
                        ev = "core.merge.unprojectable",
                        kind = spec.tag,
                        "the merged state is not yet a whole entity; it waits for the field it lacks"
                    );
                    return Ok(false);
                }
            }
        };
    }
    match target.kind() {
        EntityKind::Task => {
            let mut t: Task = decode!(Task);
            // A tombstoned context is not shown on any task (ADR-0044 §5). The
            // set keeps it, and a restore re-projects the tasks naming it.
            let mut live = BTreeSet::new();
            for c in &t.contexts {
                let deleted: Option<i64> = tx
                    .query_row(
                        "SELECT deleted FROM contexts WHERE id = ?",
                        params![&c.bytes()[..]],
                        |r| r.get(0),
                    )
                    .optional()?;
                if deleted != Some(1) {
                    live.insert(*c);
                }
            }
            t.contexts = live;
            ensure_stream_row(tx, &t.stream_id, placeholder_ms(t.created_at))?;
            if present {
                update_task_row(tx, &t, head)?;
                replace_task_contexts(tx, &t)?;
            } else {
                insert_task_row(tx, &t, head)?;
                insert_task_contexts(tx, &t)?;
            }
            replace_task_blockers(tx, &t)?;
            if t.deleted {
                ftsr_delete_task(tx, &t.id)?;
            } else {
                ftsr_upsert_task(tx, &t)?;
            }
        }
        EntityKind::Stream => {
            let s: Stream = decode!(Stream);
            if present {
                update_stream_row(tx, &s, head)?;
            } else {
                insert_stream_row(tx, &s, head)?;
            }
        }
        EntityKind::Context => {
            let c: Context = decode!(Context);
            let was_deleted: Option<i64> = tx
                .query_row(
                    "SELECT deleted FROM contexts WHERE id = ?",
                    params![&id[..]],
                    |r| r.get(0),
                )
                .optional()?;
            if present {
                update_context_row(tx, &c, head)?;
            } else {
                insert_context_row(tx, &c, head)?;
            }
            if c.deleted {
                // Keyed on the context id alone, and idempotent, so it runs
                // whether or not this replica holds the tasks yet.
                purge_context_from_tasks(tx, id)?;
            } else if was_deleted == Some(1) {
                restore_context_on_tasks(tx, &target)?;
            }
        }
        EntityKind::Routine => {
            let r: Routine = decode!(Routine);
            ensure_stream_row(tx, &r.template.stream_id, placeholder_ms(r.created_at))?;
            if present {
                update_routine_row(tx, &r, head)?;
            } else {
                insert_routine_row(
                    tx,
                    &r,
                    r.created_at.as_millisecond().max(0).unsigned_abs(),
                    head,
                )?;
                // The insert stamps `updated_at_ms` with the create's time.
                // A routine first projected from a joined snapshot or a
                // rebuild's compacted base has been edited since, so the
                // projected value is written over it.
                update_routine_row(tx, &r, head)?;
            }
        }
        EntityKind::Block => {
            let b: Block = decode!(Block);
            ensure_stream_row(tx, &b.stream_id, placeholder_ms(b.created_at))?;
            upsert_block_row(tx, &b, head)?;
            replace_block_tasks(tx, &b)?;
        }
        EntityKind::Attachment => {
            let a: Attachment = decode!(Attachment);
            upsert_attachment_row(tx, &a, head)?;
        }
        EntityKind::Preferences => {
            let p: Preferences = decode!(Preferences);
            upsert_preferences_row(tx, &p, head)?;
        }
        EntityKind::SavedView => {
            let v: SavedView = decode!(SavedView);
            upsert_saved_view_row(tx, &v, head)?;
        }
        _ => return Ok(false),
    }
    // `created_at` is merged state like any other field, and an update writer
    // leaves the column as the first insert set it. Written here so a create
    // with a smaller stamp that arrives later still moves it, whatever order
    // the ops came in.
    if let Some(storage) = spec.storage() {
        if let Some(created) = value
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_text() == Some("created_at")))
            .and_then(|(_, v)| v.deserialized::<jiff::Timestamp>().ok())
        {
            tx.execute(
                &format!(
                    "UPDATE {} SET created_at_ms = ? WHERE {} = ?",
                    storage.table, storage.key
                ),
                params![created.as_millisecond(), &id[..]],
            )?;
        }
    }
    Ok(true)
}

/// The time a placeholder `streams` row an entity names is stamped with: the
/// entity's `created_at`, a function of its ops, where its head's stamp is a
/// function of the order they came in. The placeholder keeps the earliest
/// one (`ensure_stream_row`).
fn placeholder_ms(created_at: jiff::Timestamp) -> u64 {
    u64::try_from(created_at.as_millisecond()).unwrap_or(0)
}

/// A context came back from a tombstone: re-project every task whose
/// `contexts` set still holds it, so its membership shows again.
fn restore_context_on_tasks(tx: &Transaction<'_>, context: &EntityRef) -> rusqlite::Result<()> {
    let element = enc(&Value::serialized(context).unwrap_or(Value::Null))?;
    let tasks: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT a.entity_id FROM merge_orset_adds a
             JOIN merge_entities m ON m.entity_id = a.entity_id
             WHERE a.field = 'contexts' AND a.element = ? AND m.kind = 'task'",
        )?;
        let rows = stmt.query_map(params![element], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for raw in tasks {
        let task = EntityRef::new(EntityKind::Task, blob16(&raw));
        refresh(tx, task).map_err(to_sql)?;
    }
    Ok(())
}
