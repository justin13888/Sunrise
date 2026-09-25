//! Attachment metadata per `docs/02-domain/attachments.md`.
//!
//! The blob bytes themselves live in the Stream's blob store (chunked &
//! encrypted; see `docs/03-crypto/data-encryption-format.md` §blob-chunks).
//! This entity carries only the per-attachment metadata.
//!
//! The metadata is written **once** and thereafter only tombstoned. Every
//! field but `deleted` describes a specific run of ciphertext identified by
//! `content_hash`; changing one would be describing different bytes, so there
//! is no `AttachmentPatch` and no update op. Re-attaching an edited file is a
//! new attachment, which is also what content addressing already implies.

use crate::unknown::Unknowns;
use crate::validation::{
    validate_title, ValidationError, MAX_ATTACHMENT_BYTES, MAX_FILENAME_LEN, MAX_MIME_TYPE_LEN,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::{EntityKind, EntityRef};

/// Persisted Attachment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
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
    /// BLAKE3 of the concatenated **ciphertext** (32 bytes): the name the relay
    /// stores this blob under.
    ///
    /// `POST /blobs/finalize` content-addresses a committed blob at the first
    /// sixteen bytes of this hash, and `GET /blobs/{blob_id}` answers to that
    /// and nothing else. A replica that holds this op and not the bytes cannot
    /// derive it — it would have to hash the ciphertext it is asking for — so
    /// the device that sealed the chunks records it here, where it costs
    /// nothing: the sealer is holding every sealed chunk at that moment.
    ///
    /// Without it, a second device has the key, the id, the chunk count and the
    /// content hash, and still no way to *name* the blob it wants. That is the
    /// shape #176 describes: metadata everywhere, bytes nowhere.
    ///
    /// All zeroes on an attachment written before this field existed, and
    /// [`Attachment::is_fetchable`] is the check that distinguishes the two.
    /// Those attachments were never uploaded either, so nothing is lost by
    /// being unable to name them.
    #[serde(default, with = "serde_bytes")]
    pub ciphertext_hash: [u8; 32],
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl Attachment {
    /// Whether this replica can name this attachment's blob on the relay.
    ///
    /// False for an op written before [`Attachment::ciphertext_hash`] existed,
    /// which carries all zeroes. Those blobs were never uploaded, so the honest
    /// answer to "fetch it" is that there is nothing to fetch — not a `GET`
    /// against `blb_00000000000000000000000000000000`, which would either 404
    /// or, far worse, name whatever some other upload happened to hash to.
    #[must_use]
    pub fn is_fetchable(&self) -> bool {
        self.ciphertext_hash != [0u8; 32]
    }

    /// The relay's id for this blob: `blb_` + the first sixteen bytes of
    /// [`Attachment::ciphertext_hash`] in lowercase hex.
    ///
    /// `None` when [`Attachment::is_fetchable`] is false. The derivation is
    /// `finalize`'s, reproduced here because the fetching device never sees a
    /// `finalize` response — the uploading device did, on another machine,
    /// possibly months ago.
    #[must_use]
    pub fn relay_blob_id(&self) -> Option<[u8; 16]> {
        if !self.is_fetchable() {
            return None;
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.ciphertext_hash[..16]);
        Some(out)
    }
}

/// Draft for `Command::AttachFile`. The core fills the id and the timestamps;
/// everything else is a fact about bytes the client already encrypted, so the
/// client supplies it.
///
/// `blob_key` in particular is generated **client-side, before upload** — the
/// file has to be sealed under it to be uploadable at all, so the core cannot
/// mint it after the fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentDraft {
    /// Parent entity. Tasks only, currently.
    pub parent: EntityRef,
    /// Filename, informational.
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// Plaintext size in bytes.
    pub size_bytes: u64,
    /// Per-blob symmetric key.
    pub blob_key: [u8; 32],
    /// Blob id assigned by the creating device.
    pub blob_id: [u8; 16],
    /// Chunk count.
    pub chunk_count: u32,
    /// BLAKE3 of the concatenated plaintext.
    pub content_hash: [u8; 32],
    /// BLAKE3 of the concatenated ciphertext. See
    /// [`Attachment::ciphertext_hash`] for what names it.
    pub ciphertext_hash: [u8; 32],
}

