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
//!   the control families are covered as well as the registry's;
//! - every lossless string enum on the wire, with the spellings its
//!   `lossy_enum!` table holds and the arm an unknown value reads as.
//!
//! What it does not hold is what is not part of the document's shape: storage
//! tables and columns, Rust field names, declaration comments.
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
use sunrise_id::registry::{Crdt, Merge, OpClass, Owner, ENTITIES};

/// The version of this document's own layout. Bumped only if the schema's
/// JSON structure changes, which also moves every fingerprint.
const SCHEMA_FORMAT: u64 = 1;

/// Every lossless string enum that crosses the wire, as
/// `(name, known spellings, fallback spelling)`.
///
/// The spellings and the fallback are read from each enum's own
/// `lossy_enum!` table, so a variant added there reaches the schema. The list
/// of enums is the one part written here: a new `lossy_enum!` user must be
/// added to it.
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

/// The canonical document schema of this build. See the module docs.
pub(crate) fn canonical_schema() -> Value {
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

    json!({
        "format": SCHEMA_FORMAT,
        "entities": entities,
        "op_kinds": crate::inner_op::known_kinds(),
        "enums": enums,
    })
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

    #[test]
    fn type_names_drop_spacing_and_a_payload_box() {
        assert_eq!(type_name("Option < NoteBody >"), "Option<NoteBody>");
        assert_eq!(type_name("Box < Routine >"), "Routine");
        assert_eq!(type_name("[u8 ; 32]"), "[u8;32]");
    }
}
