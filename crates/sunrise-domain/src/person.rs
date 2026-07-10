//! Person entity per `docs/02-domain/people-and-sharing.md`.
//!
//! v1 Persons are first-class identities used for sharing grants and (as
//! informational labels only) Task `assignee`. v1 does not implement
//! delegation; assigning a Task to a non-self Person is a label, not access.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Persisted Person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// Person id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Display name (UI; treat as plaintext).
    pub display_name: String,
    /// Optional reference to the Person's identity public key set; absent
    /// means we only hold a label, no identity link.
    #[serde(default)]
    pub identity_id: Option<EntityRef>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
}
