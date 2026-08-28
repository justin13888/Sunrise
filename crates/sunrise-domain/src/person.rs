//! Person entity per `docs/02-domain/people-and-sharing.md`.
//!
//! v1 Persons are first-class identities used for sharing grants and (as
//! informational labels only) Task `assignee`. v1 does not implement
//! delegation; assigning a Task to a non-self Person is a label, not access.

use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Persisted Person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// Person id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Display name (UI; treat as plaintext).
    pub display_name: String,
    /// Optional reference to the Person's identity public key set; absent
    /// means we only hold a label, no identity link.
    #[serde(default)]
    pub identity_id: Option<EntityRef>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}
