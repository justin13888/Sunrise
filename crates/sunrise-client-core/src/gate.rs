//! What this build may edit in a vault (ADR-0045 §8), decided once so every
//! client disables the same actions.
//!
//! The core refuses a write that needs a feature this build lacks whatever a
//! client does; the gate is what lets a client say so *before* the user acts.
//! `read_only` is the banner: "Update Sunrise to edit" shows whenever any
//! required feature is missing, including one that locks nothing here,
//! because the user is still missing data this build cannot show.

use sunrise_core::{FeatureScope, MissingFeature};

/// What one missing feature locks in this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lock {
    /// A structural feature: every edit is locked.
    Everything,
    /// A feature of an entity kind this build has: edits to that kind are
    /// locked. `tag` is the entity tag (`"task"`, `"stream"`, …).
    Entity {
        /// The entity tag.
        tag: String,
    },
    /// A feature of an entity kind this build does not have. Nothing here
    /// writes that kind, so nothing is locked; the banner still shows.
    Nothing {
        /// The prefix of the feature id, which names that entity.
        entity: String,
    },
}

/// One missing feature and what it locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    /// The feature id, as the vault names it.
    pub feature: String,
    /// What it locks here.
    pub lock: Lock,
}

impl From<&MissingFeature> for Missing {
    fn from(m: &MissingFeature) -> Self {
        let lock = match &m.scope {
            FeatureScope::Structural => Lock::Everything,
            FeatureScope::Entity(kind) => Lock::Entity {
                tag: kind.tag().to_owned(),
            },
            FeatureScope::UnknownEntity(prefix) => Lock::Nothing {
                entity: prefix.clone(),
            },
        };
        Self {
            feature: m.id.clone(),
            lock,
        }
    }
}

/// The gate for one vault, as [`sunrise_core::Core::missing_features`]
/// reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditGate {
    /// Whether to show the read-only banner.
    pub read_only: bool,
    /// Whether every entity edit is locked.
    pub locks_all: bool,
    /// The entity tags whose edits are locked, sorted and deduplicated. Empty
    /// when `locks_all` is set, which already covers every one.
    pub locked_tags: Vec<String>,
    /// Every missing feature, in the order the core reported them.
    pub missing: Vec<Missing>,
}

impl EditGate {
    /// The gate for `missing`.
    #[must_use]
    pub fn from_missing(missing: &[MissingFeature]) -> Self {
        let missing: Vec<Missing> = missing.iter().map(Into::into).collect();
        let locks_all = missing.iter().any(|m| matches!(m.lock, Lock::Everything));
        let mut locked_tags: Vec<String> = if locks_all {
            Vec::new()
        } else {
            missing
                .iter()
                .filter_map(|m| match &m.lock {
                    Lock::Entity { tag } => Some(tag.clone()),
                    _ => None,
                })
                .collect()
        };
        locked_tags.sort_unstable();
        locked_tags.dedup();
        Self {
            read_only: !missing.is_empty(),
            locks_all,
            locked_tags,
            missing,
        }
    }

    /// Whether edits to the entity tagged `tag` are open.
    #[must_use]
    pub fn allows(&self, tag: &str) -> bool {
        edit_allowed(self.locks_all, &self.locked_tags, tag)
    }
}

/// The rule [`EditGate::allows`] applies, over the two fields it reads, for a
/// client that holds them in its own record.
#[must_use]
pub fn edit_allowed(locks_all: bool, locked_tags: &[String], tag: &str) -> bool {
    !(locks_all || locked_tags.iter().any(|t| t == tag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn missing(id: &str, scope: FeatureScope) -> MissingFeature {
        MissingFeature {
            id: id.into(),
            scope,
        }
    }

    #[test]
    fn nothing_missing_is_an_open_vault() {
        let gate = EditGate::from_missing(&[]);
        assert_eq!(gate, EditGate::default());
        assert!(gate.allows("task"));
    }

    #[test]
    fn an_entity_feature_locks_that_entity_only() {
        let gate =
            EditGate::from_missing(&[missing("task.x", FeatureScope::Entity(EntityKind::Task))]);
        assert!(gate.read_only);
        assert!(!gate.locks_all);
        assert_eq!(gate.locked_tags, vec!["task".to_owned()]);
        assert_eq!(
            gate.missing,
            vec![Missing {
                feature: "task.x".into(),
                lock: Lock::Entity { tag: "task".into() },
            }]
        );
        assert!(!gate.allows("task"));
        assert!(gate.allows("stream"));
    }

    #[test]
    fn a_structural_feature_locks_everything() {
        let gate = EditGate::from_missing(&[
            missing("core.x", FeatureScope::Structural),
            missing("task.x", FeatureScope::Entity(EntityKind::Task)),
        ]);
        assert!(gate.locks_all);
        assert!(gate.locked_tags.is_empty());
        assert!(!gate.allows("stream"));
    }

    #[test]
    fn two_features_of_one_entity_lock_it_once() {
        let gate = EditGate::from_missing(&[
            missing("task.y", FeatureScope::Entity(EntityKind::Task)),
            missing("stream.x", FeatureScope::Entity(EntityKind::Stream)),
            missing("task.x", FeatureScope::Entity(EntityKind::Task)),
        ]);
        assert_eq!(
            gate.locked_tags,
            vec!["stream".to_owned(), "task".to_owned()]
        );
    }

    /// A feature of an entity this build lacks locks nothing, and the banner
    /// still shows: the user is missing data this build cannot display.
    #[test]
    fn an_unknown_entitys_feature_shows_the_banner_and_locks_nothing() {
        let gate = EditGate::from_missing(&[missing(
            "place.entity",
            FeatureScope::UnknownEntity("place".into()),
        )]);
        assert!(gate.read_only);
        assert!(!gate.locks_all);
        assert!(gate.locked_tags.is_empty());
        assert!(gate.allows("task"));
    }
}
