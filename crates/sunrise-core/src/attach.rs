//! The attachment byte path: seal, store, reassemble, verify.
//!
//! `Command::AttachFile` records *metadata about bytes that already exist*.
//! Until this module, nothing in the workspace put those bytes anywhere: the
//! metadata command was reachable, the relay's chunk routes were mounted, and
//! the two had no client between them. An attachment could be described and
//! never read back.
//!
//! # Why this is on `Core` and not on a client
//!
//! Every step is a decision the core already owns. The per-blob key comes from
//! the injected [`crate::config::Rng`], so a test gets a deterministic one and
//! the `clippy.toml` determinism gate stays satisfied. The chunking, sealing
//! and content hash are the frozen v1 format in
//! `docs/03-crypto/data-encryption-format.md` §Blob chunks. A client that did
//! any of it would be a second implementation of a wire format, and a wrong one
//! would produce attachments only that client could open.
//!
//! # What this does not do
//!
//! Chunks land in **this vault's** blob store, and this module stops there.
//! Getting them to the relay so a paired device can fetch them —
//! `POST /blobs/init` → `PUT` → `finalize` — is `crate::blob_sync`, driven by
//! [`crate::sync_driver`]; all `attach_file` does about it is write the queue
//! row. That split is the point: attaching a file is a synchronous local call
//! the user is waiting on, and it has to finish on a device with no network.
//!
//! So an attachment is readable on the device that made it from the moment
//! `attach_file` returns, and readable elsewhere once the upload and the fetch
//! have both run. In between, and for an attachment whose bytes this device has
//! chosen not to fetch, [`Core::attachment_bytes`] reports
//! [`AttachError::BytesNotHere`] rather than pretending, so a client can say so.
//!
//! "Chosen not to fetch" is a decision with a way out of it, which is the part
//! issue #227 reported missing. [`Core::fetch_attachment`], in this crate's
//! private `blob_fetch` module, asks for one named attachment whatever its
//! size, and is what a client's "Download" button calls.

use crate::blob_cache::BlobRef;
use crate::commands::Command;
use crate::core::{Core, CoreError};
use crate::queries::{Query, QueryResult};
use sunrise_crypto::blob_chunk::{
    chunk_count_for, content_hash, open_chunk, seal_chunk, verify_content, BlobChunkError,
    CiphertextHasher, CHUNK_PLAINTEXT_LEN,
};
use sunrise_domain::validation::MAX_ATTACHMENT_BYTES;
use sunrise_domain::{Attachment, AttachmentDraft, Thumbnail, MAX_THUMBNAIL_BYTES};
use sunrise_id::EntityRef;
use sunrise_storage::{BlobStore, BlobStoreError};
use thiserror::Error;

