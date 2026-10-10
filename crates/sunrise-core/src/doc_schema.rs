//! The canonical document schema, its fingerprint, and the registry that pins
//! one fingerprint to each `DOC_SCHEMA_V` (ADR-0045 §2, issue #323).
//!
//! `DOC_SCHEMA_V` names a set of shapes, and until this module nothing checked
//! which. Two branches could each ship the same next integer with different
//! shapes, and every version check would say their clients agree. So the
//! shapes are described here as data, that description is hashed, and the hash
//! is pinned against the version that names it.
//!
//! # The schema
//!
//! [`canonical_schema`] builds it from the code, never by hand:
//!
//! - every entity, its op variants and the records they write, with each
//!   field's wire name, value type and CRDT type, from the entity registry
//!   ([`sunrise_id::registry::ENTITIES`]);
//! - every `InnerOp` variant the decoder accepts, read out of the derive, so
//!   the control families are covered as well as the registry's, and the
//!   payload type of each variant the registry does not declare
//!   ([`crate::control_op::OP_PAYLOADS`]);
//! - every value type a record or one of those payloads carries, field by
//!   field: a record's fields, the type an alias encodes as, a tuple's items,
//!   or an enum's variants and tag (`sunrise_domain::registry::VALUE_TYPES`
//!   and [`crate::control_op::PAYLOAD_VALUE_TYPES`]). A test fails while a
//!   type the schema names is described nowhere;
//! - every lossless string enum on the wire, with the spellings its
//!   `lossy_enum!` table holds and the arm an unknown value reads as;
//! - every feature id this build supports ([`crate::feature::FEATURES`]),
//!   with its scope, the op kinds, fields and field-op kinds it introduces,
//!   and the version it arrived in (ADR-0045 §7).
//!
//! What it does not hold is what is not part of the document's shape: storage
//! tables and columns, Rust field names, declaration comments, and the order
//! anything is declared in. Every list is sorted before it is hashed: an
//! entity is found by its tag, an op by its kind, a field by its name in a
//! canonical CBOR map that sorts its keys, and an enum value by its spelling,
//! so no declaration order reaches the wire.
//!
//! It is committed as `schemas/doc-schema/current.json`.
//!
//! # The fingerprint
//!
//! `BLAKE3::derive_key(DOC_SCHEMA_FP_DOMAIN, JCS(schema))`. JCS (RFC 8785) is
//! the canonical JSON ADR-0022 already uses, so the committed file's layout
//! does not reach the hash.
//!
//! # The registry
//!
//! [`sunrise_cbor::version::DOC_SCHEMA_FINGERPRINTS`] is the build's copy,
//! which the writer stamps envelope field 13 from. It is committed as
//! `schemas/doc-schema/registry.json`, and every entry is frozen as a literal
//! in `sunrise-crypto-test-vectors`, so an entry that changes or disappears
//! fails a test whose expectation the change did not touch.
//!
//! # When a test here fails
//!
//! A changed shape fails [`tests::the_schema_fingerprint_is_the_registered_one`].
//! That is a document-schema change: bump `DOC_SCHEMA_V`, append the new
//! version and the fingerprint the failure prints to `DOC_SCHEMA_FINGERPRINTS`
//! and to the frozen list, then regenerate both committed files and let Biome
//! lay them out:
//!
//! ```text
//! SUNRISE_REGEN_FIXTURES=1 cargo test -p sunrise-core --lib doc_schema
//! mise run fix
//! ```
//!
//! Never edit an existing registry entry to make the failure go away: an
//! entry is what every build that shipped it believes the version means.

use serde_json::{json, Value};
use std::path::PathBuf;
use sunrise_cbor::version::{DOC_SCHEMA_FINGERPRINTS, DOC_SCHEMA_FP_DOMAIN};
use sunrise_id::registry::{
    Crdt, Merge, OpClass, Owner, Tagging, ValueField, ValueShape, ValueSpec, VariantShape, ENTITIES,
};

