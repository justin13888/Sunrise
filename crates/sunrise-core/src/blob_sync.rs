//! Getting an attachment's bytes off this device, and another device's bytes
//! onto it.
//!
//! [`crate::attach`] seals an attachment into this vault's blob store and
//! submits the metadata op. Until this module, that was the whole byte path:
//! the op synced, the ciphertext did not, and an attachment was complete on the
//! machine that made it and permanently incomplete everywhere else (issue
//! #176). This is the other half — `blobs/init` → `PUT` → `finalize` outbound,
//! and `GET /blobs/{blob_id}` inbound.
//!
//! # Where the upload is driven from, and why not the write path
//!
//! From the **sync driver**, over the transport it already holds.
//! [`crate::Core::attach_file`] writes a row into `blob_uploads` and returns;
//! `drain_blob_transfers` in [`crate::sync_driver`] is what talks to the relay.
//!
//! Driving it from `attach_file` instead was the other candidate, and it is
//! wrong for three reasons, each of which is a state the user is actually in:
//!
//! 1. **Attaching must work offline.** `attach_file` is what the file importer
//!    and the drop target call, synchronously, while the user waits. An upload
//!    inside it either fails the attach when there is no network — losing a
//!    file the user just chose — or succeeds while silently not uploading,
//!    which is the bug this issue *is*. The same argument produced
//!    `relay_revocation_intents` in migration 0020, and for the same reason: a
//!    device that is gone is the whole scenario there, and a plane is the whole
//!    scenario here.
//! 2. **`attach_file` holds no transport.** `Core` does not own one; the driver
//!    task does, per connection attempt, along with the bearer, the device
//!    binding, the backoff and the only knowledge in the client of whether the
//!    relay is reachable. Giving the write path its own would duplicate all
//!    five and give the duplicate no reconnect policy.
//! 3. **Retry has to outlive the call.** An upload that fails needs to be tried
//!    again later, by something that is still running. `attach_file` returns
//!    the moment the op is submitted.
//!
//! So the write path records an intent and the driver drains it, which is
//! exactly how ordinary ops already reach the relay: `Core::submit` writes the
//! outbox row, the driver sends it.
//!
//! # Retry and resumption
//!
//! Two failures are ordinary and both are answered by **one row, one upload
//! id**.
//!
//! *A chunk `PUT` that fails halfway.* The `upload_id` from `init` is persisted
//! before the first chunk goes out, and the retry re-uses it, re-sending every
//! chunk from zero. The relay keys its pending area by upload id alone, so each
//! re-`PUT` overwrites the chunk already there: the retry costs bandwidth and
//! nothing else. Calling `init` again per attempt is what must not happen —
//! each call mints a fresh id and therefore a fresh pending directory, holding
//! a full copy of the attachment, and nothing sweeps the one left behind.
//! [`Core::forget_blob_upload_id`] is the single place an id is dropped, and it
//! is reached only when the relay has said the id itself is unusable.
//!
//! Resuming from the first missing chunk rather than from zero would be a
//! further optimisation and is deliberately not done:
//! `docs/02-domain/attachments.md` §Lazy fetch already specifies "re-tapping a
//! `partial: true` attachment retries from byte 0 ... resume-from-partial is
//! not implemented in v1", and the relay exposes no way to ask which chunks it
//! already holds, so a client-side guess would be a second source of truth.
//!
//! *A finalize that never arrives.* Either it never reached the relay, in which
//! case nothing was committed and the retry is the first real attempt; or it
//! committed and the response was lost, in which case the relay has already
//! deleted the pending area. The retry re-`PUT`s every chunk under the same
//! upload id — recreating that same directory, not a second one — and finalizes
//! again. `finalize` is content-addressed, so the second commit writes the same
//! bytes under the same blob id as the first. The observable result is
//! identical either way, which is what makes the row safe to drop on success
//! and safe to keep on anything else.
//!
//! # When the bytes are not there yet on the reading device
//!
//! The metadata op can arrive before the blob is committed, and for Stream keys
//! that shape is `deferred_ops`: the op is *parked* because the thing that
//! would make it usable has not arrived, and a bounded, evicting, TTL'd buffer
//! holds it in the meantime.
//!
//! That is the wrong shape here, and the difference is worth stating because
//! the two look alike. A deferred op is parked because **nothing else on the
//! device can reconstruct it** — it is ciphertext, and the key that opens it is
//! precisely what is missing, so if the buffer drops it the op is gone until
//! the relay re-sends. An attachment whose blob has not landed is not like
//! that. The op itself applied cleanly; the row is in `attachments`, durably,
//! with the key, the blob id, the chunk count and — since migration 0022 — the
//! name the relay knows the blob by. Nothing needs to be held in a buffer,
//! because the durable row *is* the record of what to fetch.
//!
//! So the read side is a **pull off the attachments table**, not a queue:
//! [`Core::attachments_awaiting_bytes`] asks the vault which live attachments
//! under the auto-fetch threshold it has metadata for and chunks it does not,
//! and the driver fetches those. A 404 means "not committed yet" and costs a
//! retry on the next drain, not an entry in anything. There is no second
//! structure to bound, evict from, or age out, which is the strongest form of
//! the bound `deferred_ops` has to approximate with two caps and a TTL.
//!
//! # Bounds
//!
//! | What | Bound | Where it comes from |
//! |---|---|---|
//! | Chunk size | 256 KiB plaintext, `SEALED_CHUNK_LEN` on the wire | The frozen v1 blob format. Under the relay's 1 MiB `MAX_CHUNK_BYTES` with room to spare. |
//! | Total size | 100 MB | `MAX_ATTACHMENT_BYTES`, refused by `attach_file` before a byte is sealed, and equal to the relay's own `MAX_BLOB_BYTES`. |
//! | Uploads in flight | [`MAX_UPLOADS_PER_DRAIN`], **sequential** | One at a time, so a backlog cannot monopolise the session or the relay. |
//! | Fetches in flight | [`MAX_FETCHES_PER_DRAIN`], sequential | Same. |
//! | Attempts per upload | [`MAX_UPLOAD_ATTEMPTS`] | A relay that keeps refusing stops being retried; the row stays, so the state is inspectable rather than lost. |
//! | Rows scanned per fetch drain | [`MAX_FETCH_SCAN`] | Bounds the filesystem work one drain does looking for missing chunks. |
//!
//! The upload queue itself needs no cap, and the reason is the one
//! `DEFERRED_TOTAL_CAP` documents from the other side: that cap exists because
//! `defer_op` is reached before anything about the payload is checked, so a
//! *peer* could park bytes of its choosing on every device in the account. A
//! `blob_uploads` row is written only by this device's own `attach_file`, only
//! after the 100 MB ceiling has been enforced, and only for bytes already on
//! this disk. The adversary is the user, and the user is bounded by their own
//! storage.