impl AttachmentDraft {
    /// Validate the draft against `docs/02-domain/attachments.md`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.parent.kind() != EntityKind::Task {
            return Err(ValidationError::Field {
                field: "attachment.parent",
                constraint: "task_only_in_v1",
            });
        }
        let _ = validate_title(&self.filename, "attachment.filename", MAX_FILENAME_LEN)?;
        let _ = validate_title(&self.mime_type, "attachment.mime_type", MAX_MIME_TYPE_LEN)?;
        if self.chunk_count == 0 {
            return Err(ValidationError::Field {
                field: "attachment.chunk_count",
                constraint: "at_least_one",
            });
        }
        // The size policy is a domain rule, not a server one: a client that
        // cannot upload the blob must not first write an op describing it.
        if self.size_bytes == 0 || self.size_bytes > MAX_ATTACHMENT_BYTES {
            return Err(ValidationError::Field {
                field: "attachment.size_bytes",
                constraint: "size_policy",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> AttachmentDraft {
        AttachmentDraft {
            parent: EntityRef::new(EntityKind::Task, [3u8; 16]),
            filename: "receipt.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 4096,
            blob_key: [7u8; 32],
            blob_id: [9u8; 16],
            chunk_count: 1,
            content_hash: [11u8; 32],
            ciphertext_hash: [13u8; 32],
        }
    }

    fn attachment(ciphertext_hash: [u8; 32]) -> Attachment {
        Attachment {
            id: EntityRef::new(EntityKind::Attachment, [1u8; 16]),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            parent: EntityRef::new(EntityKind::Task, [3u8; 16]),
            filename: "receipt.pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 4096,
            blob_key: [7u8; 32],
            blob_id: [9u8; 16],
            chunk_count: 1,
            content_hash: [11u8; 32],
            ciphertext_hash,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    #[test]
    fn a_well_formed_draft_validates() {
        draft().validate().unwrap();
    }

    /// The relay's derivation, reproduced by the device that has to fetch.
    #[test]
    fn the_relay_blob_id_is_the_first_half_of_the_ciphertext_hash() {
        let mut hash = [0u8; 32];
        for (i, b) in hash.iter_mut().enumerate() {
            *b = u8::try_from(i).expect("a 32-entry index fits a byte");
        }
        let att = attachment(hash);
        assert!(att.is_fetchable());
        assert_eq!(
            att.relay_blob_id(),
            Some([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15])
        );
    }

    /// An attachment from before the field existed decodes with all zeroes, and
    /// must not be mistaken for one that hashes to zero: nothing was uploaded,
    /// so there is nothing on the relay to name.
    #[test]
    fn an_attachment_with_no_ciphertext_hash_cannot_be_named_on_the_relay() {
        let att = attachment([0u8; 32]);
        assert!(!att.is_fetchable());
        assert_eq!(att.relay_blob_id(), None);
    }

    #[test]
    fn only_tasks_can_carry_an_attachment_in_v1() {
        let d = AttachmentDraft {
            parent: EntityRef::new(EntityKind::Stream, [3u8; 16]),
            ..draft()
        };
        assert_eq!(
            d.validate(),
            Err(ValidationError::Field {
                field: "attachment.parent",
                constraint: "task_only_in_v1"
            })
        );
    }

    #[test]
    fn an_empty_filename_or_mime_type_is_rejected() {
        for d in [
            AttachmentDraft {
                filename: "   ".into(),
                ..draft()
            },
            AttachmentDraft {
                mime_type: String::new(),
                ..draft()
            },
        ] {
            assert_eq!(d.validate(), Err(ValidationError::InvalidTitle));
        }
    }

    #[test]
    fn a_chunkless_attachment_is_rejected() {
        let d = AttachmentDraft {
            chunk_count: 0,
            ..draft()
        };
        assert!(d.validate().is_err());
    }

    #[test]
    fn the_size_policy_is_enforced_at_both_ends() {
        for size in [0, MAX_ATTACHMENT_BYTES + 1] {
            let d = AttachmentDraft {
                size_bytes: size,
                ..draft()
            };
            assert_eq!(
                d.validate(),
                Err(ValidationError::Field {
                    field: "attachment.size_bytes",
                    constraint: "size_policy"
                })
            );
        }
        let ok = AttachmentDraft {
            size_bytes: MAX_ATTACHMENT_BYTES,
            ..draft()
        };
        ok.validate().unwrap();
    }
}
