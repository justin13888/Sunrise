//! Context entity per `docs/02-domain/contexts-and-tags.md`.

use crate::validation::{validate_title, ValidationError, MAX_CONTEXT_NAME_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Persisted Context (cross-cutting tag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// Context id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Display name (e.g., `@deep-work`).
    pub name: String,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
}

impl Context {
    /// Trim and validate the name.
    pub fn validate_name(name: &str) -> Result<String, ValidationError> {
        validate_title(name, "context.name", MAX_CONTEXT_NAME_LEN)
    }
}