/// The version of this document's own layout. Bumped only if the schema's
/// JSON structure changes, which also moves every fingerprint.
const SCHEMA_FORMAT: u64 = 1;

/// Every lossless string enum that crosses the wire, as
/// `(name, known spellings, fallback spelling)`.
///
/// The spellings and the fallback are read from each enum's own
/// `lossy_enum!` table, so a variant added there reaches the schema. The list
/// of enums is the one part written here, and the test
/// `every_lossy_enum_user_is_described` fails while a `lossy_enum!`
/// user in `sunrise-domain` is missing from it.
fn lossless_enums() -> Vec<(&'static str, Vec<&'static str>, Option<&'static str>)> {
    use sunrise_domain::constraint::ConstraintSeverity;
    use sunrise_domain::focus::{FocusKind, InterruptionReason};
    use sunrise_domain::routine::RoutineCatchupPolicy;
    use sunrise_domain::rrule::{Frequency, Weekday};
    use sunrise_domain::stream::{StreamColor, StreamReviewCadence};
    use sunrise_domain::task::TaskState;
    use sunrise_domain::Energy;

    macro_rules! spellings {
        ($ty:ty) => {
            <$ty>::KNOWN.iter().map(|v| v.as_str()).collect::<Vec<_>>()
        };
    }
    macro_rules! with_fallback {
        ($name:literal, $ty:ty) => {
            ($name, spellings!($ty), Some(<$ty>::FALLBACK.as_str()))
        };
    }
    macro_rules! without_fallback {
        ($name:literal, $ty:ty) => {
            ($name, spellings!($ty), None)
        };
    }

    vec![
        with_fallback!("ConstraintSeverity", ConstraintSeverity),
        with_fallback!("Energy", Energy),
        with_fallback!("FocusKind", FocusKind),
        without_fallback!("Frequency", Frequency),
        with_fallback!("InterruptionReason", InterruptionReason),
        with_fallback!("RoutineCatchupPolicy", RoutineCatchupPolicy),
        with_fallback!("StreamColor", StreamColor),
        with_fallback!("StreamReviewCadence", StreamReviewCadence),
        with_fallback!("TaskState", TaskState),
        without_fallback!("Weekday", Weekday),
    ]
}

const fn merge_name(merge: Merge) -> &'static str {
    match merge {
        Merge::Lww => "lww",
        Merge::AppendOnly => "append_only",
        Merge::Control => "control",
        Merge::Unsynced => "unsynced",
    }
}

fn owner_value(owner: Owner) -> Value {
    match owner {
        Owner::Meta => json!("meta"),
        Owner::Parent => json!("parent"),
        Owner::Unowned => json!("unowned"),
        Owner::Field(field) => json!({ "field": field }),
    }
}

const fn op_class_name(class: OpClass) -> &'static str {
    match class {
        OpClass::Create => "create",
        OpClass::Update => "update",
        OpClass::Delete => "delete",
    }
}

const fn crdt_name(crdt: Crdt) -> &'static str {
    match crdt {
        Crdt::Register => "register",
        Crdt::Map => "map",
        Crdt::OrSet => "or_set",
        Crdt::Counter => "counter",
        Crdt::Nested => "nested",
        Crdt::Derived => "derived",
    }
}

/// A type as the registry writes it, with the token spacing `stringify!`
/// leaves (`Option < NoteBody >`) removed, and a payload's `Box<…>` removed
/// because serde does not show it on the wire.
fn type_name(written: &str) -> String {
    let compact: String = written.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .strip_prefix("Box<")
        .and_then(|inner| inner.strip_suffix('>'))
        .map_or_else(|| compact.clone(), str::to_owned)
}

