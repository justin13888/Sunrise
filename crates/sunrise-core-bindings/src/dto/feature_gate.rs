//! Feature gate: what a write needs that this build or vault lacks.

/// What a missing feature locks on this build (ADR-0045 §8).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FeatureLock {
    /// A `core.*` feature: every entity edit in the vault.
    Everything,
    /// Edits to one entity kind, by its registry tag (`"task"`,
    /// `"focus_session"`, …).
    Entity {
        /// The entity tag.
        tag: String,
    },
    /// A feature of an entity kind this build does not have. Nothing here
    /// writes that kind, so nothing is locked; the feature is still missing,
    /// and the banner still shows.
    Nothing {
        /// The prefix of the feature id, which names that entity.
        entity: String,
    },
}

/// A feature the vault requires that this build does not have.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MissingFeatureItem {
    /// The feature id, as the vault names it.
    pub feature: String,
    /// What it locks here.
    pub lock: FeatureLock,
}

impl From<&sunrise_core::MissingFeature> for MissingFeatureItem {
    fn from(m: &sunrise_core::MissingFeature) -> Self {
        Self::from(sunrise_client_core::gate::Missing::from(m))
    }
}

impl From<sunrise_client_core::gate::Missing> for MissingFeatureItem {
    fn from(m: sunrise_client_core::gate::Missing) -> Self {
        use sunrise_client_core::gate::Lock;
        let lock = match m.lock {
            Lock::Everything => FeatureLock::Everything,
            Lock::Entity { tag } => FeatureLock::Entity { tag },
            Lock::Nothing { entity } => FeatureLock::Nothing { entity },
        };
        Self {
            feature: m.feature,
            lock,
        }
    }
}

/// What this build may edit in this vault, decided once in Rust so every
/// client disables the same actions.
///
/// `read_only` is the banner: "Update Sunrise to edit" shows whenever any
/// required feature is missing, including one that locks nothing here,
/// because the user is still missing data this build cannot show. `locks_all`
/// and `locked_tags` say which edit actions to disable. A command on a locked
/// kind is refused with [`crate::BindingError::FeatureMissing`] whether or not
/// the client disabled it first.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct EditGate {
    /// Whether to show the read-only banner.
    pub read_only: bool,
    /// Whether every entity edit is locked.
    pub locks_all: bool,
    /// The entity tags whose edits are locked, sorted. Empty when
    /// `locks_all` is set, which already covers every one.
    pub locked_tags: Vec<String>,
    /// Every missing feature, sorted by id, for a details view.
    pub missing: Vec<MissingFeatureItem>,
}

impl EditGate {
    /// The gate for `missing`, as [`sunrise_core::Core::missing_features`]
    /// returns it.
    #[must_use]
    pub fn from_missing(missing: &[sunrise_core::MissingFeature]) -> Self {
        let gate = sunrise_client_core::gate::EditGate::from_missing(missing);
        Self {
            read_only: gate.read_only,
            locks_all: gate.locks_all,
            locked_tags: gate.locked_tags,
            missing: gate.missing.into_iter().map(Into::into).collect(),
        }
    }
}
