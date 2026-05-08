//! Attachment metadata per `spec/02-domain/attachments.md`.
//!
//! The blob bytes themselves live in the Stream's blob store (chunked &
//! encrypted; see `spec/03-crypto/data-encryption-format.md` §blob-chunks).
//! This entity carries only the per-attachment metadata.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Persisted Attachment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Parent entity (Task usually).
    pub parent: EntityRef,
    /// Filename (informational).
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// Total plaintext size in bytes.
    pub size_bytes: u64,
    /// Per-blob symmetric key (32 bytes), used to seal/open chunks.
    /// On the wire this is sealed inside the parent's encrypted op envelope.
    #[serde(with = "serde_bytes")]
    pub blob_key: [u8; 32],
    /// 16-byte blob id assigned by the creating device.
    #[serde(with = "serde_bytes")]
    pub blob_id: [u8; 16],
    /// Number of 256 KiB chunks (last may be shorter).
    pub chunk_count: u32,
    /// BLAKE3 of the concatenated plaintext (32 bytes), checked after
    /// reassembly.
    #[serde(with = "serde_bytes")]
    pub content_hash: [u8; 32],
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
}
