//! Read queries.

use serde::{Deserialize, Serialize};
use sunrise_domain::{Stream, Task};
use sunrise_id::EntityRef;

/// Read query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Query {
    /// Today view: scheduled blocks, due-today tasks, manually-pulled tasks.
    Today {
        /// "Now" (ms since epoch).
        now_ms: u64,
        /// Filter to these contexts (empty means all).
        contexts: Vec<EntityRef>,
    },
    /// Inbox.
    Inbox,
    /// All tasks in a single Stream.
    StreamTasks(EntityRef),
    /// One entity by id.
    EntityById(EntityRef),
    /// Connected device list.
    DeviceList,
    /// Snapshot of sync status.
    SyncStatus,
}

/// Query result. (Not `Deserialize`: some inner types use Cow/static
/// references in their `serde` impls. Query results are one-direction
/// across the FFI seam.)
#[derive(Debug, Clone, Serialize)]
pub enum QueryResult {
    /// `Today` returns a flat list of tasks (UI groups them).
    Tasks(Vec<Task>),
    /// `Inbox` and `StreamTasks` return tasks.
    StreamTasks(Vec<Task>),
    /// `EntityById` may return a stream.
    Stream(Box<Stream>),
    /// `EntityById` may return a task.
    Task(Box<Task>),
    /// Device list rows: (device_id, nickname, platform, is_revoked).
    Devices(Vec<DeviceRow>),
    /// Sync status snapshot.
    SyncStatus(crate::events::SyncStatus),
}

/// One row of [`Query::DeviceList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    /// Device id.
    pub device_id: [u8; 16],
    /// Human-readable nickname.
    pub nickname: String,
    /// Platform string.
    pub platform: String,
    /// Revoked.
    pub revoked: bool,
}