use crate::core::{Core, CoreError};
use crate::engine::read_attachment;
use sunrise_crypto::blob_chunk::{open_chunk, split_sealed, verify_content};
use sunrise_domain::Attachment;
use sunrise_storage::BlobStore;

/// How many blobs one drain uploads before yielding, and therefore how many
/// are ever in flight: they run one after another, so this is a ceiling on
/// work per drain rather than on concurrency.
///
/// Sequential rather than concurrent because the constraint is the link, not
/// the client. Four 100 MB uploads racing each other over one connection
/// finish no sooner than four in a row and make the first one finish four times
/// later — and the first one finishing is what makes an attachment readable
/// somewhere else.
pub(crate) const MAX_UPLOADS_PER_DRAIN: usize = 4;

/// How many blobs one drain fetches. See [`MAX_UPLOADS_PER_DRAIN`].
pub(crate) const MAX_FETCHES_PER_DRAIN: usize = 4;

/// How many times one blob's upload may be attempted and refused before this
/// device stops trying.
///
/// The row is **kept** at the ceiling rather than deleted: the queue is then
/// still an honest statement of what has not been uploaded, and a future
/// version can offer the user a retry. Deleting it would make a permanently
/// failing upload indistinguishable from a successful one.
pub(crate) const MAX_UPLOAD_ATTEMPTS: u32 = 10;