/// Every value type the schema describes: the ones registered records carry
/// (`sunrise_domain::registry::VALUE_TYPES`) and the ones the payloads of the
/// ops the registry does not declare carry
/// ([`crate::control_op::PAYLOAD_VALUE_TYPES`]).
fn value_types() -> impl Iterator<Item = &'static ValueSpec> {
    sunrise_domain::registry::VALUE_TYPES
        .iter()
        .chain(crate::control_op::PAYLOAD_VALUE_TYPES)
}

fn value_fields(fields: &[ValueField]) -> Vec<Value> {
    fields
        .iter()
        .map(|f| json!({ "name": f.name, "type": type_name(f.value_type) }))
        .collect()
}

/// One value type as the schema writes it. A tuple's items keep their
/// order, because their position is their name on the wire.
fn value_type_value(spec: &ValueSpec) -> Value {
    match spec.shape {
        ValueShape::Record {
            fields,
            keeps_unknowns,
        } => json!({
            "name": spec.name,
            "shape": "record",
            "keeps_unknowns": keeps_unknowns,
            "fields": value_fields(fields),
        }),
        ValueShape::Alias(ty) => json!({
            "name": spec.name,
            "shape": "alias",
            "type": type_name(ty),
        }),
        ValueShape::Tuple(items) => json!({
            "name": spec.name,
            "shape": "tuple",
            "items": items.iter().map(|t| type_name(t)).collect::<Vec<_>>(),
        }),
        ValueShape::Variants {
            tagging,
            variants,
            keeps_unknowns,
        } => json!({
            "name": spec.name,
            "shape": "variants",
            "tag": match tagging {
                Tagging::External => Value::Null,
                Tagging::Internal(key) => json!(key),
            },
            "keeps_unknowns": keeps_unknowns,
            "variants": variants.iter().map(|v| match v.shape {
                VariantShape::Unit => json!({ "name": v.name }),
                VariantShape::Newtype(ty) => json!({ "name": v.name, "type": type_name(ty) }),
                VariantShape::Fields(fields) => {
                    json!({ "name": v.name, "fields": value_fields(fields) })
                }
            }).collect::<Vec<_>>(),
        }),
    }
}

/// `items` sorted by the string each holds at `key`, so the order they were
/// declared in does not reach the hash.
fn sorted_by(mut items: Vec<Value>, key: &str) -> Vec<Value> {
    items.sort_by(|a, b| a[key].as_str().cmp(&b[key].as_str()));
    items
}

/// The canonical document schema of this build. See the module docs.
pub(crate) fn canonical_schema() -> Value {
    canonical_order(raw_schema())
}

/// Sort every list in a schema by its key: entities by tag, ops by variant,
/// records and fields and enums by name, and the string lists themselves.
fn canonical_order(mut schema: Value) -> Value {
    fn sort_strings(list: &mut Value) {
        if let Value::Array(items) = list {
            let mut names: Vec<String> = items
                .iter()
                .map(|v| v.as_str().expect("a list of names").to_owned())
                .collect();
            names.sort_unstable();
            *items = names.into_iter().map(Value::String).collect();
        }
    }
    fn take_sorted(list: &mut Value, key: &str) -> Vec<Value> {
        let items = match list.take() {
            Value::Array(items) => items,
            _ => Vec::new(),
        };
        sorted_by(items, key)
    }

    let mut entities = take_sorted(&mut schema["entities"], "tag");
    for entity in &mut entities {
        sort_strings(&mut entity["features"]);
        entity["ops"] = Value::Array(take_sorted(&mut entity["ops"], "variant"));
        let mut records = take_sorted(&mut entity["records"], "name");
        for record in &mut records {
            record["fields"] = Value::Array(take_sorted(&mut record["fields"], "name"));
        }
        entity["records"] = Value::Array(records);
    }
    schema["entities"] = Value::Array(entities);
    sort_strings(&mut schema["op_kinds"]);
    schema["op_payloads"] = Value::Array(take_sorted(&mut schema["op_payloads"], "variant"));
    let mut values = take_sorted(&mut schema["values"], "name");
    for value in &mut values {
        if value.get("fields").is_some() {
            value["fields"] = Value::Array(take_sorted(&mut value["fields"], "name"));
        }
        if value.get("variants").is_some() {
            let mut variants = take_sorted(&mut value["variants"], "name");
            for variant in &mut variants {
                if variant.get("fields").is_some() {
                    variant["fields"] = Value::Array(take_sorted(&mut variant["fields"], "name"));
                }
            }
            value["variants"] = Value::Array(variants);
        }
    }
    schema["values"] = Value::Array(values);
    let mut enums = take_sorted(&mut schema["enums"], "name");
    for e in &mut enums {
        sort_strings(&mut e["variants"]);
    }
    schema["enums"] = Value::Array(enums);
    let mut features = take_sorted(&mut schema["features"], "id");
    for f in &mut features {
        sort_strings(&mut f["op_kinds"]);
        sort_strings(&mut f["fields"]);
        sort_strings(&mut f["field_op_kinds"]);
    }
    schema["features"] = Value::Array(features);
    schema
}

