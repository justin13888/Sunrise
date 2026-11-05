//! Attachments: a blob's record and the fixed-width digests it is named by.

use sunrise_domain::Attachment;
use sunrise_id::EntityRef;

use super::hex16;

/// See [`sunrise_domain::Attachment`]: metadata for one encrypted blob.
///
/// `blob_key` crosses the seam because it has to — the bytes in the blob store
/// are sealed under it and unreadable without it. It never leaves the device
/// except inside an op envelope.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AttachmentItem {
    /// Attachment id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Parent task.
    pub parent: EntityRef,
    /// Filename, informational.
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// Plaintext size in bytes.
    pub size_bytes: u64,
    /// Per-blob symmetric key, 32 bytes.
    pub blob_key: Vec<u8>,
    /// Blob id, 16 bytes, lowercase hex.
    pub blob_id: String,
    /// Chunk count.
    pub chunk_count: u32,
    /// BLAKE3 of the concatenated plaintext, 32 bytes, lowercase hex.
    pub content_hash: String,
    /// BLAKE3 of the concatenated ciphertext, 32 bytes, lowercase hex: the
    /// name the relay knows this blob by. All zeroes on an attachment written
    /// before the uploader existed, which is the same thing as "not on the
    /// relay".
    pub ciphertext_hash: String,
    /// The original's width in pixels, when the source device knew it.
    pub width: Option<u32>,
    /// The original's height in pixels, when the source device knew it.
    pub height: Option<u32>,
    /// The MIME type of the attachment's usable thumbnail, `image/jpeg` or
    /// `image/png`, or `None` when it has none. Its bytes come from
    /// [`crate::SunriseCore::thumbnail_bytes`]; its key never crosses the
    /// seam.
    pub thumbnail_mime: Option<String>,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Attachment> for AttachmentItem {
    fn from(a: &Attachment) -> Self {
        let Attachment {
            id,
            created_at,
            updated_at,
            parent,
            filename,
            mime_type,
            size_bytes,
            blob_key,
            blob_id,
            chunk_count,
            content_hash,
            ciphertext_hash,
            width,
            height,
            // The thumbnail crosses as its MIME type, and only when
            // `Attachment::thumbnail` finds it usable; the client reads its
            // bytes through the core, so the key stays on this side.
            thumbnail_blob_id: _,
            thumbnail_blob_key: _,
            thumbnail_mime: _,
            thumbnail_size_bytes: _,
            thumbnail_content_hash: _,
            thumbnail_ciphertext_hash: _,
            deleted,
            // Deliberately not exported: see the module docs.
            unknown: _,
        } = a;
        Self {
            width: *width,
            height: *height,
            thumbnail_mime: a.thumbnail().map(|t| t.mime_type),
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            parent: *parent,
            filename: filename.clone(),
            mime_type: mime_type.clone(),
            size_bytes: *size_bytes,
            blob_key: blob_key.to_vec(),
            blob_id: hex16(blob_id),
            chunk_count: *chunk_count,
            content_hash: sunrise_client_core::ids::hex(content_hash),
            ciphertext_hash: sunrise_client_core::ids::hex(ciphertext_hash),
            deleted: *deleted,
        }
    }
}

impl AttachmentItem {
    /// Back to the domain record this row was projected from.
    ///
    /// UniFFI has no fixed-size array, so the key crosses as `Vec<u8>` and the
    /// two digests as hex; widening them again is *checked* rather than
    /// assumed. The row is usually one the seam handed out moments ago, but
    /// nothing stops a caller assembling one, and a silently truncated blob key
    /// would surface much later as "this attachment will not open".
    ///
    /// `unknown` is empty: forward-compat fields are preserved in storage and
    /// deliberately never exported (see the module docs), so a round trip
    /// through the seam is not how a record gets written back. The thumbnail
    /// fields are empty for the same reason: only its MIME type crosses.
    ///
    /// # Errors
    ///
    /// [`crate::BindingError::BadFixedBytes`], naming the field.
    pub(crate) fn to_domain(&self) -> Result<Attachment, crate::BindingError> {
        Ok(Attachment {
            id: self.id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            parent: self.parent,
            filename: self.filename.clone(),
            mime_type: self.mime_type.clone(),
            size_bytes: self.size_bytes,
            blob_key: fixed_bytes(&self.blob_key, "blob_key")?,
            blob_id: from_hex(&self.blob_id, "blob_id")?,
            chunk_count: self.chunk_count,
            content_hash: from_hex(&self.content_hash, "content_hash")?,
            ciphertext_hash: from_hex(&self.ciphertext_hash, "ciphertext_hash")?,
            width: self.width,
            height: self.height,
            thumbnail_blob_id: None,
            thumbnail_blob_key: None,
            thumbnail_mime: None,
            thumbnail_size_bytes: None,
            thumbnail_content_hash: None,
            thumbnail_ciphertext_hash: None,
            deleted: self.deleted,
            unknown: sunrise_domain::Unknowns::new(),
        })
    }
}