/// Largest attachment this device fetches without being asked.
///
/// `docs/02-domain/attachments.md` §Lazy fetch: "Default auto-fetch threshold:
/// 10 MiB. Smaller attachments fetch silently on first view." Anything larger
/// is specified to wait for a Download button, which is a client surface that
/// does not exist yet — so today a larger attachment is simply not fetched, and
/// `Core::attachment_bytes` keeps reporting `BytesNotHere`, which is the same
/// answer it gave before this module existed.
pub(crate) const AUTO_FETCH_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// How many live attachment rows one fetch drain examines.
///
/// Finding out whether a blob is local is a filesystem question — one `exists`
/// per chunk — so the scan has to be bounded by something. Newest first,
/// because an attachment that has just arrived is the one a user is about to
/// open, and because an older one has had every drain since it arrived to be
/// fetched already.
pub(crate) const MAX_FETCH_SCAN: usize = 512;

/// One row of `blob_uploads`: a sealed blob that has not reached the relay.
#[derive(Debug, Clone)]
pub(crate) struct PendingUpload {
    /// This vault's blob id — the name the chunks are stored under locally, and
    /// the AAD the chunks were sealed with. Not the relay's id.
    pub blob_id: [u8; 16],
    /// The Stream the parent task belongs to, for `init`.
    pub stream_id: [u8; 16],
    /// How many sealed chunks to send.
    pub chunk_count: u32,
    /// Total ciphertext bytes, for `init`'s advisory size check.
    pub size_bytes: u64,
    /// BLAKE3 over the concatenated ciphertext: `finalize`'s `content_hash`.
    pub ciphertext_hash: [u8; 32],
    /// The upload id `init` handed back, once it has. Re-used by every retry;
    /// see the module docs for what a fresh one per attempt would cost.
    ///
    /// The row's `attempts` column is deliberately *not* carried here. The
    /// driver never branches on it — [`Core::pending_blob_uploads`] has already
    /// applied [`MAX_UPLOAD_ATTEMPTS`] in SQL, so a row that reaches the driver
    /// is by construction one it should try — and a copy the driver could read
    /// but must not act on is an invitation to a second, disagreeing ceiling.
    pub upload_id: Option<String>,
}

impl Core {
    /// Queue `att`'s sealed chunks for upload.
    ///
    /// Called by [`Core::attach_file`] once the chunks are on disk and the
    /// metadata op is durable. `INSERT OR IGNORE`, so re-attaching the identical
    /// blob does not reset an upload already in progress.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn enqueue_blob_upload(&self, att: &Attachment) -> Result<(), CoreError> {
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        let db = self.db();
        // The Stream the parent task lives on. `init` only checks that the
        // field is non-empty — the relay stores nothing keyed by it — but
        // sending the real one keeps the request honest and matches what the
        // op it describes was routed under.
        let stream_id: Option<Vec<u8>> = db
            .conn()
            .query_row(
                "SELECT stream_id FROM tasks WHERE id = ?",
                rusqlite::params![&att.parent.bytes()[..]],
                |r| r.get(0),
            )
            .ok();
        db.conn().execute(
            "INSERT OR IGNORE INTO blob_uploads
             (blob_id, stream_id, chunk_count, size_bytes, ciphertext_hash, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &att.blob_id[..],
                stream_id.unwrap_or_else(|| vec![0u8; 16]),
                att.chunk_count,
                i64::try_from(att.size_bytes).unwrap_or(i64::MAX),
                &att.ciphertext_hash[..],
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// Blobs this device has sealed and not yet uploaded, oldest first.
    ///
    /// Rows past [`MAX_UPLOAD_ATTEMPTS`] are left in the table and skipped
    /// here, so they stop consuming a session without ceasing to be visible.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn pending_blob_uploads(&self) -> Result<Vec<PendingUpload>, CoreError> {
        let db = self.db();
        let mut stmt = db.conn().prepare(
            "SELECT blob_id, stream_id, chunk_count, size_bytes, ciphertext_hash,
                    upload_id
             FROM blob_uploads
             WHERE attempts < ?
             ORDER BY created_at_ms ASC, blob_id ASC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    i64::from(MAX_UPLOAD_ATTEMPTS),
                    i64::try_from(MAX_UPLOADS_PER_DRAIN).unwrap_or(i64::MAX)
                ],
                |r| {
                    Ok(PendingUpload {
                        blob_id: sixteen(&r.get::<_, Vec<u8>>(0)?),
                        stream_id: sixteen(&r.get::<_, Vec<u8>>(1)?),
                        chunk_count: r.get::<_, i64>(2)?.try_into().unwrap_or(0),
                        size_bytes: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
                        ciphertext_hash: thirty_two(&r.get::<_, Vec<u8>>(4)?),
                        upload_id: r.get::<_, Option<String>>(5)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Remember the upload id `init` reserved.
    ///
    /// Written *before* the first chunk goes out, so a crash between `init` and
    /// the first `PUT` still leaves the retry re-using that id rather than
    /// minting a second one.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn record_blob_upload_id(
        &self,
        blob_id: &[u8; 16],
        upload_id: &str,
    ) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "UPDATE blob_uploads SET upload_id = ? WHERE blob_id = ?",
            rusqlite::params![upload_id, &blob_id[..]],
        )?;
        Ok(())
    }

    /// Drop a reserved upload id, so the next attempt calls `init` again.
    ///
    /// The one escape hatch from the re-use rule, and it is deliberately narrow:
    /// reached only when the relay has refused the id *itself* — a 4xx that is
    /// not about the bytes — because retrying an id the relay will never accept
    /// is the one case where keeping it is worse than a second pending
    /// directory. Everything else keeps the id.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn forget_blob_upload_id(&self, blob_id: &[u8; 16]) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "UPDATE blob_uploads SET upload_id = NULL WHERE blob_id = ?",
            rusqlite::params![&blob_id[..]],
        )?;
        Ok(())
    }

