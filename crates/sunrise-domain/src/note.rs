//! Note entity per `docs/02-domain/notes.md`.
//!
//! Notes are children of Task / Stream / Block. They cannot float free.

use crate::common::NoteBody;
use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Persisted Note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// Note id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Parent entity (Task / Stream / Block).
    pub parent: EntityRef,
    /// Body.
    pub body: NoteBody,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}
