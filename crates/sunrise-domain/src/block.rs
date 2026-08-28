//! Block (time-block) entity per `docs/02-domain/time-blocks.md`.
//!
//! A Block is a scheduled time range that may bind to 0..N Tasks. Tasks may
//! reference 0..N Blocks via `Task.blocks` (OR-Set).

use crate::time::SunriseTime;
use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// Persisted Block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Block id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Start time. See [`SunriseTime`]: a block at "09:00" is a different
    /// commitment from a block at a fixed instant, and a device that changes
    /// zone must move one and not the other.
    pub starts_at: SunriseTime,
    /// End time. MUST resolve after `starts_at`.
    pub ends_at: SunriseTime,
    /// Optional title (free text; UI may compose from bound tasks).
    #[serde(default)]
    pub title: Option<String>,
    /// Tasks bound to this Block (OR-Set).
    #[serde(default)]
    pub tasks: BTreeSet<EntityRef>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Draft for creating a Block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDraft {
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Start.
    pub starts_at: SunriseTime,
    /// End.
    pub ends_at: SunriseTime,
    /// Optional title.
    pub title: Option<String>,
    /// Optional tasks to bind on creation.
    pub tasks: Vec<EntityRef>,
}
