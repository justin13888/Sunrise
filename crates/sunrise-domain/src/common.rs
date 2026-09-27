//! Shared scalar types used across multiple entities.

use crate::unknown::UnknownVariant;
use serde::{Deserialize, Serialize};

/// Energy level facet on Task / Routine. Multi-stream operators sort work by
/// energy in addition to priority.
///
/// An unrecognised value reads as [`Energy::Med`] and is written back
/// verbatim. The middle rung: an unknown energy must not make a task look
/// unusually cheap or unusually expensive to the planner.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Energy {
    /// Low cognitive demand.
    Low,
    /// Medium / balanced.
    Med,
    /// High cognitive demand; deep-work block.
    High,
    /// An energy this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(Energy, fallback = Med, {
    Low => "low",
    Med => "med",
    High => "high",
});

/// Rich-text body, stored as opaque bytes; richer rendering is the UI's job.
///
/// The bytes are **not** a CRDT-text payload. ADR-0014 replaced per-field
/// character-level merge with entity-level last-writer-wins, and this
/// workspace has no CRDT layer for a note body to be encoded against — a
/// concurrent edit of a body resolves by the same LWW rule as every other
/// field.
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
