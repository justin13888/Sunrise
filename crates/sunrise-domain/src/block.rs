//! Block (time-block) entity per `docs/02-domain/time-blocks.md`.
//!
//! A Block is a scheduled time range that may bind to 0..N Tasks. Tasks may
//! reference 0..N Blocks via `Task.blocks` (OR-Set).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// Persisted Block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Block id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Start time.
    pub starts_at: DateTime<Utc>,
    /// End time. MUST be > `starts_at`.
    pub ends_at: DateTime<Utc>,
    /// Optional title (free text; UI may compose from bound tasks).
    #[serde(default)]
    pub title: Option<String>,
    /// Tasks bound to this Block (OR-Set).
    #[serde(default)]
    pub tasks: BTreeSet<EntityRef>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
}

/// Draft for creating a Block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDraft {
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Start.
    pub starts_at: DateTime<Utc>,
    /// End.
    pub ends_at: DateTime<Utc>,
    /// Optional title.
    pub title: Option<String>,
    /// Optional tasks to bind on creation.
    pub tasks: Vec<EntityRef>,
}
