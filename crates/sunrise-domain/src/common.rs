//! Shared scalar types used across multiple entities.

use serde::{Deserialize, Serialize};

/// Energy level facet on Task / Routine. Multi-stream operators sort work by
/// energy in addition to priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Energy {
    /// Low cognitive demand.
    Low,
    /// Medium / balanced.
    Med,
    /// High cognitive demand; deep-work block.
    High,
}

/// Rich-text body. v1 stores a CRDT-text payload as opaque bytes; richer
/// rendering is the UI's job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteBody(#[serde(with = "serde_bytes")] pub Vec<u8>);

impl NoteBody {
    /// Empty body.
    #[must_use]
    pub const fn empty() -> Self {
        Self(Vec::new())
    }

    /// Whether the body is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}
