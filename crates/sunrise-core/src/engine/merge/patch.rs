//! Reading a `Patch`: its field ops, and the check every one of them passes
//! against the registry before anything is written.
//!
//! The check is a pure function of the op. Whether a value fits a field is
//! decided against a fixed sample entity ([`probe`]), never against what a
//! replica happens to hold, so every replica of one build accepts or refuses
//! the same op.

use super::OpRef;
use crate::inner_op::PatchPayload;
use ciborium::value::Value;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::time::SunriseTime;
use sunrise_domain::{
    Attachment, Block, Context, Frequency, Preferences, RRule, Routine, RoutineCatchupPolicy,
    Stream, StreamColor, StreamReviewCadence, Task, TaskState, TaskTemplate, Unknowns,
};
use sunrise_id::registry::{Crdt, EntitySpec, FieldSpec, Merge, RecordSpec};
use sunrise_id::{EntityKind, EntityRef};

// ---- field ops ----

/// One parsed field op of a `Patch` (ADR-0044 §1).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::engine) enum FieldOp {
    /// `{"set": v}`: a register write. `null` clears an optional field.
    Set(Value),
    /// `{"add": [..], "remove": [[v, [op-ref, ..]], ..]}`: an OR-set edit.
    Edit {
        /// Elements added, each tagged with this op's op-ref.
        add: Vec<Value>,
        /// Elements removed, each with the add-tags its writer had observed.
        remove: Vec<(Value, Vec<OpRef>)>,
    },
    /// `{"inc": n}`: a counter delta, never zero.
    Inc(i64),
    /// `{"map": {k: {"set": v}, ..}}`: per-key register writes. A null value
    /// tombstones the key.
    Map(Vec<(String, Value)>),
}

/// Why a `Patch` cannot be applied as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::engine) enum PatchProblem {
    /// It is well formed but uses something this build cannot merge: a
    /// field-op kind it does not know (ADR-0044 §8), or an entity kind it does
    /// not project. The op is parked and replayed after an upgrade; the
    /// string is the kind to park it under.
    Park(String),
    /// It is malformed, or writes a field this build knows in a way the
    /// field's CRDT type forbids, or with a value the field's type cannot
    /// hold. Refused like any other malformed op.
    Invalid(String),
}

/// The fields a `Patch` may never write: identity and the two derived
/// timestamps (ADR-0044 §4, §10).
const UNWRITABLE: [&str; 3] = ["id", "created_at", "updated_at"];

/// Parse and check every field op of `p` against the registry.
///
/// # Errors
/// [`PatchProblem::Park`] for a field-op kind this build does not know, or a
/// `Patch` of an entity kind that has no materialized state here (a newer
/// build's synced entity); [`PatchProblem::Invalid`] for anything malformed
/// or ill-typed.
pub(in crate::engine) fn check_patch(
    p: &PatchPayload,
) -> Result<Vec<(String, FieldOp)>, PatchProblem> {
    let spec = p.target.kind().spec();
    match spec.merge {
        Merge::Lww => {}
        // An entity this build models but does not sync yet. A newer build
        // that syncs it writes it with `Patch`, and the op waits for an
        // upgrade rather than being refused.
        Merge::Unsynced => return Err(PatchProblem::Park(PATCH_KIND.into())),
        Merge::AppendOnly | Merge::Control => {
            return Err(PatchProblem::Invalid(format!(
                "a {} is not written by field ops",
                spec.tag
            )))
        }
    }
    if p.fields.is_empty() {
        return Err(PatchProblem::Invalid("a patch writes no field".into()));
    }
    let mut out = Vec::with_capacity(p.fields.len());
    // An unknown field-op kind anywhere parks the whole op: it is never
    // applied partially (§8). So every field is parsed before any is checked.
    for (name, raw) in &p.fields {
        out.push((name.clone(), parse_field_op(raw.get())?));
    }
    for (name, op) in &out {
        check_field(spec, p.target.kind(), name, op).map_err(PatchProblem::Invalid)?;
    }
    Ok(out)
}

/// The `kind` a parked `Patch` is recorded under.
pub(in crate::engine) const PATCH_KIND: &str = "Patch";

