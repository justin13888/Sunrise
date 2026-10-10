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
    /// Width of the original in pixels, when the source device knew it.
    ///
    /// This field and the seven after it are the `attachment.thumbnail`
    /// feature (ADR-0053 §2). Every one is optional, written with the
    /// attachment and never changed, and absent from the wire when unset, so
    /// an attachment without a thumbnail encodes exactly as it did before they
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height of the original in pixels, when the source device knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// The thumbnail blob's id: the AAD its one chunk is sealed with. Fresh,
    /// never the original's.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub thumbnail_blob_id: Option<[u8; 16]>,
    /// The thumbnail blob's key. Fresh, never the original's: the chunk nonce
    /// is derived from the key and the chunk index alone, so one key over two
    /// plaintexts is a nonce reuse.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub thumbnail_blob_key: Option<[u8; 32]>,
    /// `image/jpeg` or `image/png`. A receiver ignores any other value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_mime: Option<String>,
    /// The thumbnail's plaintext size, at most [`MAX_THUMBNAIL_BYTES`]: one
    /// chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_size_bytes: Option<u32>,
    /// BLAKE3 of the thumbnail plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub thumbnail_content_hash: Option<[u8; 32]>,
    /// BLAKE3 of the thumbnail's sealed chunk: the name the relay stores it
    /// under, for the reason [`Attachment::ciphertext_hash`] exists. Without
    /// it a receiver holds the thumbnail's key and cannot ask for its bytes.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "serde_bytes")]
    pub thumbnail_ciphertext_hash: Option<[u8; 32]>,
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

    /// This attachment's thumbnail, when it has one a receiver may use.
    ///
    /// `None` unless all six `thumbnail_*` fields are present, the MIME type
    /// is JPEG or PNG, the size is one chunk at most, and the blob has a name
    /// on the relay. ADR-0053 §4: a thumbnail of any other shape is ignored
    /// rather than rendered, because the receiver is the one that must decode
    /// it.
    #[must_use]
    pub fn thumbnail(&self) -> Option<Thumbnail> {
        let thumb = Thumbnail {
            blob_id: self.thumbnail_blob_id?,
            blob_key: self.thumbnail_blob_key?,
            mime_type: self.thumbnail_mime.clone()?,
            size_bytes: self.thumbnail_size_bytes?,
            content_hash: self.thumbnail_content_hash?,
            ciphertext_hash: self.thumbnail_ciphertext_hash?,
        };
        thumb.validate().is_ok().then_some(thumb)
    }

    /// Set the eight `attachment.thumbnail` fields from a draft's values.
    pub fn set_preview(
        &mut self,
        width: Option<u32>,
        height: Option<u32>,
        thumb: Option<&Thumbnail>,
    ) {
        self.width = width;
        self.height = height;
        self.thumbnail_blob_id = thumb.map(|t| t.blob_id);
        self.thumbnail_blob_key = thumb.map(|t| t.blob_key);
        self.thumbnail_mime = thumb.map(|t| t.mime_type.clone());
        self.thumbnail_size_bytes = thumb.map(|t| t.size_bytes);
        self.thumbnail_content_hash = thumb.map(|t| t.content_hash);
        self.thumbnail_ciphertext_hash = thumb.map(|t| t.ciphertext_hash);
    }

    /// Whether this attachment carries any `attachment.thumbnail` field, valid
    /// or not. What decides whether writing it needs the feature.
    #[must_use]
    pub fn uses_thumbnail_feature(&self) -> bool {
        self.width.is_some()
            || self.height.is_some()
            || self.thumbnail_blob_id.is_some()
            || self.thumbnail_blob_key.is_some()
            || self.thumbnail_mime.is_some()
            || self.thumbnail_size_bytes.is_some()
            || self.thumbnail_content_hash.is_some()
            || self.thumbnail_ciphertext_hash.is_some()
    }
}

