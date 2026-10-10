//! The feature registry and the read-only gate (ADR-0045 §7–§8, issue #324).
//!
//! A **feature** is a stable id naming something a vault's data can depend
//! on: an op kind, a field, a field-op kind. When a client first writes data
//! that uses a feature it emits a `VaultRequires` control op naming it, and
//! the vault's required set is the union of every one applied. A build that
//! lacks a required feature keeps syncing and keeps reading, and refuses
//! local writes on the feature's scope, so it can never overwrite data it
//! cannot represent.
//!
//! # The scope is in the id
//!
//! `core.<name>` is **structural**: it locks every entity write in the vault.
//! `<entity>.<name>` is **entity-scoped**: it locks writes to the entity
//! whose registry tag is `<entity>` (`task`, `focus_session`, …).
//!
//! The scope is read off the id rather than stored beside it, because the
//! build that needs the scope is exactly the build that does not have the
//! feature's registry entry. A scope carried only in the registry would be
//! unknowable to the one reader it exists for. So a feature that changes how
//! two entities are written is either structural or two features, one per
//! entity.
//!
//! An id whose prefix names no entity this build knows is a feature of an
//! entity a newer build added. This build has no command that writes that
//! entity, so the feature locks nothing here; it is still reported, so the
//! user is told to update.
//!
//! # Where the registry lives
//!
//! [`FEATURES`] is the build's list. Each entity-scoped id is also declared
//! on its entity in [`sunrise_id::for_each_entity!`], and a test holds the
//! two equal. The canonical document schema (`crate::doc_schema`) hashes the
//! list, so adding a feature is a `DOC_SCHEMA_V` bump, as ADR-0045 §1 says a
//! feature id is.
//!
//! A feature adds its entry here and emits `VaultRequires` through
//! [`crate::Engine::require_features`] before its first op. The first is
//! `preferences.entity` (ADR-0050). An `<entity>.entity` feature is used by
//! every `Patch` on that entity, read off the `ref`'s prefix, so an entity
//! written only by `Patch` needs no op kind or field to be gated.

use sunrise_id::registry::Merge;
use sunrise_id::EntityKind;

/// One feature this build supports (ADR-0045 §7).
///
/// Ids are never reused or renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feature {
    /// The stable id, `[a-z][a-z0-9_]*(\.[a-z0-9_]+)+`. Its first segment is
    /// its scope; see the [module docs](self).
    pub id: &'static str,
    /// The `InnerOp` variant names it introduces.
    pub op_kinds: &'static [&'static str],
    /// The field names (CBOR map keys of an op's payload) it introduces.
    pub fields: &'static [&'static str],
    /// The field-op kinds it introduces (ADR-0044 §8). None exist yet.
    pub field_op_kinds: &'static [&'static str],
    /// The `DOC_SCHEMA_V` it arrived in.
    pub since: u16,
}

/// Every feature this build supports.
pub static FEATURES: &[Feature] = &[
    // The vault's Preferences entity (ADR-0050, issue #337). It has no op
    // kind or field of its own: every op on it is a `Patch` whose `ref` is a
    // `prf_` id, which `features_used` reads as this feature.
    Feature {
        id: "preferences.entity",
        op_kinds: &[],
        fields: &[],
        field_op_kinds: &[],
        since: 11,
    },
    // An attachment's source-made thumbnail and the original's dimensions
    // (ADR-0053 §2, issue #346). Eight optional fields on `AttachmentCreate`
    // and `AttachmentDelete`; an op that carries none of them uses nothing.
    Feature {
        id: "attachment.thumbnail",
        op_kinds: &[],
        fields: &[
            "width",
            "height",
            "thumbnail_blob_id",
            "thumbnail_blob_key",
            "thumbnail_mime",
            "thumbnail_size_bytes",
            "thumbnail_content_hash",
            "thumbnail_ciphertext_hash",
        ],
        field_op_kinds: &[],
        since: 12,
    },
];