/// See [`sunrise_core::AttachPreview`]: what the platform could tell about a
/// file being attached. Every field `None` when it could tell nothing.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct AttachPreviewIn {
    /// The original's width in pixels.
    pub width: Option<u32>,
    /// The original's height in pixels.
    pub height: Option<u32>,
    /// `image/jpeg` or `image/png`, with `thumbnail_bytes`.
    pub thumbnail_mime: Option<String>,
    /// The encoded thumbnail, at most 256 KiB, with `thumbnail_mime`.
    pub thumbnail_bytes: Option<Vec<u8>>,
}

impl From<AttachPreviewIn> for sunrise_core::AttachPreview {
    fn from(p: AttachPreviewIn) -> Self {
        Self {
            width: p.width,
            height: p.height,
            thumbnail: match (p.thumbnail_mime, p.thumbnail_bytes) {
                (Some(mime_type), Some(bytes)) => {
                    Some(sunrise_core::ThumbnailImage { mime_type, bytes })
                }
                _ => None,
            },
        }
    }
}

/// An attachment's thumbnail, decrypted and verified.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ThumbnailOut {
    /// `image/jpeg` or `image/png`.
    pub mime_type: String,
    /// The encoded image.
    pub bytes: Vec<u8>,
}

/// See [`sunrise_core::CacheUsage`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct CacheUsageItem {
    /// Sealed bytes the cache holds.
    pub used_bytes: u64,
    /// What Clear cache would free.
    pub evictable_bytes: u64,
    /// The limit.
    pub limit_bytes: u64,
}

impl From<sunrise_core::CacheUsage> for CacheUsageItem {
    fn from(u: sunrise_core::CacheUsage) -> Self {
        Self {
            used_bytes: u.used_bytes,
            evictable_bytes: u.evictable_bytes,
            limit_bytes: u.limit_bytes,
        }
    }
}

/// See [`sunrise_core::RebuildReport`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct RebuildReportItem {
    /// Entity ops folded again from the log.
    pub ops_replayed: u64,
    /// Entity ops this build would not apply, left out and logged.
    pub ops_skipped: u64,
    /// Entities whose compacted state was carried across from the merge
    /// state.
    pub compacted_entities: u64,
}

impl From<sunrise_core::RebuildReport> for RebuildReportItem {
    fn from(r: sunrise_core::RebuildReport) -> Self {
        Self {
            ops_replayed: r.ops_replayed,
            ops_skipped: r.ops_skipped,
            compacted_entities: r.compacted_entities,
        }
    }
}

/// See [`sunrise_domain::AttachmentDraft`]. Every field is a fact about bytes
/// the client already sealed and uploaded, so the client supplies it; the core
/// fills only the id and the timestamps.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AttachmentDraftIn {
    /// Parent task.
    pub parent: EntityRef,
    /// Filename.
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// Plaintext size in bytes.
    pub size_bytes: u64,
    /// Per-blob symmetric key; must be exactly 32 bytes.
    pub blob_key: Vec<u8>,
    /// Blob id as 32 lowercase hex characters.
    pub blob_id: String,
    /// Chunk count.
    pub chunk_count: u32,
    /// BLAKE3 of the concatenated plaintext, 64 lowercase hex characters.
    pub content_hash: String,
    /// BLAKE3 of the concatenated ciphertext, 64 lowercase hex characters.
    /// See [`sunrise_domain::Attachment::ciphertext_hash`].
    pub ciphertext_hash: String,
}

impl TryFrom<AttachmentDraftIn> for sunrise_domain::AttachmentDraft {
    type Error = crate::BindingError;

    fn try_from(d: AttachmentDraftIn) -> Result<Self, Self::Error> {
        Ok(Self {
            parent: d.parent,
            filename: d.filename,
            mime_type: d.mime_type,
            size_bytes: d.size_bytes,
            blob_key: fixed_bytes(&d.blob_key, "blob_key")?,
            blob_id: from_hex(&d.blob_id, "blob_id")?,
            chunk_count: d.chunk_count,
            content_hash: from_hex(&d.content_hash, "content_hash")?,
            ciphertext_hash: from_hex(&d.ciphertext_hash, "ciphertext_hash")?,
            // A client that seals its own blobs through `AttachFile` has no
            // thumbnail to describe; `SunriseCore::attach_file` is the path
            // that makes one.
            width: None,
            height: None,
            thumbnail: None,
        })
    }
}

fn bad_width<const N: usize>(field: &str) -> crate::BindingError {
    crate::BindingError::BadFixedBytes {
        field: field.to_string(),
        expected: u32::try_from(N).unwrap_or(u32::MAX),
    }
}

fn fixed_bytes<const N: usize>(raw: &[u8], field: &str) -> Result<[u8; N], crate::BindingError> {
    <[u8; N]>::try_from(raw).map_err(|_| bad_width::<N>(field))
}

fn from_hex<const N: usize>(s: &str, field: &str) -> Result<[u8; N], crate::BindingError> {
    let mut out = [0u8; N];
    if s.len() != N * 2 {
        return Err(bad_width::<N>(field));
    }
    for (i, byte) in out.iter_mut().enumerate() {
        // `get`, not indexing: the field comes from outside Rust, and a pair
        // that starts or ends inside a multi-byte character is not hex.
        let pair = s
            .get(i * 2..i * 2 + 2)
            .ok_or_else(|| bad_width::<N>(field))?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| bad_width::<N>(field))?;
    }
    Ok(out)
}

#[cfg(test)]
mod hex_tests;
