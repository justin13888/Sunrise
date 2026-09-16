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

/// How one attachment's bytes stand on **this** device.
///
/// `docs/02-domain/attachments.md` §Lazy fetch describes three client states
/// for an attachment whose metadata is here and whose bytes may not be, and
/// this is the half of that a client cannot work out from
/// [`crate::Core::attachment_is_local`] alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentFetchState {
    /// No request outstanding. With local bytes this is an attachment that
    /// opens; without them it is the placeholder with the Download button.
    Idle,
    /// A client asked for the bytes and the transfer has not finished. The
    /// state the Cancel button belongs to.
    Requested,
    /// The document's `partial: true`: a transfer was started and abandoned,
    /// by a cancel or by a relay that could not serve it. Re-asking restarts
    /// from byte 0.
    Partial,
}

/// What became of an attachment fetch a client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentFetchOutcome {
    /// Every chunk is in this device's blob store; the attachment opens.
    Fetched,
    /// A client called [`crate::Core::cancel_attachment_fetch`]. The request is
    /// now [`AttachmentFetchState::Partial`].
    Cancelled,
    /// The relay could not serve the blob within the attempt ceiling. Also
    /// [`AttachmentFetchState::Partial`]: the two are the same fact about the
    /// cache, and differ only in who stopped it.
    Unavailable,
}

/// One finished attachment fetch, streamed via
/// [`crate::Core::attachment_fetches`].
///
/// Only *finished* ones. There is no progress event, because there is no
/// progress to report: the relay's `GET /blobs/{id}` is one response body, and
/// a client that drew a percentage from anything would be drawing a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentFetch {
    /// The attachment whose bytes were asked for.
    pub attachment: EntityRef,
    /// What happened to them.
    pub outcome: AttachmentFetchOutcome,
}