/// The largest thumbnail, in plaintext bytes: one 256 KiB blob chunk
/// (ADR-0053 §1).
pub const MAX_THUMBNAIL_BYTES: u32 = 256 * 1024;

/// The only MIME types a thumbnail may have (ADR-0053 §1): every platform
/// decodes both natively.
pub const THUMBNAIL_MIME_TYPES: [&str; 2] = ["image/jpeg", "image/png"];

/// A thumbnail blob: one sealed chunk under its own key, described by the six
/// `thumbnail_*` fields of an [`Attachment`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thumbnail {
    /// Blob id, the chunk's AAD. Fresh, never the original's.
    pub blob_id: [u8; 16],
    /// Blob key. Fresh, never the original's.
    pub blob_key: [u8; 32],
    /// `image/jpeg` or `image/png`.
    pub mime_type: String,
    /// Plaintext size, `1..=MAX_THUMBNAIL_BYTES`.
    pub size_bytes: u32,
    /// BLAKE3 of the plaintext.
    pub content_hash: [u8; 32],
    /// BLAKE3 of the sealed chunk, the relay's name for it.
    pub ciphertext_hash: [u8; 32],
}

impl Thumbnail {
    /// The relay's id for this blob, as [`Attachment::relay_blob_id`] derives
    /// the original's.
    #[must_use]
    pub fn relay_blob_id(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(&self.ciphertext_hash[..16]);
        out
    }