/// Errors from the attachment byte path.
#[derive(Debug, Error)]
pub enum AttachError {
    /// An attachment must have bytes; a zero-length file is refused rather
    /// than recorded as an attachment with nothing to fetch.
    #[error("an attachment cannot be empty")]
    Empty,
    /// Over `docs/02-domain/attachments.md`'s per-attachment ceiling.
    #[error("attachment is {size} bytes, over the {MAX_ATTACHMENT_BYTES}-byte limit")]
    TooLarge {
        /// The offending size.
        size: u64,
    },
    /// The id did not name a live attachment in this vault.
    #[error("no such attachment: {0}")]
    NotFound(EntityRef),
    /// The metadata is here and the chunks are not — the usual case for an
    /// attachment created on another device, whose bytes were never fetched.
    #[error("attachment {id} has no bytes on this device")]
    BytesNotHere {
        /// The attachment.
        id: EntityRef,
    },
    /// The bytes cannot be fetched because no device ever uploaded them: the
    /// attachment predates [`sunrise_domain::Attachment::ciphertext_hash`], so
    /// this replica cannot even *name* the blob on the relay.
    ///
    /// Terminal. Distinct from [`AttachError::BytesNotHere`] on purpose: that
    /// one is an invitation to press Download, and this one is the answer that
    /// pressing it will never work.
    #[error("attachment {id} was never uploaded, so there is nothing to fetch")]
    NotOnRelay {
        /// The attachment.
        id: EntityRef,
    },
    /// [`Core::fetch_attachment`] was called on a vault with no sync driver
    /// running, so nothing could ever service the request.
    ///
    /// Not a hang and not a silent failure: an offline vault is a legitimate
    /// state — the CLI opens one for every subcommand but `sync` — and a
    /// Download button there has to say so rather than spin.
    #[error("attachment {id} cannot be fetched: this vault has no sync session")]
    SyncOffline {
        /// The attachment.
        id: EntityRef,
    },
    /// A client called [`Core::cancel_attachment_fetch`] while the transfer was
    /// running. The request is marked `partial`; asking again restarts it.
    #[error("the download of attachment {id} was cancelled")]
    FetchCancelled {
        /// The attachment.
        id: EntityRef,
    },
    /// The relay could not serve the blob within the attempt ceiling. The
    /// request is marked `partial`; asking again restarts it.
    #[error("attachment {id} could not be fetched from the relay")]
    FetchUnavailable {
        /// The attachment.
        id: EntityRef,
    },
    /// The thumbnail a client supplied is not one ADR-0053 §1 allows: not a
    /// JPEG or PNG, bytes that are not the format named, or over one chunk.
    /// Refused before anything is written; the client attaches again without
    /// it.
    #[error("the thumbnail is not a JPEG or PNG of at most one chunk: {0}")]
    BadThumbnail(&'static str),
    /// Chunk crypto failed: a wrong key, a chunk from another blob, or an
    /// altered one.
    #[error(transparent)]
    Chunk(#[from] BlobChunkError),
    /// The blob store could not be read or written.
    #[error(transparent)]
    Store(#[from] BlobStoreError),
    /// The command or query around the bytes failed.
    #[error(transparent)]
    Core(#[from] Box<CoreError>),
}

impl From<CoreError> for AttachError {
    fn from(e: CoreError) -> Self {
        Self::Core(Box::new(e))
    }
}

/// A thumbnail image a client rendered from the original (ADR-0053 §1).
///
/// The client renders it, because only the platform can decode the original;
/// the core seals it, because the blob format is the core's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailImage {
    /// `image/jpeg` or `image/png`.
    pub mime_type: String,
    /// The encoded image, at most one chunk.
    pub bytes: Vec<u8>,
}

impl ThumbnailImage {
    /// Refuse a thumbnail of a type a receiver may not be able to decode, of
    /// bytes that are not the type they claim, or over one chunk.
    fn check(&self) -> Result<(), AttachError> {
        const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
        const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let magic = match self.mime_type.as_str() {
            "image/jpeg" => JPEG,
            "image/png" => PNG,
            _ => return Err(AttachError::BadThumbnail("not image/jpeg or image/png")),
        };
        if self.bytes.is_empty() || self.bytes.len() > MAX_THUMBNAIL_BYTES as usize {
            return Err(AttachError::BadThumbnail("empty or over one chunk"));
        }
        if !self.bytes.starts_with(magic) {
            return Err(AttachError::BadThumbnail("bytes do not match the type"));
        }
        Ok(())
    }
}

/// What a client may record about an attachment beyond its bytes: the
/// original's dimensions and a thumbnail (ADR-0053 §1–§2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachPreview {
    /// The original's width in pixels, when known.
    pub width: Option<u32>,
    /// The original's height in pixels, when known.
    pub height: Option<u32>,
    /// The thumbnail, when the platform could render one.
    pub thumbnail: Option<ThumbnailImage>,
}

impl Core {
    /// Seal `bytes`, store them in this vault's blob store, and record the
    /// attachment against `parent`, with no thumbnail or dimensions.
    ///
    /// # Errors
    ///
    /// As [`Core::attach_file_with`].
    pub async fn attach_file(
        &self,
        parent: EntityRef,
        filename: String,
        mime_type: String,
        bytes: &[u8],
    ) -> Result<Attachment, AttachError> {
        self.attach_file_with(parent, filename, mime_type, bytes, AttachPreview::default())
            .await
    }

    /// Seal `bytes`, store them in this vault's blob store, and record the
    /// attachment against `parent`.
    ///
    /// The key is minted here and travels only inside the attachment op, which
    /// is itself sealed under the parent Stream's key — so the bytes are end-
    /// to-end protected and the relay, which will one day hold the chunks,
    /// cannot open them.
    ///
    /// The chunks are written **before** the metadata op. The other order would
    /// publish an attachment whose bytes are not yet locally readable, and a
    /// reader racing the write would see [`AttachError::BytesNotHere`] for
    /// something that is in fact here.
    ///
    /// A thumbnail is sealed as a second blob under its own fresh key and id
    /// (ADR-0053 §2) and queued for upload beside the original. When the
    /// vault cannot take the `attachment.thumbnail` feature yet, because a
    /// paired device does not support it, the attachment is recorded without
    /// the thumbnail and the dimensions, and the thumbnail's chunks are
    /// discarded.
    ///
    /// # Errors
    ///
    /// [`AttachError::Empty`], [`AttachError::TooLarge`] or
    /// [`AttachError::BadThumbnail`] before anything is written; a store or
    /// command error after.
    pub async fn attach_file_with(
        &self,
        parent: EntityRef,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        preview: AttachPreview,
    ) -> Result<Attachment, AttachError> {
        let size_bytes = bytes.len() as u64;
        if bytes.is_empty() {
            return Err(AttachError::Empty);
        }
        if size_bytes > MAX_ATTACHMENT_BYTES {
            return Err(AttachError::TooLarge { size: size_bytes });
        }
        if let Some(t) = &preview.thumbnail {
            t.check()?;
        }

        let mut blob_key = [0u8; 32];
        self.rng().fill_bytes(&mut blob_key);
        let mut blob_id = [0u8; 16];
        self.rng().fill_bytes(&mut blob_id);

        let chunk_count = chunk_count_for(size_bytes);
        let store = BlobStore::new(self.vault_dir())?;
        // The ciphertext hash accumulates here and nowhere else. It is the only
        // name the relay knows this blob by, and this loop is the one moment
        // every sealed chunk exists: recomputing it later would mean reading
        // the whole attachment back off disk to learn something that was free
        // while it was being written.
        let mut ciphertext = CiphertextHasher::new();
        for (idx, piece) in bytes.chunks(CHUNK_PLAINTEXT_LEN).enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            let sealed = seal_chunk(&blob_key, &blob_id, idx, chunk_count, piece)?;
            ciphertext.update(&sealed);
            store.put_chunk(&blob_id, idx, &sealed)?;
        }
        let ciphertext_hash = ciphertext.finish();
        let thumbnail = match &preview.thumbnail {
            Some(image) => Some(self.seal_thumbnail(&store, image, &blob_key, &blob_id)?),
            None => None,
        };

        let draft = AttachmentDraft {
            parent,
            filename,
            mime_type,
            size_bytes,
            blob_key,
            blob_id,
            chunk_count,
            content_hash: content_hash(bytes),
            ciphertext_hash,
            width: preview.width,
            height: preview.height,
            thumbnail: thumbnail.clone(),
        };
        let result = self.submit(Command::AttachFile(draft)).await?;
        let att = self.attachment_row(result.entity).await?;
        // The op is durable before the upload is queued, and that order is the
        // one that is recoverable. A queued upload whose op never landed would
        // push bytes nothing references; an op whose upload was never queued is
        // an attachment that stays local, which is precisely the state this
        // vault was already in and which the user can resolve by re-attaching.
        let original = BlobRef::original(&att);
        self.enqueue_blob_upload(att.parent, &original, &att.ciphertext_hash)?;
        self.note_blob_cached(&original)?;
        match (BlobRef::thumbnail(&att), thumbnail) {
            (Some(thumb), Some(sealed)) => {
                self.enqueue_blob_upload(att.parent, &thumb, &sealed.ciphertext_hash)?;
                self.note_blob_cached(&thumb)?;
            }
            // The engine wrote the attachment without it: nothing references
            // these chunks, so nothing will ever upload or read them.
            (None, Some(sealed)) => store.delete_all(&sealed.blob_id)?,
            _ => {}
        }
        Ok(att)
    }

    /// Seal `image` as a one-chunk blob under a fresh key and id, and store
    /// it. The key and id are drawn until neither equals the original's: a
    /// shared key is a nonce reuse, and a shared id would file both blobs'
    /// chunks in one directory.
    fn seal_thumbnail(
        &self,
        store: &BlobStore,
        image: &ThumbnailImage,
        original_key: &[u8; 32],
        original_id: &[u8; 16],
    ) -> Result<Thumbnail, AttachError> {
        let mut blob_key = *original_key;
        while blob_key == *original_key {
            self.rng().fill_bytes(&mut blob_key);
        }
        let mut blob_id = *original_id;
        while blob_id == *original_id {
            self.rng().fill_bytes(&mut blob_id);
        }
        let sealed = seal_chunk(&blob_key, &blob_id, 0, 1, &image.bytes)?;
        let mut hasher = CiphertextHasher::new();
        hasher.update(&sealed);
        store.put_chunk(&blob_id, 0, &sealed)?;
        Ok(Thumbnail {
            blob_id,
            blob_key,
            mime_type: image.mime_type.clone(),
            size_bytes: u32::try_from(image.bytes.len()).unwrap_or(u32::MAX),
            content_hash: content_hash(&image.bytes),
            ciphertext_hash: hasher.finish(),
        })
    }

    /// One attachment's thumbnail: its MIME type and its plaintext, verified.
    ///
    /// `Ok(None)` for an attachment with no usable thumbnail, which a client
    /// draws as the file-type icon.
    ///
    /// # Errors
    ///
    /// [`AttachError::NotFound`] for an unknown or tombstoned id,
    /// [`AttachError::BytesNotHere`] when the thumbnail has not been fetched
    /// yet, and [`AttachError::Chunk`] when what is stored does not open.
    pub async fn thumbnail_bytes(
        &self,
        id: EntityRef,
    ) -> Result<Option<(String, Vec<u8>)>, AttachError> {
        let att = self.attachment_row(id).await?;
        let Some(thumb) = att.thumbnail() else {
            return Ok(None);
        };
        let store = BlobStore::new(self.vault_dir())?;
        let sealed = store
            .get_chunk(&thumb.blob_id, 0)?
            .ok_or(AttachError::BytesNotHere { id })?;
        let plain = open_chunk(&thumb.blob_key, &thumb.blob_id, 0, 1, &sealed)?;
        verify_content(&plain, &thumb.content_hash)?;
        Ok(Some((thumb.mime_type, plain)))
    }

    /// Reassemble one attachment's plaintext from this vault's blob store.
    ///
    /// Verifies the content hash the metadata claims after reassembly, once,
    /// per `docs/03-crypto/data-encryption-format.md` — the per-chunk AEAD tag
    /// has already covered each piece, so a second per-chunk hash would be
    /// duplicated work.
    ///
    /// # Errors
    ///
    /// [`AttachError::NotFound`] for an unknown or tombstoned id,
    /// [`AttachError::BytesNotHere`] when the metadata arrived but the chunks
    /// did not, and [`AttachError::Chunk`] when what is stored does not open or
    /// does not hash to its claim.
    pub async fn attachment_bytes(&self, id: EntityRef) -> Result<Vec<u8>, AttachError> {
        let att = self.attachment_row(id).await?;
        let store = BlobStore::new(self.vault_dir())?;
        if !store.has_all(&att.blob_id, att.chunk_count)? {
            return Err(AttachError::BytesNotHere { id });
        }
        let mut out = Vec::with_capacity(usize::try_from(att.size_bytes).unwrap_or(0));
        for idx in 0..att.chunk_count {
            let sealed = store
                .get_chunk(&att.blob_id, idx)?
                .ok_or(AttachError::BytesNotHere { id })?;
            out.extend_from_slice(&open_chunk(
                &att.blob_key,
                &att.blob_id,
                idx,
                att.chunk_count,
                &sealed,
            )?);
        }
        verify_content(&out, &att.content_hash)?;
        // Every preview and every open reads through here, so this is where
        // the cache learns what was used last (ADR-0053 §5).
        self.touch_blob(&att.blob_id)?;
        Ok(out)
    }

    /// Whether this device holds every chunk of `att`.
    ///
    /// A client needs this to decide between "open" and "not downloaded" on a
    /// row it has metadata for, without reassembling the file to find out.
    ///
    /// # Errors
    ///
    /// A blob-store read error.
    pub fn attachment_is_local(&self, att: &Attachment) -> Result<bool, AttachError> {
        let store = BlobStore::new(self.vault_dir())?;
        Ok(store.has_all(&att.blob_id, att.chunk_count)?)
    }

    /// One live attachment by id.
    pub(crate) async fn attachment_row(&self, id: EntityRef) -> Result<Attachment, AttachError> {
        match self.query(Query::EntityById(id)).await {
            Ok(QueryResult::Attachments(rows)) => rows
                .into_iter()
                .next()
                .ok_or(AttachError::NotFound(id))
                .and_then(|a| {
                    if a.deleted {
                        Err(AttachError::NotFound(id))
                    } else {
                        Ok(a)
                    }
                }),
            // A non-attachment id, and an engine `NotFound`, are the same
            // answer to the caller: this vault has no such attachment.
            Ok(_) | Err(CoreError::Engine(_)) => Err(AttachError::NotFound(id)),
            Err(e) => Err(AttachError::from(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoreConfig;
    use crate::unlock::Unlock;
    use parking_lot::Mutex as PLMutex;
    use std::sync::Arc;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::TaskDraft;

    #[derive(Debug)]
    struct FakeClock(PLMutex<u64>);
    impl crate::config::Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock()
        }
    }

    /// A counting RNG. Not for production, and that is the point: an
    /// attachment's blob key and blob id come from the injected source, so a
    /// test can assert the exact bytes that were sealed.
    #[derive(Debug, Default)]
    struct CountingRng(PLMutex<u8>);
    impl crate::config::Rng for CountingRng {
        fn fill_bytes(&self, dest: &mut [u8]) {
            let mut n = self.0.lock();
            for b in dest.iter_mut() {
                *b = *n;
                *n = n.wrapping_add(1);
            }
        }
    }

    async fn open_vault(dir: &std::path::Path) -> Core {
        let cfg = CoreConfig::with_clock(
            dir.to_path_buf(),
            "0.1.0+test",
            Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
            Arc::new(CountingRng::default()),
        );
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes([7u8; 32]),
                paired: None,
            },
        )
        .await
        .expect("open")
    }

    async fn a_task(core: &Core) -> EntityRef {
        core.submit(Command::CreateTask(TaskDraft {
            title: "Renew the passport".into(),
            ..Default::default()
        }))
        .await
        .expect("create task")
        .entity
    }

    #[tokio::test]
    async fn a_file_round_trips_through_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;

        let bytes = b"%PDF-1.7 a small document".to_vec();
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), &bytes)
            .await
            .expect("attach");

        assert_eq!(att.parent, task);
        assert_eq!(att.filename, "form.pdf");
        assert_eq!(att.size_bytes, bytes.len() as u64);
        assert_eq!(att.chunk_count, 1);
        assert!(core.attachment_is_local(&att).expect("locality"));
        assert_eq!(core.attachment_bytes(att.id).await.expect("read"), bytes);
    }

    /// The reason `chunk_count` exists: a file past one chunk has to reassemble
    /// in order, and the boundary is where an off-by-one would hide.
    #[tokio::test]
    async fn a_multi_chunk_file_reassembles_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;

        let bytes: Vec<u8> = (0..CHUNK_PLAINTEXT_LEN * 2 + 11)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let att = core
            .attach_file(task, "scan.png".into(), "image/png".into(), &bytes)
            .await
            .expect("attach");
        assert_eq!(att.chunk_count, 3);
        assert_eq!(core.attachment_bytes(att.id).await.expect("read"), bytes);
    }

    /// Attachment metadata syncs; blob chunks do not. A replica that has the
    /// row and not the bytes must say so rather than return a short file or an
    /// opaque store error.
    #[tokio::test]
    async fn metadata_without_chunks_reports_that_the_bytes_are_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "notes.txt".into(), "text/plain".into(), b"hello")
            .await
            .expect("attach");

        // Exactly what an unfetched attachment looks like: the row is here and
        // the chunk file is not.
        std::fs::remove_dir_all(dir.path().join("blobs")).expect("drop the chunks");

        assert!(!core.attachment_is_local(&att).expect("locality"));
        assert!(matches!(
            core.attachment_bytes(att.id).await,
            Err(AttachError::BytesNotHere { id }) if id == att.id
        ));
    }

    /// The AEAD tag is the per-chunk integrity check, so a flipped byte in the
    /// store must surface as a decrypt failure — never as corrupted plaintext.
    #[tokio::test]
    async fn a_tampered_chunk_fails_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(
                task,
                "notes.txt".into(),
                "text/plain".into(),
                b"hello there",
            )
            .await
            .expect("attach");

        let store = BlobStore::new(core.vault_dir()).expect("store");
        let mut sealed = store
            .get_chunk(&att.blob_id, 0)
            .expect("read")
            .expect("chunk 0");
        sealed[0] ^= 0x01;
        store.put_chunk(&att.blob_id, 0, &sealed).expect("rewrite");

        assert!(matches!(
            core.attachment_bytes(att.id).await,
            Err(AttachError::Chunk(BlobChunkError::AuthFailed))
        ));
    }

    #[tokio::test]
    async fn an_empty_file_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;

        assert!(matches!(
            core.attach_file(task, "empty".into(), "text/plain".into(), b"")
                .await,
            Err(AttachError::Empty)
        ));
        assert!(matches!(
            core.query(Query::TaskAttachments(task)).await,
            Ok(QueryResult::Attachments(rows)) if rows.is_empty()
        ));
    }

    #[tokio::test]
    async fn reading_an_unknown_or_detached_attachment_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "notes.txt".into(), "text/plain".into(), b"hello")
            .await
            .expect("attach");

        // A task id is not an attachment id.
        assert!(matches!(
            core.attachment_bytes(task).await,
            Err(AttachError::NotFound(_))
        ));

        core.submit(Command::DetachFile(att.id))
            .await
            .expect("detach");
        assert!(matches!(
            core.attachment_bytes(att.id).await,
            Err(AttachError::NotFound(_))
        ));
    }
}