/// What a feature locks when a build lacks it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FeatureScope {
    /// `core.*`: every entity write in the vault.
    Structural,
    /// `<entity>.*`, for an entity this build knows.
    Entity(EntityKind),
    /// `<entity>.*`, for an entity this build does not know. Carries the
    /// prefix. Locks nothing here, because nothing here writes it.
    UnknownEntity(String),
}

impl FeatureScope {
    /// The scope a feature id names.
    ///
    /// Total: a malformed id reads as [`Self::UnknownEntity`] of whatever
    /// precedes its first dot. Nothing malformed is ever stored, because
    /// `VaultRequires` is applied through [`is_feature_id`] first.
    #[must_use]
    pub fn of(id: &str) -> Self {
        let prefix = id.split('.').next().unwrap_or(id);
        if prefix == "core" {
            return Self::Structural;
        }
        EntityKind::all()
            .into_iter()
            .find(|k| k.tag() == prefix)
            .map_or_else(|| Self::UnknownEntity(prefix.to_owned()), Self::Entity)
    }

    /// Whether this scope refuses a local write to `kind`.
    ///
    /// Only kinds whose ops carry user data are ever refused. Control kinds
    /// (`device`, `identity`) are what revocation, rotation and pairing write,
    /// and ADR-0045 §8 keeps those allowed whatever the vault requires:
    /// refusing them would harm the vault more than any feature mismatch.
    #[must_use]
    pub fn locks(&self, kind: EntityKind) -> bool {
        if !matches!(kind.spec().merge, Merge::Lww | Merge::AppendOnly) {
            return false;
        }
        match self {
            Self::Structural => true,
            Self::Entity(k) => *k == kind,
            Self::UnknownEntity(_) => false,
        }
    }
}

/// A feature the vault requires and this build does not support.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MissingFeature {
    /// The feature id, as the vault names it.
    pub id: String,
    /// What it locks on this build.
    pub scope: FeatureScope,
}

/// Whether `s` is a well-formed feature id:
/// `[a-z][a-z0-9_]*(\.[a-z0-9_]+)+`, the CDDL of ADR-0045 §7.
#[must_use]
pub fn is_feature_id(s: &str) -> bool {
    let mut segments = s.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    let mut first_chars = first.chars();
    if !first_chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return false;
    }
    let tail_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
    if !first_chars.all(tail_ok) {
        return false;
    }
    let mut rest = 0usize;
    for segment in segments {
        if segment.is_empty() || !segment.chars().all(tail_ok) {
            return false;
        }
        rest += 1;
    }
    rest > 0
}