/// The schema in the order the code declares it, before [`canonical_order`].
fn raw_schema() -> Value {
    let entities: Vec<Value> = ENTITIES
        .iter()
        .map(|e| {
            json!({
                "tag": e.tag,
                "prefix": e.prefix,
                "merge": merge_name(e.merge),
                "owner": owner_value(e.owner),
                "features": e.features,
                "ops": e.ops.iter().map(|o| json!({
                    "variant": o.variant,
                    "payload": type_name(o.payload),
                    "inner_kind": o.inner_kind,
                    "class": op_class_name(o.class),
                    "target": o.target,
                })).collect::<Vec<_>>(),
                "records": e.records.iter().map(|r| json!({
                    "name": r.name,
                    "keeps_unknowns": r.unknowns.is_some(),
                    "fields": r.fields.iter().map(|f| json!({
                        "name": f.name,
                        "type": type_name(f.value_type),
                        "crdt": crdt_name(f.crdt),
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let enums: Vec<Value> = lossless_enums()
        .into_iter()
        .map(|(name, variants, fallback)| {
            json!({ "name": name, "variants": variants, "fallback": fallback })
        })
        .collect();

    let features: Vec<Value> = crate::feature::FEATURES
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "scope": feature_scope_value(f.id),
                "op_kinds": f.op_kinds,
                "fields": f.fields,
                "field_op_kinds": f.field_op_kinds,
                "since": f.since,
            })
        })
        .collect();

    let op_payloads: Vec<Value> = crate::control_op::OP_PAYLOADS
        .iter()
        .map(|(variant, payload)| json!({ "variant": variant, "payload": type_name(payload) }))
        .collect();

    json!({
        "format": SCHEMA_FORMAT,
        "entities": entities,
        "op_kinds": crate::inner_op::known_kinds(),
        "op_payloads": op_payloads,
        "values": value_types().map(value_type_value).collect::<Vec<_>>(),
        "enums": enums,
        "features": features,
    })
}

/// A feature's scope as the schema writes it: `"structural"`, or the tag of
/// the entity it gates. Read off the id, as every build reads it
/// ([`crate::feature`] §The scope is in the id).
fn feature_scope_value(id: &str) -> Value {
    use crate::feature::FeatureScope;
    match FeatureScope::of(id) {
        FeatureScope::Structural => json!("structural"),
        FeatureScope::Entity(kind) => json!({ "entity": kind.tag() }),
        FeatureScope::UnknownEntity(prefix) => json!({ "entity": prefix }),
    }
}

/// `BLAKE3::derive_key(DOC_SCHEMA_FP_DOMAIN, JCS(schema))`.
pub(crate) fn fingerprint(schema: &Value) -> [u8; 32] {
    let canonical = serde_jcs::to_vec(schema).expect("a schema of strings and integers is JCS");
    blake3::derive_key(DOC_SCHEMA_FP_DOMAIN, &canonical)
}

/// `schemas/doc-schema/current.json` as this build renders it.
pub(crate) fn render_schema() -> String {
    let mut out =
        serde_json::to_string_pretty(&canonical_schema()).expect("a JSON value serializes");
    out.push('\n');
    out
}

/// `schemas/doc-schema/registry.json` as this build renders it: every
/// registered `DOC_SCHEMA_V`, as a decimal string, to its full fingerprint in
/// lowercase hex.
pub(crate) fn render_registry() -> String {
    let entries: serde_json::Map<String, Value> = DOC_SCHEMA_FINGERPRINTS
        .iter()
        .map(|(v, fp)| (v.to_string(), Value::String(hex_lower(fp))))
        .collect();
    let mut out =
        serde_json::to_string_pretty(&Value::Object(entries)).expect("a JSON value serializes");
    out.push('\n');
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn committed_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/doc-schema")
        .join(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use sunrise_cbor::version::{doc_schema_fingerprint, DOC_SCHEMA_V};

    /// Compare a committed file with what this build renders, or write it when
    /// `SUNRISE_REGEN_FIXTURES` is set, the convention the forward-compat
    /// fixture already uses.
    ///
    /// The comparison is of the parsed JSON, not the bytes. The files sit
    /// outside a `generated/` directory, so Biome formats them (`biome.jsonc`
    /// §overrides), and its layout is not serde's; the fingerprint is over the
    /// JCS form for the same reason, so layout is nobody's contract.
    fn check_committed(file: &str, rendered: &str) {
        let path = committed_path(file);
        if std::env::var("SUNRISE_REGEN_FIXTURES").is_ok() {
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
            std::fs::write(&path, rendered).expect("write the committed file");
            return;
        }
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let committed: Value = serde_json::from_str(&committed)
            .unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()));
        let rendered: Value = serde_json::from_str(rendered).expect("rendered JSON parses");
        assert_eq!(
            committed, rendered,
            "schemas/doc-schema/{file} is stale; regenerate it with \
             `SUNRISE_REGEN_FIXTURES=1 cargo test -p sunrise-core --lib doc_schema` \
             and then `mise run fix`"
        );
    }

    #[test]
    fn the_committed_schema_is_current() {
        check_committed("current.json", &render_schema());
    }

    #[test]
    fn the_committed_registry_is_the_builds() {
        check_committed("registry.json", &render_registry());
    }

    /// The test ADR-0045 §2 asks for: a shape change fails here until
    /// `DOC_SCHEMA_V` moves and the new version is registered.
    #[test]
    fn the_schema_fingerprint_is_the_registered_one() {
        let fp = fingerprint(&canonical_schema());
        let registered = doc_schema_fingerprint(u32::from(DOC_SCHEMA_V));
        assert_eq!(
            registered.map(|r| hex_lower(&r)),
            Some(hex_lower(&fp)),
            "the document schema does not match the fingerprint registered for \
             DOC_SCHEMA_V {DOC_SCHEMA_V}. A shape changed: bump DOC_SCHEMA_V, append \
             ({next}, {fp}) to DOC_SCHEMA_FINGERPRINTS and to \
             sunrise_crypto_test_vectors::protocol::DOC_SCHEMA_REGISTRY, then regenerate \
             schemas/doc-schema/. Never edit an existing entry.",
            next = DOC_SCHEMA_V + 1,
            fp = hex_lower(&fp),
        );
    }

    /// The other test ADR-0045 §2 asks for. Every entry is frozen as a literal
    /// in a crate with no dependencies, so an entry that is edited or removed
    /// here fails against an expectation the edit did not touch.
    #[test]
    fn no_registered_fingerprint_changes_or_disappears() {
        use sunrise_crypto_test_vectors::protocol::DOC_SCHEMA_REGISTRY as FROZEN;
        assert_eq!(
            DOC_SCHEMA_FINGERPRINTS, FROZEN,
            "DOC_SCHEMA_FINGERPRINTS is append-only: every frozen entry must be present \
             and unchanged, and a new one is appended to both lists"
        );
    }

    /// The `derive_key` context is frozen beside the registry: a build that
    /// hashed the same schema under another context would register a
    /// different fingerprint for the same shapes.
    #[test]
    fn the_fingerprint_domain_is_the_frozen_one() {
        assert_eq!(
            DOC_SCHEMA_FP_DOMAIN,
            sunrise_crypto_test_vectors::protocol::DOC_SCHEMA_FP_DOMAIN
        );
    }

    /// Versions only ever increase, so the registry is in version order with
    /// no repeats, and the build's own version is its newest entry.
    #[test]
    fn the_registry_is_ordered_and_ends_at_this_build() {
        let versions: Vec<u16> = DOC_SCHEMA_FINGERPRINTS.iter().map(|(v, _)| *v).collect();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
        assert_eq!(versions.last(), Some(&DOC_SCHEMA_V));
    }

    /// The fingerprint is over the JCS form, so the committed file, in
    /// whatever layout Biome gave it, hashes to the registered fingerprint:
    /// anyone can check an entry from the file alone.
    #[test]
    fn the_committed_schema_hashes_to_the_registered_fingerprint() {
        let path = committed_path("current.json");
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let reparsed: Value = serde_json::from_str(&committed).expect("valid JSON");
        assert_eq!(
            Some(fingerprint(&reparsed)),
            doc_schema_fingerprint(u32::from(DOC_SCHEMA_V))
        );
    }

    /// The schema reaches past the registry: every control family the decoder
    /// accepts, and every spelling of an enum, is inside what is hashed.
    #[test]
    fn the_schema_covers_control_ops_and_enum_spellings() {
        let schema = canonical_schema();
        let kinds = schema["op_kinds"].as_array().expect("op kinds");
        assert!(kinds.contains(&json!("IdentityTransition")));
        assert!(kinds.contains(&json!("TaskCreate")));
        let task_state = schema["enums"]
            .as_array()
            .expect("enums")
            .iter()
            .find(|e| e["name"] == "TaskState")
            .expect("TaskState is described");
        assert_eq!(task_state["fallback"], "todo");
        assert!(task_state["variants"]
            .as_array()
            .expect("variants")
            .contains(&json!("done")));
    }

    /// A one-word change anywhere in the schema moves the fingerprint, which
    /// is what makes the registry pin a pin.
    #[test]
    fn any_change_to_the_schema_moves_the_fingerprint() {
        let schema = canonical_schema();
        let mut renamed = schema.clone();
        renamed["entities"][0]["records"][0]["fields"][0]["name"] = json!("renamed");
        assert_ne!(fingerprint(&schema), fingerprint(&renamed));
    }

    /// Declaration order is not on the wire, so reversing every list the
    /// code declares leaves the fingerprint where it was: reordering source
    /// never forces a `DOC_SCHEMA_V` bump.
    #[test]
    fn declaration_order_does_not_reach_the_fingerprint() {
        fn reverse(list: &mut Value) {
            if let Value::Array(items) = list {
                items.reverse();
            }
        }
        let mut raw = raw_schema();
        reverse(&mut raw["entities"]);
        for entity in raw["entities"].as_array_mut().expect("entities") {
            reverse(&mut entity["features"]);
            reverse(&mut entity["ops"]);
            reverse(&mut entity["records"]);
            for record in entity["records"].as_array_mut().expect("records") {
                reverse(&mut record["fields"]);
            }
        }
        reverse(&mut raw["op_kinds"]);
        reverse(&mut raw["op_payloads"]);
        reverse(&mut raw["values"]);
        // `get_mut`, not indexing: indexing a map by a missing key inserts it.
        for value in raw["values"].as_array_mut().expect("values") {
            if let Some(fields) = value.get_mut("fields") {
                reverse(fields);
            }
            if let Some(variants) = value.get_mut("variants") {
                reverse(variants);
                for variant in variants.as_array_mut().expect("variants") {
                    if let Some(fields) = variant.get_mut("fields") {
                        reverse(fields);
                    }
                }
            }
        }
        reverse(&mut raw["enums"]);
        for e in raw["enums"].as_array_mut().expect("enums") {
            reverse(&mut e["variants"]);
        }
        assert_ne!(raw, raw_schema(), "the reversal changed the declared order");
        assert_eq!(
            fingerprint(&canonical_order(raw)),
            fingerprint(&canonical_schema())
        );
    }

    /// The types the schema names without describing: scalars, byte arrays,
    /// the id and time types every record shares, `Unknowns`, a map kept as
    /// raw CBOR, and `CborValue`, any CBOR value (a Preferences entry).
    const LEAF_TYPES: &[&str] = &[
        "CborValue",
        "bool",
        "u8",
        "u32",
        "u64",
        "i32",
        "i64",
        "String",
        "EntityRef",
        "Timestamp",
        "Date",
        "Time",
        "DateTime",
        "ByteBuf",
        "Unknowns",
    ];

    /// The containers a type may be written in, each with one parameter.
    const WRAPPERS: &[&str] = &["Option", "Vec", "BTreeSet", "Box"];

    /// The containers a type may be written in with a key and a value.
    const MAPS: &[&str] = &["BTreeMap"];

    /// `inner` split at its top-level commas: `A,B<C,D>` is `A` and `B<C,D>`.
    fn type_params(inner: &str) -> Vec<&str> {
        let (mut depth, mut start, mut out) = (0usize, 0, Vec::new());
        for (i, c) in inner.char_indices() {
            match c {
                '<' | '[' => depth += 1,
                '>' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        out.push(&inner[start..]);
        out
    }

    /// The named types inside `ty` (already through [`type_name`]), past
    /// every container. A byte array `[u8;N]` names none.
    fn named_types(ty: &str) -> Vec<String> {
        if let Some(array) = ty.strip_prefix('[') {
            assert!(array.starts_with("u8;"), "unexpected array type {ty}");
            return Vec::new();
        }
        match ty.split_once('<') {
            Some((outer, rest)) => {
                let inner = rest.strip_suffix('>').expect("a closed container");
                let params = type_params(inner);
                if MAPS.contains(&outer) {
                    assert_eq!(params.len(), 2, "{outer} takes a key and a value in {ty}");
                } else {
                    assert!(WRAPPERS.contains(&outer), "unexpected container in {ty}");
                    assert_eq!(params.len(), 1, "{outer} takes one parameter in {ty}");
                }
                params.into_iter().flat_map(named_types).collect()
            }
            None => vec![ty.to_owned()],
        }
    }

    /// Every type string anywhere in the schema.
    fn every_type_named(schema: &Value) -> Vec<String> {
        fn walk(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    for (k, v) in map {
                        match (k.as_str(), v) {
                            ("type" | "payload", Value::String(t)) => out.push(t.clone()),
                            ("items", Value::Array(items)) => out
                                .extend(items.iter().filter_map(|i| i.as_str()).map(str::to_owned)),
                            _ => walk(v, out),
                        }
                    }
                }
                Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(schema, &mut out);
        out
    }

    /// The test #439 asks for: a type a record field, a value type or an op
    /// payload names is described in the schema — as a record, a value type
    /// or a lossless enum — or is a leaf. A nested type nobody describes
    /// would change the wire shape without moving the fingerprint.
    #[test]
    fn every_type_the_schema_names_is_described() {
        let schema = canonical_schema();
        let mut described: BTreeSet<String> = LEAF_TYPES.iter().map(|t| (*t).to_owned()).collect();
        for list in ["values", "enums"] {
            described.extend(
                schema[list]
                    .as_array()
                    .expect(list)
                    .iter()
                    .map(|v| v["name"].as_str().expect("a name").to_owned()),
            );
        }
        for entity in schema["entities"].as_array().expect("entities") {
            described.extend(
                entity["records"]
                    .as_array()
                    .expect("records")
                    .iter()
                    .map(|r| r["name"].as_str().expect("a name").to_owned()),
            );
        }
        for ty in every_type_named(&schema) {
            for name in named_types(&ty) {
                assert!(
                    described.contains(&name),
                    "`{name}` (in `{ty}`) is named by the schema and described nowhere. \
                     Describe it in sunrise_domain::registry::VALUE_TYPES or \
                     crate::control_op::PAYLOAD_VALUE_TYPES, or add it to lossless_enums()"
                );
            }
        }
    }

    #[test]
    fn named_types_reach_past_every_container() {
        assert_eq!(
            named_types("Option<Vec<ScheduleConstraint>>"),
            ["ScheduleConstraint"]
        );
        assert_eq!(named_types("BTreeSet<EntityRef>"), ["EntityRef"]);
        assert!(named_types("[u8;32]").is_empty());
    }

    /// Value types and records share one namespace in the schema, so a name
    /// says which shape it is.
    #[test]
    fn value_type_names_are_unique_and_are_not_records() {
        let records: BTreeSet<&str> = ENTITIES
            .iter()
            .flat_map(|e| e.records.iter().map(|r| r.name))
            .collect();
        let mut seen = BTreeSet::new();
        for v in value_types() {
            assert!(seen.insert(v.name), "{} is described twice", v.name);
            assert!(!records.contains(v.name), "{} is also a record", v.name);
        }
    }

    /// The other half of #439's lossy-enum check: `lossy_enum!` is private to
    /// `sunrise-domain`, so every user is an invocation in its sources, and
    /// each must be in [`lossless_enums`]. A test rather than a build
    /// failure because no macro can collect its own call sites.
    #[test]
    fn every_lossy_enum_user_is_described() {
        const CALL: &str = "lossy_enum!(";
        fn sources(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read the domain sources") {
                let path = entry.expect("a directory entry").path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sunrise-domain/src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        let described: BTreeSet<&str> = lossless_enums().iter().map(|(n, _, _)| *n).collect();
        let mut users = BTreeSet::new();
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read a domain source");
            for (at, _) in text.match_indices(CALL) {
                let name: String = text[at + CALL.len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                // The macro's own recursive arm passes `$ty`, which is not a name.
                if !name.is_empty() {
                    users.insert(name);
                }
            }
        }
        assert!(
            users.contains("TaskState"),
            "the scan found the users: {users:?}"
        );
        let missing: Vec<&String> = users
            .iter()
            .filter(|u| !described.contains(u.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "lossy_enum! users missing from doc_schema::lossless_enums(): {missing:?}"
        );
    }

    /// Every `InnerOp` variant is either declared by the entity registry or
    /// in [`crate::control_op::OP_PAYLOADS`], never both, so every op's
    /// payload is described.
    #[test]
    fn every_op_the_decoder_accepts_has_a_described_payload() {
        let registered: BTreeSet<&str> = ENTITIES
            .iter()
            .flat_map(|e| e.ops.iter().map(|o| o.variant))
            .collect();
        let others: BTreeSet<&str> = crate::control_op::OP_PAYLOADS
            .iter()
            .map(|(v, _)| *v)
            .collect();
        assert!(registered.is_disjoint(&others));
        let all: BTreeSet<&str> = registered.union(&others).copied().collect();
        let decoded: BTreeSet<&str> = crate::inner_op::known_kinds().iter().copied().collect();
        assert_eq!(all, decoded);
    }

    #[test]
    fn type_names_drop_spacing_and_a_payload_box() {
        assert_eq!(type_name("Option < NoteBody >"), "Option<NoteBody>");
        assert_eq!(type_name("Box < Routine >"), "Routine");
        assert_eq!(type_name("[u8 ; 32]"), "[u8;32]");
    }
}