    /// Record one refused attempt and return the new count.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn note_blob_upload_attempt(&self, blob_id: &[u8; 16]) -> Result<u32, CoreError> {
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        let db = self.db();
        db.conn().execute(
            "UPDATE blob_uploads
             SET attempts = attempts + 1, last_attempt_ms = ?
             WHERE blob_id = ?",
            rusqlite::params![now_ms, &blob_id[..]],
        )?;
        let attempts: i64 = db.conn().query_row(
            "SELECT attempts FROM blob_uploads WHERE blob_id = ?",
            rusqlite::params![&blob_id[..]],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(attempts).unwrap_or(u32::MAX))
    }

    /// Forget an upload the relay has committed.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn clear_blob_upload(&self, blob_id: &[u8; 16]) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "DELETE FROM blob_uploads WHERE blob_id = ?",
            rusqlite::params![&blob_id[..]],
        )?;
        Ok(())
    }

    /// Every sealed chunk of `blob_id`, in order, or `None` if any is missing.
    ///
    /// All-or-nothing because a partial upload is not worth starting: the relay
    /// refuses `finalize` for a chunk that never arrived, so sending the ones
    /// that exist would cost the whole transfer and commit nothing.
    ///
    /// # Errors
    /// Blob store failures.
    pub(crate) fn sealed_chunks(
        &self,
        blob_id: &[u8; 16],
        chunk_count: u32,
    ) -> Result<Option<Vec<Vec<u8>>>, CoreError> {
        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        let mut out = Vec::with_capacity(chunk_count as usize);
        for idx in 0..chunk_count {
            match store.get_chunk(blob_id, idx).map_err(CoreError::from)? {
                Some(bytes) => out.push(bytes),
                None => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    /// Live attachments this device has metadata for and bytes for, and which
    /// are small enough to fetch without being asked.
    ///
    /// The read side's whole queue. See the module docs for why this is a query
    /// rather than a buffer.
    ///
    /// # Errors
    /// Storage or blob store failures.
    pub(crate) fn attachments_awaiting_bytes(&self) -> Result<Vec<Attachment>, CoreError> {
        let candidates: Vec<Attachment> = {
            let db = self.db();
            let mut stmt = db.conn().prepare(
                "SELECT id FROM attachments
                 WHERE deleted = 0 AND size_bytes <= ?
                 ORDER BY created_at_ms DESC, id DESC
                 LIMIT ?",
            )?;
            let ids = stmt
                .query_map(
                    rusqlite::params![
                        i64::try_from(AUTO_FETCH_MAX_BYTES).unwrap_or(i64::MAX),
                        i64::try_from(MAX_FETCH_SCAN).unwrap_or(i64::MAX)
                    ],
                    |r| r.get::<_, Vec<u8>>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut rows = Vec::new();
            for raw in ids {
                if let Some(a) = read_attachment(db.conn(), &sixteen(&raw))? {
                    if a.is_fetchable() {
                        rows.push(a);
                    }
                }
            }
            rows
        };

        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        let mut out = Vec::new();
        for att in candidates {
            if out.len() >= MAX_FETCHES_PER_DRAIN {
                break;
            }
            if !store
                .has_all(&att.blob_id, att.chunk_count)
                .map_err(CoreError::from)?
            {
                out.push(att);
            }
        }
        Ok(out)
    }

    /// Store a blob body fetched from the relay, after checking it really is
    /// this attachment's.
    ///
    /// `body` is the concatenated ciphertext `GET /blobs/{blob_id}` streamed
    /// back. It is split, every chunk is opened, and the reassembled plaintext
    /// is checked against the attachment's `content_hash` **before anything is
    /// written**. Only then do the chunks land in the blob store, so a bad
    /// download leaves the vault exactly as it was rather than half-populated
    /// with ciphertext that will never open.
    ///
    /// That check is not redundant with the relay's. The relay hashes what it
    /// stored; this hashes what arrived, under this attachment's key and blob
    /// id, and the AEAD's AAD binds the index and the count — so a blob
    /// committed by a different attachment that happened to be addressed here
    /// fails at the tag rather than being stored as this one's.
    ///
    /// Returns whether the body was accepted.
    ///
    /// # Errors
    /// Blob store failures. A body that does not check out is `Ok(false)`, not
    /// an error: it is a statement about the relay's copy, not a fault here.
    pub(crate) fn store_fetched_blob(
        &self,
        att: &Attachment,
        body: &[u8],
    ) -> Result<bool, CoreError> {
        let Some(sealed) = split_sealed(body, att.chunk_count) else {
            return Ok(false);
        };
        let mut plaintext = Vec::with_capacity(usize::try_from(att.size_bytes).unwrap_or(0));
        for (idx, piece) in sealed.iter().enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            match open_chunk(&att.blob_key, &att.blob_id, idx, att.chunk_count, piece) {
                Ok(bytes) => plaintext.extend_from_slice(&bytes),
                Err(_) => return Ok(false),
            }
        }
        if verify_content(&plaintext, &att.content_hash).is_err() {
            return Ok(false);
        }

        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        for (idx, piece) in sealed.iter().enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            store
                .put_chunk(&att.blob_id, idx, piece)
                .map_err(CoreError::from)?;
        }
        Ok(true)
    }
}

/// A stored blob narrowed to the 16 bytes it is; short or long is padded or
/// truncated, the way `engine::ids` already treats one.
fn sixteen(raw: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    let take = raw.len().min(16);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

/// [`sixteen`] at digest width.
fn thirty_two(raw: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    let take = raw.len().min(32);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Command;
    use crate::config::CoreConfig;
    use crate::unlock::Unlock;
    use std::sync::Arc;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::TaskDraft;
    use sunrise_id::EntityRef;

    async fn open_vault(dir: &std::path::Path) -> Arc<Core> {
        let cfg = CoreConfig::production(dir.to_path_buf(), "0.1.0+test");
        Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes([7u8; 32]),
                    paired: None,
                },
            )
            .await
            .expect("open"),
        )
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

    /// Attaching a file queues its blob, and the queued row carries everything
    /// `init` and `finalize` need — so a driver that picks it up needs no
    /// second lookup and no access to the attachment row.
    #[tokio::test]
    async fn attaching_a_file_queues_its_bytes_for_upload() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");

        let queued = core.pending_blob_uploads().expect("read the queue");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].blob_id, att.blob_id);
        assert_eq!(queued[0].chunk_count, att.chunk_count);
        assert_eq!(queued[0].ciphertext_hash, att.ciphertext_hash);
        assert!(
            queued[0].upload_id.is_none(),
            "nothing has been reserved until a session runs"
        );
    }

    /// The idempotency the relay's `pending_store` depends on. A reserved
    /// upload id survives every failure, so a retry re-`PUT`s into the same
    /// pending directory instead of asking for a second one.
    #[tokio::test]
    async fn a_retry_keeps_the_upload_id_it_already_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");

        core.record_blob_upload_id(&att.blob_id, "up_0123456789abcdef0123456789abcdef")
            .expect("reserve");
        for _ in 0..3 {
            let attempts = core
                .note_blob_upload_attempt(&att.blob_id)
                .expect("count a refusal");
            assert!(attempts <= MAX_UPLOAD_ATTEMPTS);
            let queued = core.pending_blob_uploads().expect("read the queue");
            assert_eq!(
                queued[0].upload_id.as_deref(),
                Some("up_0123456789abcdef0123456789abcdef"),
                "a refused attempt must not cost a second pending directory"
            );
        }

        // ...and the one refusal that does release it.
        core.forget_blob_upload_id(&att.blob_id).expect("release");
        assert!(core.pending_blob_uploads().expect("read")[0]
            .upload_id
            .is_none());
    }

    /// A blob the relay has committed leaves the queue, and an exhausted one
    /// stays in the table while ceasing to be offered to the driver.
    #[tokio::test]
    async fn the_queue_stops_offering_an_upload_the_relay_keeps_refusing() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");

        for _ in 0..MAX_UPLOAD_ATTEMPTS {
            core.note_blob_upload_attempt(&att.blob_id)
                .expect("count a refusal");
        }
        assert!(
            core.pending_blob_uploads().expect("read").is_empty(),
            "an exhausted upload must not consume a slot in every drain"
        );

        let still_there: i64 = core
            .db()
            .conn()
            .query_row("SELECT count(*) FROM blob_uploads", [], |r| r.get(0))
            .expect("count rows");
        assert_eq!(
            still_there, 1,
            "the row stays, so a permanently failing upload is visible rather than \
             indistinguishable from a successful one"
        );

        core.clear_blob_upload(&att.blob_id).expect("commit");
        let gone: i64 = core
            .db()
            .conn()
            .query_row("SELECT count(*) FROM blob_uploads", [], |r| r.get(0))
            .expect("count rows");
        assert_eq!(gone, 0);
    }

    /// The read side's queue is the attachments table, so an attachment whose
    /// chunks are here is not offered and one whose chunks are not is.
    #[tokio::test]
    async fn only_attachments_missing_their_bytes_are_offered_for_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");

        assert!(
            core.attachments_awaiting_bytes().expect("scan").is_empty(),
            "the device that sealed it already has every chunk"
        );

        // Exactly what a replica that received the op and not the bytes looks
        // like.
        std::fs::remove_dir_all(dir.path().join("blobs")).expect("drop the chunks");
        let wanted = core.attachments_awaiting_bytes().expect("scan");
        assert_eq!(wanted.len(), 1);
        assert_eq!(wanted[0].id, att.id);
    }

    /// A download is checked against this attachment's own key, blob id and
    /// content hash before a byte is written, so a body that is not this blob
    /// leaves the vault exactly as it was.
    #[tokio::test]
    async fn a_download_that_is_not_this_attachment_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");

        // Capture the real ciphertext, then take the chunks away.
        let sealed = core
            .sealed_chunks(&att.blob_id, att.chunk_count)
            .expect("read")
            .expect("every chunk is here");
        std::fs::remove_dir_all(dir.path().join("blobs")).expect("drop the chunks");

        // Not this blob's bytes: refused, and nothing written.
        assert!(!core
            .store_fetched_blob(&att, b"not this attachment's ciphertext")
            .expect("no error, just a refusal"));
        assert!(matches!(
            core.attachment_bytes(att.id).await,
            Err(crate::AttachError::BytesNotHere { .. })
        ));

        // The real bytes: accepted, and the attachment opens.
        assert!(core
            .store_fetched_blob(&att, &sealed.concat())
            .expect("store"));
        assert_eq!(
            core.attachment_bytes(att.id).await.expect("read"),
            b"hello".to_vec()
        );
    }
}