/// The ids in `registry` that the inner-op blob `inner` uses: its variant is
/// one of a feature's op kinds, or its payload carries one of a feature's
/// fields with a value other than null.
///
/// Read from the encoded op rather than the typed one, so it sees exactly
/// what goes on the wire, and so the seal path, which holds only the bytes,
/// can ask. A blob that is not an externally tagged op uses nothing.
pub(crate) fn features_used(registry: &[Feature], inner: &[u8]) -> Vec<&'static str> {
    use ciborium::value::Value;
    if registry.is_empty() {
        return Vec::new();
    }
    let Ok(Value::Map(mut entries)) = ciborium::de::from_reader::<Value, _>(inner) else {
        return Vec::new();
    };
    if entries.len() != 1 {
        return Vec::new();
    }
    let Some((Value::Text(variant), payload)) = entries.pop() else {
        return Vec::new();
    };
    // A `Patch` names its entity by the prefix of its `ref`; the entity's
    // `<tag>.entity` feature is used by every one of them.
    let patched_entity: Option<String> = match (&*variant, &payload) {
        ("Patch", Value::Map(fields)) => fields
            .iter()
            .find(|(k, _)| k.as_text() == Some("ref"))
            .and_then(|(_, v)| v.as_text())
            .and_then(|r| r.get(..4))
            .and_then(EntityKind::from_prefix)
            .map(|k| format!("{}.entity", k.tag())),
        _ => None,
    };
    let carried: Vec<String> = match payload {
        Value::Map(fields) => fields
            .into_iter()
            .filter(|(_, v)| !matches!(v, Value::Null))
            .filter_map(|(k, _)| match k {
                Value::Text(name) => Some(name),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    registry
        .iter()
        .filter(|f| {
            f.op_kinds.contains(&variant.as_str())
                || f.fields
                    .iter()
                    .any(|name| carried.iter().any(|c| c == name))
                || patched_entity.as_deref() == Some(f.id)
        })
        .map(|f| f.id)
        .collect()
}

/// The features in `required` that `supported` lacks, each with its scope,
/// sorted by id.
pub(crate) fn missing<'a>(
    required: impl IntoIterator<Item = &'a str>,
    supported: &[Feature],
) -> Vec<MissingFeature> {
    let mut out: Vec<MissingFeature> = required
        .into_iter()
        .filter(|id| !supported.iter().any(|f| f.id == *id))
        .map(|id| MissingFeature {
            id: id.to_owned(),
            scope: FeatureScope::of(id),
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The first missing feature that refuses a local write to an op whose
/// op-log `target_kind` is `target_kind`, if any.
///
/// A target kind that is no entity's tag (`stream_key`, `vault`) is a control
/// op's, and is never refused.
pub(crate) fn refusal<'a>(missing: &'a [MissingFeature], target_kind: &str) -> Option<&'a str> {
    let kind = EntityKind::all()
        .into_iter()
        .find(|k| k.tag() == target_kind)?;
    missing
        .iter()
        .find(|m| m.scope.locks(kind))
        .map(|m| m.id.as_str())
}

/// Refusal raised from inside a write transaction when an op would land on a
/// scope a missing feature locks, or would use a feature the vault has not
/// been told about.
///
/// Carried out of the transaction boxed in a `rusqlite` error,
/// because the seal path returns `rusqlite::Result`, and lifted back into a
/// typed [`crate::EngineError`] by [`crate::Engine::apply`]. The transaction
/// it aborts rolls back, so a refused command leaves nothing behind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SealRefusal {
    /// The vault requires `0`, this build lacks it, and its scope covers the
    /// op's entity.
    #[error("the vault requires feature `{0}`, which this build does not have")]
    Missing(String),
    /// The op uses feature `0` and no `VaultRequires` naming it has been
    /// applied. A bug in the feature's command path, never a user error.
    #[error("an op uses feature `{0}` before the vault requires it")]
    NotRequired(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_ids_follow_the_cddl() {
        for ok in [
            "core.field_ops",
            "task.optional_stream",
            "focus_session.breaks",
            "a.b",
            "x1_.y.z9",
        ] {
            assert!(is_feature_id(ok), "{ok}");
        }
        for bad in [
            "",
            "core",
            ".x",
            "core.",
            "core..x",
            "Core.x",
            "1core.x",
            "_core.x",
            "core.X",
            "core.field-ops",
            "core. x",
        ] {
            assert!(!is_feature_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_scope_is_the_first_segment() {
        assert_eq!(FeatureScope::of("core.field_ops"), FeatureScope::Structural);
        assert_eq!(
            FeatureScope::of("task.deadlines_v2"),
            FeatureScope::Entity(EntityKind::Task)
        );
        assert_eq!(
            FeatureScope::of("focus_session.breaks"),
            FeatureScope::Entity(EntityKind::FocusSession)
        );
        assert_eq!(
            FeatureScope::of("place.entity"),
            FeatureScope::UnknownEntity("place".into())
        );
    }

    #[test]
    fn a_scope_locks_user_data_and_never_control_kinds() {
        let structural = FeatureScope::Structural;
        assert!(structural.locks(EntityKind::Task));
        assert!(structural.locks(EntityKind::FocusSession));
        assert!(
            !structural.locks(EntityKind::Device),
            "revocation stays allowed"
        );
        assert!(
            !structural.locks(EntityKind::Identity),
            "rotation stays allowed"
        );

        let task = FeatureScope::Entity(EntityKind::Task);
        assert!(task.locks(EntityKind::Task));
        assert!(!task.locks(EntityKind::Stream));

        assert!(!FeatureScope::UnknownEntity("place".into()).locks(EntityKind::Task));
    }

    #[test]
    fn missing_is_what_the_vault_requires_and_the_build_lacks() {
        static HAVE: &[Feature] = &[Feature {
            id: "task.have",
            op_kinds: &[],
            fields: &[],
            field_op_kinds: &[],
            since: 8,
        }];
        let got = missing(
            ["task.have", "core.lack", "core.lack", "place.entity"],
            HAVE,
        );
        assert_eq!(
            got,
            vec![
                MissingFeature {
                    id: "core.lack".into(),
                    scope: FeatureScope::Structural
                },
                MissingFeature {
                    id: "place.entity".into(),
                    scope: FeatureScope::UnknownEntity("place".into())
                },
            ]
        );
        assert_eq!(refusal(&got, "task"), Some("core.lack"));
        assert_eq!(refusal(&got, "device"), None);
        assert_eq!(refusal(&got, "stream_key"), None, "not an entity at all");
    }

    #[test]
    fn features_used_reads_the_variant_and_the_non_null_fields() {
        use ciborium::value::Value;
        static REG: &[Feature] = &[
            Feature {
                id: "task.by_kind",
                op_kinds: &["TaskSplit"],
                fields: &[],
                field_op_kinds: &[],
                since: 8,
            },
            Feature {
                id: "task.by_field",
                op_kinds: &[],
                fields: &["deadline_v2"],
                field_op_kinds: &[],
                since: 8,
            },
        ];
        let op = |variant: &str, fields: Vec<(&str, Value)>| {
            let payload = Value::Map(
                fields
                    .into_iter()
                    .map(|(k, v)| (Value::Text(k.into()), v))
                    .collect(),
            );
            let mut buf = Vec::new();
            ciborium::ser::into_writer(
                &Value::Map(vec![(Value::Text(variant.into()), payload)]),
                &mut buf,
            )
            .unwrap();
            buf
        };
        assert_eq!(
            features_used(REG, &op("TaskSplit", vec![])),
            ["task.by_kind"]
        );
        assert_eq!(
            features_used(
                REG,
                &op(
                    "TaskUpdate",
                    vec![("deadline_v2", Value::Integer(1.into()))]
                )
            ),
            ["task.by_field"]
        );
        assert!(
            features_used(REG, &op("TaskUpdate", vec![("deadline_v2", Value::Null)])).is_empty(),
            "a null field is an absent one"
        );
        assert!(features_used(REG, &op("TaskUpdate", vec![("title", Value::Null)])).is_empty());
        assert!(features_used(&[], &op("TaskSplit", vec![])).is_empty());
        assert!(features_used(REG, &[0xff]).is_empty());
    }

    /// The registry's own invariants: every id is well formed and unique, and
    /// each entity-scoped id is declared on its entity in the entity registry,
    /// and nothing else is.
    #[test]
    fn the_registry_agrees_with_the_entity_registry() {
        let mut seen = std::collections::BTreeSet::new();
        for f in FEATURES {
            assert!(is_feature_id(f.id), "{} is not a feature id", f.id);
            assert!(seen.insert(f.id), "{} is registered twice", f.id);
            assert!(
                !matches!(FeatureScope::of(f.id), FeatureScope::UnknownEntity(_)),
                "{} names no entity this build has",
                f.id
            );
        }
        for spec in &sunrise_id::registry::ENTITIES {
            let declared: std::collections::BTreeSet<&str> =
                spec.features.iter().copied().collect();
            let registered: std::collections::BTreeSet<&str> = FEATURES
                .iter()
                .filter(|f| FeatureScope::of(f.id) == FeatureScope::Entity(spec.kind))
                .map(|f| f.id)
                .collect();
            assert_eq!(declared, registered, "features of `{}`", spec.tag);
        }
    }
}
