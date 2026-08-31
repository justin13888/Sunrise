//! Domain events + sync status streamed to the UI.

use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;
use sunrise_sync::SyncState;

/// One domain event published to subscribers via `Core::changes()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DomainEvent {
    /// An entity was created.
    Created(EntityRef),
    /// An entity was updated.
    Updated(EntityRef),
    /// An entity was deleted (soft-delete tombstone).
    Deleted(EntityRef),
    /// An entity is fully gone (post-compaction).
    Forgotten(EntityRef),
}

/// Snapshot of sync status. Streamed via `Core::sync_status()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    /// Current state.
    pub state: SyncState,
    /// Outbound queue depth.
    pub outbox_pending: u32,
    /// `n` peer devices known.
    pub peer_devices: u32,
    /// Last successful sync time (ms since epoch); `None` if never.
    pub last_sync_ms: Option<u64>,
}
