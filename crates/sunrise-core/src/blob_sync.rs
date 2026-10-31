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
//! [`drain`], which the driver runs, is what talks to the relay.
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
//! `partial: true` attachment retries from byte 0 ... resume from a partial
//! transfer is not part of the design", and the relay exposes no way to ask which chunks it
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

use crate::blob_cache::BlobRef;
use crate::core::{Core, CoreError};
use crate::engine::{hex_short, read_attachment};
use crate::keychain::{to16, to32};
use sunrise_crypto::blob_chunk::{open_chunk, split_sealed, verify_content};
use sunrise_domain::Attachment;
use sunrise_error::ErrorCode;
use sunrise_id::EntityRef;
use sunrise_storage::BlobStore;
use sunrise_sync::{Transport, TransportError};

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
/// waits for the Download button in the next sentence of that section, which is
/// [`Core::fetch_attachment`] and this crate's private `blob_fetch` module.
///
/// This is therefore a bound on what happens **unasked**, and on nothing else.
/// It was the whole story until issue #227, when it was also the only story:
/// with no on-demand route, a larger attachment was permanently unreadable on
/// every device but the one that sealed it. `Core::attachment_bytes` still
/// reports `BytesNotHere` for one whose bytes have not been asked for — but
/// that is now a state with a way out of it rather than a verdict.
///
/// Public because it is the number a client draws its placeholder decision
/// from, and because `crates/sunrise-e2e` proves the over-threshold path
/// against it rather than against a copy.
pub const AUTO_FETCH_MAX_BYTES: u64 = 10 * 1024 * 1024;

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
    /// Queue one sealed blob of the attachment on `parent` for upload: its
    /// original, or its thumbnail, which is a blob of its own (ADR-0053 §2).
    ///
    /// Called by [`Core::attach_file`] once the chunks are on disk and the
    /// metadata op is durable. `INSERT OR IGNORE`, so re-attaching the identical
    /// blob does not reset an upload already in progress.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn enqueue_blob_upload(
        &self,
        parent: EntityRef,
        blob: &BlobRef,
        ciphertext_hash: &[u8; 32],
    ) -> Result<(), CoreError> {
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        let db = self.db();
        // The key domain the parent task lives in: its Stream's, or the
        // private domain's for a task with none (ADR-0046 §2). `init` only
        // checks that the field is non-empty — the relay stores nothing keyed
        // by it — but sending the real one keeps the request honest and
        // matches what the op it describes was routed under.
        let stream_id: Option<Vec<u8>> = db
            .conn()
            .query_row(
                "SELECT stream_id FROM tasks WHERE id = ?",
                rusqlite::params![&parent.bytes()[..]],
                |r| r.get::<_, Option<Vec<u8>>>(0),
            )
            .ok()
            .map(|s| s.unwrap_or_else(|| sunrise_domain::PRIVATE_DOMAIN_BYTES.to_vec()));
        db.conn().execute(
            "INSERT OR IGNORE INTO blob_uploads
             (blob_id, stream_id, chunk_count, size_bytes, ciphertext_hash, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &blob.blob_id[..],
                stream_id.unwrap_or_else(|| vec![0u8; 16]),
                blob.chunk_count,
                i64::try_from(blob.size_bytes).unwrap_or(i64::MAX),
                &ciphertext_hash[..],
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
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let mut out = Vec::with_capacity(rows.len());
        for (blob_id, stream_id, chunk_count, size_bytes, ciphertext_hash, upload_id) in rows {
            let (Some(id), Some(stream), Some(hash)) =
                (to16(&blob_id), to16(&stream_id), to32(&ciphertext_hash))
            else {
                // A wrong-width column is a corrupt row, not an id to pad
                // (`to16`'s doc). It is marked out of attempts rather than
                // skipped, so it leaves the `LIMIT` window instead of taking a
                // slot in every drain, and the row stays inspectable.
                let (reason, n_bytes) = if to16(&blob_id).is_none() {
                    ("blob_id", blob_id.len())
                } else if to16(&stream_id).is_none() {
                    ("stream_id", stream_id.len())
                } else {
                    ("ciphertext_hash", ciphertext_hash.len())
                };
                tracing::warn!(
                    ev = "sync.blob.upload_row_corrupt",
                    reason = reason,
                    n_bytes = n_bytes,
                    "a queued upload's row holds a column of the wrong width; \
                     it is marked out of attempts and not uploaded"
                );
                db.conn().execute(
                    "UPDATE blob_uploads SET attempts = ? WHERE blob_id = ?",
                    rusqlite::params![i64::from(MAX_UPLOAD_ATTEMPTS), &blob_id[..]],
                )?;
                continue;
            };
            out.push(PendingUpload {
                blob_id: id,
                stream_id: stream,
                chunk_count: chunk_count.try_into().unwrap_or(0),
                size_bytes: size_bytes.try_into().unwrap_or(0),
                ciphertext_hash: hash,
                upload_id,
            });
        }
        Ok(out)
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

    /// Blobs this device has metadata for and no bytes for, which it may
    /// fetch without being asked on the network it is on now: thumbnails
    /// first, whatever their attachment's size, then originals under the
    /// threshold that have not been evicted (ADR-0053 §3, §5, §6).
    ///
    /// The read side's whole queue. See the module docs for why this is a query
    /// rather than a buffer.
    ///
    /// # Errors
    /// Storage or blob store failures.
    pub(crate) fn attachments_awaiting_bytes(&self) -> Result<Vec<BlobRef>, CoreError> {
        let policy = self.current_auto_fetch_policy();
        let mut candidates: Vec<BlobRef> = Vec::new();
        {
            let db = self.db();
            let scan = i64::try_from(MAX_FETCH_SCAN).unwrap_or(i64::MAX);
            if policy.thumbnails {
                let ids = ids_where(
                    db.conn(),
                    "SELECT id FROM attachments
                     WHERE deleted = 0 AND thumbnail_blob_id IS NOT NULL
                     ORDER BY created_at_ms DESC, id DESC
                     LIMIT ?",
                    rusqlite::params![scan],
                )?;
                for raw in ids {
                    let Some(id) = attachment_id(&raw) else {
                        continue;
                    };
                    if let Some(a) = read_attachment(db.conn(), &id)? {
                        candidates.extend(BlobRef::thumbnail(&a));
                    }
                }
            }
            if policy.originals {
                // An evicted original stays evicted until somebody opens it:
                // fetching it back unasked would undo the eviction on the next
                // drain and hold the cache at its limit by churning it.
                let ids = ids_where(
                    db.conn(),
                    "SELECT a.id FROM attachments a
                     WHERE a.deleted = 0 AND a.size_bytes <= ?
                       AND NOT EXISTS (SELECT 1 FROM blob_cache c
                                       WHERE c.blob_id = a.blob_id
                                         AND c.evicted_at_ms IS NOT NULL)
                     ORDER BY a.created_at_ms DESC, a.id DESC
                     LIMIT ?",
                    rusqlite::params![
                        i64::try_from(AUTO_FETCH_MAX_BYTES).unwrap_or(i64::MAX),
                        scan
                    ],
                )?;
                for raw in ids {
                    let Some(id) = attachment_id(&raw) else {
                        continue;
                    };
                    if let Some(a) = read_attachment(db.conn(), &id)? {
                        if a.is_fetchable() {
                            candidates.push(BlobRef::original(&a));
                        }
                    }
                }
            }
        }

        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        let mut out = Vec::new();
        for blob in candidates {
            if out.len() >= MAX_FETCHES_PER_DRAIN {
                break;
            }
            if !store
                .has_all(&blob.blob_id, blob.chunk_count)
                .map_err(CoreError::from)?
            {
                out.push(blob);
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
        self.store_fetched(&BlobRef::original(att), body)
    }

    /// [`Core::store_fetched_blob`] for any blob, the original or the
    /// thumbnail. Once the chunks are written the blob is indexed as opened
    /// now and the cache is brought back under its limit.
    ///
    /// # Errors
    /// Blob store failures.
    pub(crate) fn store_fetched(&self, blob: &BlobRef, body: &[u8]) -> Result<bool, CoreError> {
        let Some(sealed) = split_sealed(body, blob.chunk_count) else {
            return Ok(false);
        };
        let mut plaintext = Vec::with_capacity(usize::try_from(blob.size_bytes).unwrap_or(0));
        for (idx, piece) in sealed.iter().enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            match open_chunk(&blob.blob_key, &blob.blob_id, idx, blob.chunk_count, piece) {
                Ok(bytes) => plaintext.extend_from_slice(&bytes),
                Err(_) => return Ok(false),
            }
        }
        if verify_content(&plaintext, &blob.content_hash).is_err() {
            return Ok(false);
        }

        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        for (idx, piece) in sealed.iter().enumerate() {
            let idx = u32::try_from(idx).unwrap_or(u32::MAX);
            store
                .put_chunk(&blob.blob_id, idx, piece)
                .map_err(CoreError::from)?;
        }
        self.note_blob_cached(blob)?;
        Ok(true)
    }
}

/// The raw ids one `SELECT id …` returns.
fn ids_where(
    conn: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Vec<u8>>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// An `attachments.id` read through [`to16`], or `None`, with a warning, for a
/// row whose id is not 16 bytes: padding it would look up some other
/// attachment's id, or one that never existed.
fn attachment_id(raw: &[u8]) -> Option<[u8; 16]> {
    let id = to16(raw);
    if id.is_none() {
        tracing::warn!(
            ev = "sync.blob.fetch_row_corrupt",
            n_bytes = raw.len(),
            "an attachment row's id is not 16 bytes; its bytes are not fetched"
        );
    }
    id
}

/// Move attachment ciphertext in both directions: queued blobs up, missing
/// blobs down.
///
/// This is the step that was absent, and its absence is issue #176: the
/// attachment metadata op reached every device and the bytes reached none, so a
/// Task's body was complete on the machine that made it and incomplete
/// everywhere else. The module doc carries the reasoning for every policy
/// decision below — where the upload is driven from, what a retry re-uses, why
/// the read side is a query rather than a queue, and what each bound is.
///
/// Like [`crate::relay_intents::drain`], a [`TransportError::Unsupported`]
/// transport ends the drain without counting an attempt: the in-process
/// loopbacks carry the op stream and nothing else, and charging them would
/// exhaust `MAX_UPLOAD_ATTEMPTS` on every convergence test that attaches a file.
pub(crate) async fn drain<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    if upload_queued_blobs(core, transport).await.is_err() {
        return;
    }
    // Asked-for downloads before unasked ones. Both halves are sequential and
    // an attachment may be 100 MB, so the order decides how long somebody
    // watching a Download button waits — and nobody is watching the automatic
    // queue. See [`crate::blob_fetch`] for the rest of that route.
    if crate::blob_fetch::drain_requested(core, transport)
        .await
        .is_err()
    {
        return;
    }
    fetch_missing_blobs(core, transport).await;
}

/// `init` → `PUT` → `finalize`, for up to `MAX_UPLOADS_PER_DRAIN` queued blobs.
///
/// `Err(())` means this transport has no blob API, so the caller should stop
/// rather than try the fetch half against it too.
pub(crate) async fn upload_queued_blobs<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
) -> Result<(), ()> {
    let Ok(pending) = core.pending_blob_uploads() else {
        return Ok(());
    };
    for row in pending {
        match upload_one_blob(core, transport, &row).await {
            Ok(()) => {}
            Err(TransportError::Unsupported) => return Err(()),
            Err(e) => {
                // The row survives every failure. Whatever went wrong, the
                // bytes are still on this disk and still not on the relay, and
                // that is exactly what the row says.
                let attempts = core.note_blob_upload_attempt(&row.blob_id).unwrap_or(0);
                // A refusal about the upload *id* is the one thing re-using it
                // cannot survive, so this is where the id is dropped and a
                // fresh `init` is allowed. Anything else — a dropped
                // connection, a 5xx, a refused signature — keeps it, because
                // keeping it is what stops a retry leaving a second pending
                // directory on the relay.
                if matches!(&e, TransportError::Server { code, .. }
                    if *code == ErrorCode::SyncOpInvalid.as_str())
                {
                    let _ = core.forget_blob_upload_id(&row.blob_id);
                }
                tracing::warn!(
                    ev = "sync.blob.upload_failed",
                    err_code = "SYNC_NETWORK_UNAVAILABLE",
                    err_kind = "transient",
                    retryable = attempts < MAX_UPLOAD_ATTEMPTS,
                    blob_h = hex_short(&row.blob_id),
                    attempt = attempts,
                    cause = %e,
                    "an attachment's bytes have not reached the relay"
                );
            }
        }
    }
    Ok(())
}

/// One blob, all the way to a committed relay-side copy.
async fn upload_one_blob<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
    row: &PendingUpload,
) -> Result<(), TransportError> {
    let Ok(Some(chunks)) = core.sealed_chunks(&row.blob_id, row.chunk_count) else {
        // The chunks are not on this disk. Nothing can be uploaded and no
        // number of retries will change that, so the row goes rather than
        // consuming a slot in every drain for the life of the vault.
        let _ = core.clear_blob_upload(&row.blob_id);
        tracing::warn!(
            ev = "sync.blob.upload_abandoned",
            blob_h = hex_short(&row.blob_id),
            "a queued attachment's chunks are missing locally; nothing to upload"
        );
        return Ok(());
    };

    // The id from a previous attempt, or one reserved now. Persisted before the
    // first chunk leaves, so even a crash here retries under this id.
    let upload_id = if let Some(id) = &row.upload_id {
        id.clone()
    } else {
        let id = transport
            .blob_init(&row.stream_id, row.chunk_count, row.size_bytes)
            .await?;
        core.record_blob_upload_id(&row.blob_id, &id)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        id
    };

    for (idx, chunk) in chunks.iter().enumerate() {
        let idx = u32::try_from(idx).unwrap_or(u32::MAX);
        transport.blob_put_chunk(&upload_id, idx, chunk).await?;
    }

    let hashes: Vec<[u8; 32]> = chunks
        .iter()
        .map(|c| sunrise_crypto::ciphertext_hash(std::iter::once(c.as_slice())))
        .collect();
    let commit = transport
        .blob_finalize(&upload_id, &row.ciphertext_hash, &hashes)
        .await?;

    // The relay content-addresses by the hash this device computed while
    // sealing, so the committed id is one this device already predicted — and
    // every other device predicts the same one from the same op. A
    // disagreement would mean the two sides hashed different bytes, which
    // `finalize` should have refused, so it is logged rather than trusted.
    let mut expected = [0u8; 16];
    expected.copy_from_slice(&row.ciphertext_hash[..16]);
    if commit.blob_id != expected {
        tracing::warn!(
            ev = "sync.blob.upload_id_unexpected",
            blob_h = hex_short(&row.blob_id),
            "the relay committed this blob under an address no reader will ask for"
        );
    }

    let _ = core.clear_blob_upload(&row.blob_id);
    tracing::info!(
        ev = "sync.blob.uploaded",
        blob_h = hex_short(&row.blob_id),
        n_chunks = row.chunk_count,
        "an attachment's bytes are on the relay and readable by this account's other devices"
    );
    Ok(())
}

/// Pull the ciphertext for attachments this device has metadata for and bytes
/// for.
async fn fetch_missing_blobs<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(wanted) = core.attachments_awaiting_bytes() else {
        return;
    };
    for att in wanted {
        match transport.blob_fetch(&att.relay_id).await {
            // Not committed yet, or not this account's. Either way there is
            // nothing to hold on to: the attachment row is already the durable
            // record of what to ask for, so the next drain asks again.
            Ok(None) => {}
            Ok(Some(body)) => match core.store_fetched(&att, &body) {
                Ok(true) => tracing::info!(
                    ev = "sync.blob.fetched",
                    blob_h = hex_short(&att.blob_id),
                    "an attachment created on another device is now readable here"
                ),
                Ok(false) => tracing::warn!(
                    ev = "sync.blob.fetch_rejected",
                    err_code = "SYNC_OP_INVALID",
                    err_kind = "user",
                    retryable = true,
                    blob_h = hex_short(&att.blob_id),
                    "the relay returned bytes that are not this attachment's; nothing was stored"
                ),
                Err(e) => tracing::warn!(
                    ev = "sync.blob.fetch_not_stored",
                    blob_h = hex_short(&att.blob_id),
                    cause = %e,
                    "an attachment's bytes arrived and could not be written locally"
                ),
            },
            Err(TransportError::Unsupported) => return,
            Err(e) => tracing::warn!(
                ev = "sync.blob.fetch_failed",
                err_code = "SYNC_NETWORK_UNAVAILABLE",
                err_kind = "transient",
                retryable = true,
                blob_h = hex_short(&att.blob_id),
                cause = %e,
                "an attachment's bytes could not be fetched; it stays unreadable here"
            ),
        }
    }
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
        assert_eq!(wanted[0].attachment, att.id);
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

    /// A queued row whose `blob_id` is 15 bytes is a corrupt row. It is not
    /// padded into a 16-byte id and offered for upload; it is marked out of
    /// attempts, stays in the table, and the well-formed row beside it is
    /// still offered.
    #[tokio::test]
    async fn a_queued_upload_with_a_short_blob_id_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");
        let short = [0xabu8; 15];
        core.db()
            .conn()
            .execute(
                "INSERT INTO blob_uploads
                 (blob_id, stream_id, chunk_count, size_bytes, ciphertext_hash, created_at_ms)
                 VALUES (?, ?, 1, 5, ?, 0)",
                rusqlite::params![&short[..], &[0u8; 16][..], &[0u8; 32][..]],
            )
            .expect("plant a corrupt row");

        let queued = core.pending_blob_uploads().expect("read the queue");
        assert_eq!(queued.len(), 1, "only the well-formed row: {queued:?}");
        assert_eq!(queued[0].blob_id, att.blob_id);

        let attempts: i64 = core
            .db()
            .conn()
            .query_row(
                "SELECT attempts FROM blob_uploads WHERE blob_id = ?",
                rusqlite::params![&short[..]],
                |r| r.get(0),
            )
            .expect("the corrupt row stays in the table");
        assert_eq!(attempts, i64::from(MAX_UPLOAD_ATTEMPTS));
    }

    /// The same for the other two fixed-width columns: a short `stream_id` or
    /// a long `ciphertext_hash` is refused, not padded or truncated.
    #[tokio::test]
    async fn a_queued_upload_with_a_wrong_width_stream_or_hash_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        for (blob_id, stream_id, hash) in [
            ([1u8; 16], vec![0u8; 15], vec![0u8; 32]),
            ([2u8; 16], vec![0u8; 16], vec![0u8; 33]),
        ] {
            core.db()
                .conn()
                .execute(
                    "INSERT INTO blob_uploads
                     (blob_id, stream_id, chunk_count, size_bytes, ciphertext_hash, created_at_ms)
                     VALUES (?, ?, 1, 5, ?, 0)",
                    rusqlite::params![&blob_id[..], stream_id, hash],
                )
                .expect("plant a corrupt row");
        }
        assert!(core.pending_blob_uploads().expect("read").is_empty());
    }

    /// An attachment row whose id is 17 bytes is not truncated into its
    /// neighbour's id, which would offer that attachment's blob twice.
    #[tokio::test]
    async fn an_attachment_row_with_a_wrong_width_id_is_not_offered_for_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "form.pdf".into(), "application/pdf".into(), b"hello")
            .await
            .expect("attach");
        core.db()
            .conn()
            .execute_batch(
                "CREATE TEMP TABLE long_id AS SELECT * FROM attachments;
                 UPDATE long_id SET id = CAST(id || x'00' AS BLOB);
                 INSERT INTO attachments SELECT * FROM long_id;",
            )
            .expect("plant a row with a 17-byte id");
        std::fs::remove_dir_all(dir.path().join("blobs")).expect("drop the chunks");

        let wanted = core.attachments_awaiting_bytes().expect("scan");
        assert_eq!(wanted.len(), 1, "the real row only: {wanted:?}");
        assert_eq!(wanted[0].attachment, att.id);
    }
}