    /// Check the shape ADR-0053 §1 allows: a JPEG or PNG of one chunk at most,
    /// with a name on the relay.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if !THUMBNAIL_MIME_TYPES.contains(&self.mime_type.as_str()) {
            return Err(ValidationError::Field {
                field: "attachment.thumbnail_mime",
                constraint: "jpeg_or_png",
            });
        }
        if self.size_bytes == 0 || self.size_bytes > MAX_THUMBNAIL_BYTES {
            return Err(ValidationError::Field {
                field: "attachment.thumbnail_size_bytes",
                constraint: "one_chunk",
            });
        }
        if self.ciphertext_hash == [0u8; 32] {
            return Err(ValidationError::Field {
                field: "attachment.thumbnail_ciphertext_hash",
                constraint: "present",
            });
        }
        Ok(())
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
    /// Width of the original in pixels, when the client knows it.
    #[serde(default)]
    pub width: Option<u32>,
    /// Height of the original in pixels, when the client knows it.
    #[serde(default)]
    pub height: Option<u32>,
    /// The thumbnail the source device made and sealed, if any (ADR-0053 §1).
    #[serde(default)]
    pub thumbnail: Option<Thumbnail>,
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
        if let Some(thumb) = &self.thumbnail {
            thumb.validate()?;
            // ADR-0053 §2: the nonce is derived from the key and the chunk
            // index, so a thumbnail under the original's key is a nonce reuse.
            if thumb.blob_key == self.blob_key || thumb.blob_id == self.blob_id {
                return Err(ValidationError::Field {
                    field: "attachment.thumbnail_blob_key",
                    constraint: "fresh_key",
                });
            }
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
            width: None,
            height: None,
            thumbnail: None,
        }
    }

    fn thumb() -> Thumbnail {
        Thumbnail {
            blob_id: [21u8; 16],
            blob_key: [23u8; 32],
            mime_type: "image/jpeg".into(),
            size_bytes: 2048,
            content_hash: [25u8; 32],
            ciphertext_hash: [27u8; 32],
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
            width: None,
            height: None,
            thumbnail_blob_id: None,
            thumbnail_blob_key: None,
            thumbnail_mime: None,
            thumbnail_size_bytes: None,
            thumbnail_content_hash: None,
            thumbnail_ciphertext_hash: None,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn encode<T: Serialize>(v: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(v, &mut buf).unwrap();
        buf
    }

    fn keys(bytes: &[u8]) -> Vec<String> {
        let ciborium::Value::Map(entries) = ciborium::de::from_reader(bytes).unwrap() else {
            panic!("an attachment encodes as a map");
        };
        entries
            .into_iter()
            .map(|(k, _)| k.into_text().unwrap())
            .collect()
    }

    /// An attachment with no thumbnail writes none of the new keys, so its
    /// bytes are the ones a build from before the feature writes.
    #[test]
    fn an_attachment_without_a_thumbnail_writes_none_of_its_fields() {
        let written = keys(&encode(&attachment([13u8; 32])));
        assert!(written
            .iter()
            .all(|k| !k.starts_with("thumbnail_") && k != "width" && k != "height"));
    }

    /// The thumbnail fields round-trip, the hashes and ids as byte strings.
    #[test]
    fn the_thumbnail_fields_round_trip_as_byte_strings() {
        let mut att = attachment([13u8; 32]);
        att.set_preview(Some(4032), Some(3024), Some(&thumb()));
        let bytes = encode(&att);
        let ciborium::Value::Map(entries) = ciborium::de::from_reader(bytes.as_slice()).unwrap()
        else {
            panic!("map");
        };
        for (k, v) in &entries {
            if matches!(
                k.as_text(),
                Some("thumbnail_blob_id" | "thumbnail_blob_key" | "thumbnail_content_hash")
            ) {
                assert!(v.is_bytes(), "{k:?} must be a byte string");
            }
        }
        let back: Attachment = ciborium::de::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(back, att);
        assert!(back.unknown.is_empty());
        assert_eq!(back.thumbnail(), Some(thumb()));
        assert!(back.uses_thumbnail_feature());
    }

    /// A build without the fields keeps them as unknowns and writes them back
    /// unchanged (ADR-0045, #322): decoding into a map and re-encoding is
    /// exactly what such a build's `Unknowns` does.
    #[test]
    fn a_build_without_the_fields_round_trips_them_unchanged() {
        let mut att = attachment([13u8; 32]);
        att.set_preview(Some(10), Some(20), Some(&thumb()));
        let bytes = encode(&att);
        let as_map: std::collections::BTreeMap<String, ciborium::Value> =
            ciborium::de::from_reader(bytes.as_slice()).unwrap();
        let again: Attachment = ciborium::de::from_reader(encode(&as_map).as_slice()).unwrap();
        assert_eq!(again, att);
    }

    /// ADR-0053 §4: a thumbnail a receiver cannot decode natively is ignored.
    #[test]
    fn a_thumbnail_that_is_not_jpeg_or_png_or_one_chunk_is_ignored() {
        for bad in [
            Thumbnail {
                mime_type: "image/avif".into(),
                ..thumb()
            },
            Thumbnail {
                size_bytes: MAX_THUMBNAIL_BYTES + 1,
                ..thumb()
            },
            Thumbnail {
                ciphertext_hash: [0u8; 32],
                ..thumb()
            },
        ] {
            let mut att = attachment([13u8; 32]);
            att.set_preview(None, None, Some(&bad));
            assert_eq!(att.thumbnail(), None);
        }
        // Five of six fields is not a thumbnail either.
        let mut att = attachment([13u8; 32]);
        att.set_preview(None, None, Some(&thumb()));
        att.thumbnail_content_hash = None;
        assert_eq!(att.thumbnail(), None);
    }

    #[test]
    fn a_draft_thumbnail_must_have_its_own_key_and_a_native_format() {
        let ok = AttachmentDraft {
            thumbnail: Some(thumb()),
            ..draft()
        };
        ok.validate().unwrap();
        let reused = AttachmentDraft {
            thumbnail: Some(Thumbnail {
                blob_key: [7u8; 32],
                ..thumb()
            }),
            ..draft()
        };
        assert_eq!(
            reused.validate(),
            Err(ValidationError::Field {
                field: "attachment.thumbnail_blob_key",
                constraint: "fresh_key"
            })
        );
        let webp = AttachmentDraft {
            thumbnail: Some(Thumbnail {
                mime_type: "image/webp".into(),
                ..thumb()
            }),
            ..draft()
        };
        assert!(webp.validate().is_err());
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