fn parse_field_op(raw: &Value) -> Result<FieldOp, PatchProblem> {
    let invalid = |why: &str| PatchProblem::Invalid(format!("field op: {why}"));
    let entries = raw.as_map().ok_or_else(|| invalid("not a map"))?;
    let mut keys = BTreeMap::new();
    for (k, v) in entries {
        let k = k
            .as_text()
            .ok_or_else(|| invalid("a key that is not text"))?;
        if keys.insert(k, v).is_some() {
            return Err(invalid("a repeated key"));
        }
    }
    // A key this build does not know is a newer field-op kind, which only a
    // newer build can merge.
    if let Some(unknown) = keys
        .keys()
        .find(|k| !matches!(**k, "set" | "add" | "remove" | "inc" | "map"))
    {
        return Err(PatchProblem::Park(format!("{PATCH_KIND}.{unknown}")));
    }
    let edit = keys.contains_key("add") || keys.contains_key("remove");
    let kinds = usize::from(keys.contains_key("set"))
        + usize::from(edit)
        + usize::from(keys.contains_key("inc"))
        + usize::from(keys.contains_key("map"));
    if kinds != 1 {
        return Err(invalid("exactly one of set, add/remove, inc and map"));
    }
    if let Some(v) = keys.get("set") {
        return Ok(FieldOp::Set((*v).clone()));
    }
    if let Some(v) = keys.get("inc") {
        let n = v
            .as_integer()
            .and_then(|n| i64::try_from(n).ok())
            .filter(|n| *n != 0)
            .ok_or_else(|| invalid("inc is a non-zero integer"))?;
        return Ok(FieldOp::Inc(n));
    }
    if let Some(v) = keys.get("map") {
        let entries = v.as_map().ok_or_else(|| invalid("map is a map"))?;
        let mut out = Vec::with_capacity(entries.len());
        for (k, op) in entries {
            let k = k.as_text().ok_or_else(|| invalid("a map key is text"))?;
            let FieldOp::Set(value) = parse_field_op(op)? else {
                return Err(invalid("a map entry is a set"));
            };
            out.push((k.to_owned(), value));
        }
        if out.is_empty() {
            return Err(invalid("map writes no key"));
        }
        return Ok(FieldOp::Map(out));
    }
    let add = match keys.get("add") {
        None => Vec::new(),
        Some(v) => {
            let items = v.as_array().ok_or_else(|| invalid("add is an array"))?;
            if items.is_empty() {
                return Err(invalid("add is not empty"));
            }
            items.clone()
        }
    };
    let remove = match keys.get("remove") {
        None => Vec::new(),
        Some(v) => {
            let items = v.as_array().ok_or_else(|| invalid("remove is an array"))?;
            if items.is_empty() {
                return Err(invalid("remove is not empty"));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let pair = item
                    .as_array()
                    .ok_or_else(|| invalid("a remove is a pair"))?;
                let [element, tags] = pair.as_slice() else {
                    return Err(invalid("a remove is a pair"));
                };
                let tags = tags
                    .as_array()
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| invalid("a remove names its observed tags"))?
                    .iter()
                    .map(|t| OpRef::from_value(t).ok_or_else(|| invalid("a malformed op-ref")))
                    .collect::<Result<Vec<_>, _>>()?;
                out.push((element.clone(), tags));
            }
            out
        }
    };
    Ok(FieldOp::Edit { add, remove })
}

/// The registered field `name` names, and the record it belongs to: a
/// top-level field, or `parent.child` under a nested record.
fn registered_field(
    spec: &'static EntitySpec,
    name: &str,
) -> Option<(&'static RecordSpec, &'static FieldSpec)> {
    let record = spec.records.first()?;
    if let Some(f) = record.fields.iter().find(|f| f.name == name) {
        return Some((record, f));
    }
    let (parent, child) = name.split_once('.')?;
    let nested = nested_record(spec, parent)?;
    nested
        .fields
        .iter()
        .find(|f| f.name == child)
        .map(|f| (nested, f))
}

/// The record a nested field of the entity's own record holds.
pub(super) fn nested_record(
    spec: &'static EntitySpec,
    parent: &str,
) -> Option<&'static RecordSpec> {
    let record = spec.records.first()?;
    let f = record
        .fields
        .iter()
        .find(|f| f.name == parent && f.crdt == Crdt::Nested)?;
    spec.record(f.value_type)
}

fn check_field(
    spec: &'static EntitySpec,
    kind: EntityKind,
    name: &str,
    op: &FieldOp,
) -> Result<(), String> {
    if UNWRITABLE.contains(&name) {
        return Err(format!("`{name}` is not written by a patch"));
    }
    // A field this build does not know merges by the kind its op names, and
    // is kept with the entity's unknown fields (§8).
    let Some((_, field)) = registered_field(spec, name) else {
        return Ok(());
    };
    let fits = match (field.crdt, op) {
        (Crdt::Register, FieldOp::Set(v)) => value_fits(kind, name, v),
        (Crdt::OrSet, FieldOp::Edit { add, remove }) => add
            .iter()
            .chain(remove.iter().map(|(e, _)| e))
            .all(|e| value_fits(kind, name, &Value::Array(vec![e.clone()]))),
        (Crdt::Counter, FieldOp::Inc(_)) => true,
        (Crdt::Map, FieldOp::Map(entries)) => entries.iter().all(|(k, v)| {
            v.is_null()
                || value_fits(
                    kind,
                    name,
                    &Value::Map(vec![(Value::Text(k.clone()), v.clone())]),
                )
        }),
        (Crdt::Nested | Crdt::Derived, _) => {
            return Err(format!(
                "`{name}` is {:?}, not written by a patch",
                field.crdt
            ))
        }
        (crdt, _) => return Err(format!("`{name}` is a {crdt:?}, and the op is not")),
    };
    if fits {
        Ok(())
    } else {
        Err(format!("`{name}` cannot hold the value written to it"))
    }
}

// ---- type probes ----

/// Whether `value`, written to `field`, still deserializes as `kind`'s
/// record. Checked against a fixed sample entity, so the answer depends on
/// the field and the value alone and never on what a replica holds.
fn value_fits(kind: EntityKind, field: &str, value: &Value) -> bool {
    let Some(Value::Map(mut entries)) = probe(kind) else {
        return true;
    };
    let path: Vec<&str> = match field.split_once('.') {
        Some((parent, child)) if nested_record(kind.spec(), parent).is_some() => {
            vec![parent, child]
        }
        _ => vec![field],
    };
    set_path(&mut entries, &path, value.clone());
    deserializes_as(kind, Value::Map(entries))
}

fn set_path(entries: &mut Vec<(Value, Value)>, path: &[&str], value: Value) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    let pos = entries.iter().position(|(k, _)| k.as_text() == Some(first));
    if rest.is_empty() {
        match pos {
            Some(i) => entries[i].1 = value,
            None => entries.push((Value::Text((*first).to_owned()), value)),
        }
        return;
    }
    let i = pos.unwrap_or_else(|| {
        entries.push((Value::Text((*first).to_owned()), Value::Map(Vec::new())));
        entries.len() - 1
    });
    if let Value::Map(inner) = &mut entries[i].1 {
        set_path(inner, rest, value);
    }
}

fn deserializes_as(kind: EntityKind, value: Value) -> bool {
    match kind {
        EntityKind::Task => value.deserialized::<Task>().is_ok(),
        EntityKind::Stream => value.deserialized::<Stream>().is_ok(),
        EntityKind::Context => value.deserialized::<Context>().is_ok(),
        EntityKind::Routine => value.deserialized::<Routine>().is_ok(),
        EntityKind::Block => value.deserialized::<Block>().is_ok(),
        EntityKind::Attachment => value.deserialized::<Attachment>().is_ok(),
        EntityKind::Preferences => value.deserialized::<Preferences>().is_ok(),
        _ => true,
    }
}

/// A valid sample of each projected entity, as CBOR.
fn probe(kind: EntityKind) -> Option<Value> {
    let at = jiff::Timestamp::UNIX_EPOCH;
    let id = |k| EntityRef::new(k, [1u8; 16]);
    let value = match kind {
        EntityKind::Task => Value::serialized(&Task {
            id: id(EntityKind::Task),
            created_at: at,
            updated_at: at,
            title: String::new(),
            body: None,
            stream_id: id(EntityKind::Stream),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            reminder_lead_s: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }),
        EntityKind::Stream => Value::serialized(&Stream {
            id: id(EntityKind::Stream),
            created_at: at,
            updated_at: at,
            name: String::new(),
            description: None,
            color: StreamColor::Slate,
            icon: None,
            parent_id: None,
            sort_order: String::new(),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: StreamReviewCadence::Weekly,
            default_context: None,
            reminder_lead_s: None,
            deleted: false,
            unknown: Unknowns::new(),
        }),
        EntityKind::Context => Value::serialized(&Context {
            id: id(EntityKind::Context),
            created_at: at,
            updated_at: at,
            name: String::new(),
            description: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }),
        EntityKind::Routine => Value::serialized(&Routine {
            id: id(EntityKind::Routine),
            created_at: at,
            updated_at: at,
            template: TaskTemplate {
                title: String::new(),
                stream_id: id(EntityKind::Stream),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: Unknowns::new(),
            },
            rrule: RRule {
                freq: Frequency::Daily,
                interval: 1,
                by_day: Vec::new(),
                by_month_day: Vec::new(),
                by_month: Vec::new(),
                by_set_pos: Vec::new(),
                count: None,
                until: None,
                wkst: None,
                unknown: Unknowns::new(),
            },
            timezone: "UTC".into(),
            starts_at: at,
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }),
        EntityKind::Block => Value::serialized(&Block {
            id: id(EntityKind::Block),
            created_at: at,
            updated_at: at,
            stream_id: id(EntityKind::Stream),
            starts_at: SunriseTime::Instant { at },
            ends_at: SunriseTime::Instant { at },
            title: None,
            title_track_task: false,
            tasks: BTreeSet::new(),
            deleted: false,
            unknown: Unknowns::new(),
        }),
        EntityKind::Attachment => Value::serialized(&Attachment {
            id: id(EntityKind::Attachment),
            created_at: at,
            updated_at: at,
            parent: id(EntityKind::Task),
            filename: String::new(),
            mime_type: String::new(),
            size_bytes: 0,
            blob_key: [0u8; 32],
            blob_id: [0u8; 16],
            chunk_count: 0,
            content_hash: [0u8; 32],
            ciphertext_hash: [0u8; 32],
            width: None,
            height: None,
            thumbnail_blob_id: None,
            thumbnail_blob_key: None,
            thumbnail_mime: None,
            thumbnail_size_bytes: None,
            thumbnail_content_hash: None,
            thumbnail_ciphertext_hash: None,
            deleted: false,
            unknown: Unknowns::new(),
        }),
        EntityKind::Preferences => Value::serialized(&Preferences {
            id: sunrise_domain::preferences_ref(),
            created_at: at,
            updated_at: at,
            values: BTreeMap::new(),
            unknown: Unknowns::new(),
        }),
        _ => return None,
    };
    value.ok()
}
