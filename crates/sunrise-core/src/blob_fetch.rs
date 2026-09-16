//! Fetching one named attachment because a person asked for it.
//!
//! [`crate::blob_sync`] carries the *unasked* half of
//! `docs/02-domain/attachments.md` §Lazy fetch: "Default auto-fetch threshold:
//! 10 MiB. Smaller attachments fetch silently on first view." It implements
//! that sentence as a `WHERE` clause — `attachments_awaiting_bytes` hands the
//! driver the attachments under the threshold and never mentions the rest — and
//! stopped there, because the next sentence names a client surface that did not
//! exist:
//!
//! > Larger attachments show an inline placeholder with file name, size, and a
//! > "Download" button. The "Cancel" button during transfer aborts and marks
//! > the attachment `partial: true` in cache.
//!
//! With the button unbuilt and the threshold enforced, an attachment over
//! 10 MiB was not *slow* to reach a second device. It was unreachable there:
//! the ciphertext sat on the relay, the metadata sat in the vault, and no code
//! path in the workspace joined them (issue #227). This module is that path.
//!
//! # Why this is a durable request and not a call
//!
//! Because the transport belongs to somebody else. `blob_fetch` is a method on
//! [`sunrise_sync::Transport`], the driver task owns the one transport this
//! client has, and it owns it by `&mut` — so a request made from a UI thread
//! cannot reach the relay except by crossing a task boundary. The two
//! candidates for that crossing are a channel and a table, and the table wins
//! for the reason [`crate::blob_sync`] gives about uploads, read in the other
//! direction: a 100 MB download over a link that drops has to survive the drop,
//! because the alternative is a user who pressed Download, watched nothing
//! happen, and must now find the attachment again. A row also survives the
//! process, so a request made on a train is still a request when the app
//! reopens.
//!
//! So: [`Core::fetch_attachment`] writes a `blob_fetches` row, pokes the
//! driver, and waits for the answer on a broadcast. [`drain_requested`] is what
//! the driver runs.
//!
//! # What the threshold means here
//!
//! Nothing. [`requested_blob_fetches`](Core::requested_blob_fetches) has no
//! size predicate at all, and that is the whole feature:
//! `AUTO_FETCH_MAX_BYTES` is a policy about what this device fetches
//! **without being asked**, and there is nothing unasked about a row in this
//! table. Re-applying it here would reproduce the defect with an extra table
//! in front of it.
//!
//! # Cancellation
//!
//! Two halves, and they are separate because they answer to different clocks.
//!
//! The **durable** half is [`Core::cancel_attachment_fetch`]: it moves the row
//! to `partial` — the document's word — and publishes the outcome, so the
//! waiting client is released immediately whether or not a driver is running or
//! reachable. A Cancel button that only works while the relay is answering is
//! not a Cancel button.
//!
//! The **live** half is a flag the driver is watching. `blob_fetch` is one
//! `await` over however many megabytes, so aborting it means dropping that
//! future, and [`fetch_or_cancel`] is the select that does so. Dropping it is
//! safe because [`Core::store_fetched_blob`] is all-or-nothing: it opens every
//! chunk and checks the whole plaintext against `content_hash` *before* it
//! writes anything, so a transfer abandoned mid-body has written no chunk at
//! all.
//!
//! "Has written no chunk" is not the same promise as "leaves the store ready to
//! start again", though, and the second is what the next attempt needs. A
//! process killed inside `store_fetched_blob`'s write loop *does* leave chunks
//! behind. So the cancel path calls
//! [`discard_partial_blob`](Core::discard_partial_blob), which removes a blob's
//! chunks unless every one of them is present — which is exactly the document's
//! "re-tapping a `partial: true` attachment retries from byte 0", and is why
//! resume-from-partial being out of scope for v1 costs nothing here.

use std::collections::HashSet;

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};

use crate::attach::AttachError;
use crate::blob_sync::MAX_FETCHES_PER_DRAIN;
use crate::core::{Core, CoreError};
use crate::engine::{hex_short, read_attachment};
use crate::events::{AttachmentFetch, AttachmentFetchOutcome, AttachmentFetchState};
use sunrise_domain::Attachment;
use sunrise_id::EntityRef;
use sunrise_storage::BlobStore;
use sunrise_sync::{Transport, TransportError};

/// How many times one requested fetch may fail before the request is parked as
/// `partial` and the waiting client is told.
///
/// The same number as [`crate::blob_sync::MAX_UPLOAD_ATTEMPTS`] and for the
/// same reason, but it is doing a second job here: a person is watching. The
/// commonest reason a first attempt answers `404` is that the *uploading*
/// device has not finished yet, and that resolves on its own within seconds —
/// so a request that gave up on the first refusal would show a failure to the
/// user for a download that was about to work. Ten attempts across drains is
/// long enough to cover that and short enough that a blob the relay does not
/// have stops being asked for.
pub(crate) const MAX_FETCH_ATTEMPTS: u32 = 10;

/// How many broadcast outcomes a slow subscriber may fall behind before it is
/// lagged.
///
/// Small on purpose: a client waiting in [`Core::fetch_attachment`] is awaiting
/// one event, and [`Core::fetch_attachment`] re-reads the durable state on
/// `Lagged` rather than trusting the channel — so the buffer is a latency
/// optimisation, not the mechanism.
const OUTCOME_BUFFER: usize = 32;

/// The row state a `blob_fetches` row is in while the driver should act on it.
const STATE_REQUESTED: &str = "requested";

/// `docs/02-domain/attachments.md` §Lazy fetch's `partial: true`.
const STATE_PARTIAL: &str = "partial";

/// The live half of a fetch request: who has been cancelled, and who is
/// listening for outcomes.
///
/// Held by [`Core`] and shared with the driver task. Deliberately not part of
/// `SyncShared`: this outlives any one session — a request survives a
/// reconnect — and `SyncShared` is scoped to the driver's own state.
#[derive(Debug)]
pub(crate) struct BlobFetchSignals {
    /// Attachments a client has asked to stop fetching.
    ///
    /// A set rather than a single id because two panes can be downloading two
    /// attachments, and the driver is the only thing that knows which one it is
    /// mid-transfer on.
    cancelled: Mutex<HashSet<EntityRef>>,
    /// Woken on every cancellation. Level-triggered by convention: a waiter
    /// re-reads [`BlobFetchSignals::cancelled`] after being woken rather than
    /// treating the wake as the message.
    cancel: Notify,
    /// Finished fetches, for whoever is waiting on one.
    outcomes: broadcast::Sender<AttachmentFetch>,
}

impl BlobFetchSignals {
    /// A fresh set of signals with nothing cancelled and nobody listening.
    pub(crate) fn new() -> Self {
        let (outcomes, _) = broadcast::channel(OUTCOME_BUFFER);
        Self {
            cancelled: Mutex::new(HashSet::new()),
            cancel: Notify::new(),
            outcomes,
        }
    }

    /// Listen for finished fetches.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<AttachmentFetch> {
        self.outcomes.subscribe()
    }

    /// Announce a finished fetch. Best-effort: nobody listening is the ordinary
    /// case for a fetch nothing is waiting on.
    fn publish(&self, attachment: EntityRef, outcome: AttachmentFetchOutcome) {
        let _ = self.outcomes.send(AttachmentFetch {
            attachment,
            outcome,
        });
    }

    /// Ask any transfer of `id` to stop, and wake the driver so it notices.
    fn request_cancel(&self, id: EntityRef) {
        self.cancelled.lock().insert(id);
        self.cancel.notify_waiters();
    }

    /// Whether `id` has been cancelled and not yet cleared.
    fn is_cancelled(&self, id: EntityRef) -> bool {
        self.cancelled.lock().contains(&id)
    }

    /// Forget a cancellation, so the next request for `id` is not born
    /// cancelled.
    fn clear_cancel(&self, id: EntityRef) {
        self.cancelled.lock().remove(&id);
    }
}

impl Core {
    /// Fetch one attachment's bytes now, whatever its size, and wait for the
    /// answer.
    ///
    /// This is the route past `AUTO_FETCH_MAX_BYTES` that
    /// `docs/02-domain/attachments.md` §Lazy fetch's "Download" button needs.
    /// It returns when the chunks are on this device, when a client calls
    /// [`Core::cancel_attachment_fetch`], or when the relay has refused it
    /// `MAX_FETCH_ATTEMPTS` times — not on a timer, because the caller
    /// already has the better instrument: a Cancel button the user is looking
    /// at.
    ///
    /// Idempotent and cheap for an attachment already here: that case is a
    /// blob-store `exists` per chunk and no row, which is what makes this safe
    /// to call from a view that does not track what it has.
    ///
    /// # Errors
    ///
    /// [`AttachError::NotFound`] for an unknown or tombstoned id;
    /// [`AttachError::NotOnRelay`] for an attachment no device ever uploaded,
    /// which no amount of waiting will produce; [`AttachError::SyncOffline`]
    /// when no sync driver is running, since there is then nothing that could
    /// ever service the request; [`AttachError::FetchCancelled`] and
    /// [`AttachError::FetchUnavailable`] for the two ways a started transfer
    /// ends without bytes.
    pub async fn fetch_attachment(&self, id: EntityRef) -> Result<(), AttachError> {
        let att = self.attachment_row(id).await?;
        if self.attachment_is_local(&att)? {
            // Nothing to ask for. Clearing any stale row here rather than
            // leaving it is what stops a `partial` mark outliving the download
            // that succeeded after it.
            self.clear_blob_fetch(id)?;
            return Ok(());
        }
        if !att.is_fetchable() {
            return Err(AttachError::NotOnRelay { id });
        }
        if !self.sync_is_active() {
            return Err(AttachError::SyncOffline { id });
        }

        // Subscribe *before* the row is written. The driver can be mid-drain on
        // the poke from an unrelated submit, so a subscription taken afterwards
        // can miss the outcome of the very request being made.
        let mut outcomes = self.blob_fetch_signals().subscribe();
        self.blob_fetch_signals().clear_cancel(id);
        self.enqueue_blob_fetch(&att)?;
        tracing::info!(
            ev = "sync.blob.fetch_requested",
            attachment_h = hex_short(att.id.bytes()),
            blob_h = hex_short(&att.blob_id),
            n_bytes = att.size_bytes,
            "a client asked for an attachment's bytes, past the auto-fetch threshold"
        );
        // The same wake `attach_file` uses. `SessionEvent::Submit` already
        // drains blob transfers, so the request is acted on in this session
        // rather than at the next reconnect.
        self.poke_sync();

        loop {
            let event = tokio::select! {
                biased;
                // A vault closing under a pending download must not strand the
                // caller: the driver task is aborted on shutdown, so nothing
                // would ever publish the outcome this is waiting for.
                () = self.sync_shutdown_notified() => {
                    return Err(AttachError::from(CoreError::Closed))
                }
                r = outcomes.recv() => r,
            };
            match event {
                Ok(ev) if ev.attachment == id => {
                    return match ev.outcome {
                        AttachmentFetchOutcome::Fetched => Ok(()),
                        AttachmentFetchOutcome::Cancelled => {
                            Err(AttachError::FetchCancelled { id })
                        }
                        AttachmentFetchOutcome::Unavailable => {
                            Err(AttachError::FetchUnavailable { id })
                        }
                    }
                }
                // Somebody else's attachment.
                Ok(_) => {}
                // A burst of outcomes overran this subscriber. The channel is
                // an optimisation, so fall back to the durable state, which is
                // authoritative either way.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if self.attachment_is_local(&att)? {
                        return Ok(());
                    }
                    if self.attachment_fetch_state(id)? == AttachmentFetchState::Partial {
                        return Err(AttachError::FetchUnavailable { id });
                    }
                }
                // Only reachable if `Core` itself has gone, which it has not:
                // `self` owns the sender. Answered rather than asserted.
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(AttachError::from(CoreError::Closed))
                }
            }
        }
    }

    /// Abort a running fetch of `id` and mark it `partial`.
    ///
    /// `docs/02-domain/attachments.md` §Lazy fetch: "The 'Cancel' button during
    /// transfer aborts and marks the attachment `partial: true` in cache."
    ///
    /// Synchronous, and does not wait for the driver to acknowledge. The
    /// button's job is to release the user, and a device with no reachable
    /// relay — the state a user is most likely to be cancelling *from* — has
    /// no driver activity to acknowledge anything. So the durable mark and the
    /// outcome are made here, and the driver's copy of the signal only decides
    /// how soon the socket stops reading.
    ///
    /// A no-op for an id with no request outstanding, including one whose
    /// transfer finished a moment ago: the `UPDATE` is conditioned on the row
    /// still being `requested`, so a cancel that loses that race does not
    /// contradict the `Fetched` outcome already published.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub fn cancel_attachment_fetch(&self, id: EntityRef) -> Result<(), AttachError> {
        // The flag first. A transfer already running in the driver is watching
        // it, and must be able to see it set the instant the wake arrives.
        self.blob_fetch_signals().request_cancel(id);
        if self.mark_blob_fetch_partial(id)? {
            self.blob_fetch_signals()
                .publish(id, AttachmentFetchOutcome::Cancelled);
        }
        Ok(())
    }

    /// This device's cache state for one attachment's bytes.
    ///
    /// What a client draws the placeholder from, beside
    /// [`Core::attachment_is_local`]: `Idle` with no local bytes is the
    /// "Download" state, `Requested` is the transfer with its Cancel button,
    /// and `Partial` is the interrupted transfer the document says re-tapping
    /// restarts from byte 0.
    ///
    /// # Errors
    ///
    /// Storage failures.
    pub fn attachment_fetch_state(
        &self,
        id: EntityRef,
    ) -> Result<AttachmentFetchState, AttachError> {
        let db = self.db();
        let state: Option<String> = db
            .conn()
            .query_row(
                "SELECT state FROM blob_fetches WHERE attachment_id = ?",
                rusqlite::params![&id.bytes()[..]],
                |r| r.get(0),
            )
            .map_or_else(
                |e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                },
                |v: String| Ok(Some(v)),
            )
            .map_err(CoreError::from)?;
        Ok(match state.as_deref() {
            Some(STATE_REQUESTED) => AttachmentFetchState::Requested,
            Some(STATE_PARTIAL) => AttachmentFetchState::Partial,
            _ => AttachmentFetchState::Idle,
        })
    }

    /// Finished attachment fetches, for a client that wants to follow them
    /// without awaiting one.
    ///
    /// [`Core::fetch_attachment`] is the ordinary way to learn a fetch's
    /// outcome. This exists for the second view of the same attachment — a list
    /// row beside an open editor — which never called `fetch_attachment` and
    /// still has to stop drawing a spinner.
    pub fn attachment_fetches(&self) -> broadcast::Receiver<AttachmentFetch> {
        self.blob_fetch_signals().subscribe()
    }

    /// Record that `att`'s bytes have been asked for.
    ///
    /// Upsert rather than `INSERT OR IGNORE`, and that is the difference
    /// between a Cancel the user can undo and one they cannot: a cancelled
    /// request is left in the table as `partial`, so an insert that yielded to
    /// the existing row would make the second press of Download do nothing at
    /// all. Re-asking resets the state *and* the attempt count, because the
    /// count is a statement about one transfer and this is a new one.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn enqueue_blob_fetch(&self, att: &Attachment) -> Result<(), CoreError> {
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        let db = self.db();
        db.conn().execute(
            "INSERT INTO blob_fetches
             (attachment_id, blob_id, state, requested_at_ms, attempts, last_attempt_ms)
             VALUES (?, ?, ?, ?, 0, NULL)
             ON CONFLICT(attachment_id) DO UPDATE SET
                 state = excluded.state,
                 requested_at_ms = excluded.requested_at_ms,
                 attempts = 0,
                 last_attempt_ms = NULL",
            rusqlite::params![
                &att.id.bytes()[..],
                &att.blob_id[..],
                STATE_REQUESTED,
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// The attachments a client has asked this device to fetch, oldest request
    /// first.
    ///
    /// **No size predicate.** The contrast with
    /// [`Core::attachments_awaiting_bytes`](crate::Core), which filters on
    /// `size_bytes <= AUTO_FETCH_MAX_BYTES`, is the entire point of this
    /// module: that threshold governs what this device fetches unasked, and a
    /// row here is a person asking.
    ///
    /// Oldest first because a user who pressed Download twice is waiting on the
    /// first answer, and bounded by [`MAX_FETCHES_PER_DRAIN`] for the reason
    /// the automatic queue is: the fetches run one after another, so this caps
    /// the work one drain does rather than any concurrency.
    ///
    /// # Errors
    /// Storage or blob store failures.
    pub(crate) fn requested_blob_fetches(&self) -> Result<Vec<Attachment>, CoreError> {
        let candidates: Vec<Attachment> = {
            let db = self.db();
            let mut stmt = db.conn().prepare(
                "SELECT attachment_id FROM blob_fetches
                 WHERE state = ?
                 ORDER BY requested_at_ms ASC, attachment_id ASC
                 LIMIT ?",
            )?;
            let ids = stmt
                .query_map(
                    rusqlite::params![
                        STATE_REQUESTED,
                        i64::try_from(MAX_FETCHES_PER_DRAIN).unwrap_or(i64::MAX)
                    ],
                    |r| r.get::<_, Vec<u8>>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut rows = Vec::new();
            for raw in ids {
                let mut id = [0u8; 16];
                let take = raw.len().min(16);
                id[..take].copy_from_slice(&raw[..take]);
                if let Some(a) = read_attachment(db.conn(), &id)? {
                    rows.push(a);
                }
            }
            rows
        };

        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        let mut out = Vec::new();
        for att in candidates {
            if att.deleted || !att.is_fetchable() {
                continue;
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

    /// Record one failed attempt at a requested fetch and return the new count.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn note_blob_fetch_attempt(&self, id: EntityRef) -> Result<u32, CoreError> {
        let now_ms = i64::try_from(self.now_ms()).unwrap_or(i64::MAX);
        let db = self.db();
        db.conn().execute(
            "UPDATE blob_fetches
             SET attempts = attempts + 1, last_attempt_ms = ?
             WHERE attachment_id = ?",
            rusqlite::params![now_ms, &id.bytes()[..]],
        )?;
        let attempts: i64 = db
            .conn()
            .query_row(
                "SELECT attempts FROM blob_fetches WHERE attachment_id = ?",
                rusqlite::params![&id.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(u32::try_from(attempts).unwrap_or(u32::MAX))
    }

    /// Mark a running request `partial`, and report whether it was still
    /// running.
    ///
    /// `false` means the row had already left the `requested` state — finished,
    /// or cancelled a moment ago — and is what keeps a losing cancel from
    /// contradicting an outcome already published.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn mark_blob_fetch_partial(&self, id: EntityRef) -> Result<bool, CoreError> {
        let db = self.db();
        let changed = db.conn().execute(
            "UPDATE blob_fetches SET state = ? WHERE attachment_id = ? AND state = ?",
            rusqlite::params![STATE_PARTIAL, &id.bytes()[..], STATE_REQUESTED],
        )?;
        Ok(changed > 0)
    }

    /// Forget a request. Called when the chunks are here, and when the
    /// attachment they name is not.
    ///
    /// # Errors
    /// Storage failures.
    pub(crate) fn clear_blob_fetch(&self, id: EntityRef) -> Result<(), CoreError> {
        let db = self.db();
        db.conn().execute(
            "DELETE FROM blob_fetches WHERE attachment_id = ?",
            rusqlite::params![&id.bytes()[..]],
        )?;
        Ok(())
    }

    /// Remove `att`'s chunks unless every one of them is present, and report
    /// whether anything went.
    ///
    /// What makes an abandoned transfer restartable. `store_fetched_blob`
    /// writes all of a blob's chunks or none, so an ordinary cancel finds
    /// nothing here — but a process killed inside that write loop does not get
    /// to finish it, and the chunks it did write are ciphertext the next
    /// attempt would leave in place while believing it had started clean.
    ///
    /// The `has_all` guard is not a nicety. Without it this is a method that
    /// deletes a complete, verified attachment from the cache on a cancel that
    /// arrived one instant late.
    ///
    /// # Errors
    /// Blob store failures.
    pub(crate) fn discard_partial_blob(&self, att: &Attachment) -> Result<bool, CoreError> {
        let store = BlobStore::new(self.vault_dir()).map_err(CoreError::from)?;
        if store
            .has_all(&att.blob_id, att.chunk_count)
            .map_err(CoreError::from)?
        {
            return Ok(false);
        }
        store.delete_all(&att.blob_id).map_err(CoreError::from)?;
        Ok(true)
    }
}

/// Pull the ciphertext for attachments a client has asked for, ignoring the
/// auto-fetch threshold.
///
/// Run by the sync driver before [`crate::sync_driver`]'s automatic fetch
/// drain, because somebody is watching this one and nobody is watching that
/// one. `Err(())` means this transport has no blob API, which is the caller's
/// signal to stop rather than try the automatic half against it too.
///
/// It lives here rather than beside its sibling in `sync_driver` because
/// [`fetch_or_cancel`] is the subject of this module and the two are one
/// mechanism read in one place.
pub(crate) async fn drain_requested<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
) -> Result<(), ()> {
    let Ok(wanted) = core.requested_blob_fetches() else {
        return Ok(());
    };
    for att in wanted {
        // Everything but `Unsupported` has already been counted against the
        // request and logged inside; the next drain tries again.
        if matches!(
            fetch_one(core, transport, &att).await,
            Err(TransportError::Unsupported)
        ) {
            return Err(());
        }
    }
    Ok(())
}

/// One requested attachment, all the way to a stored blob or a parked request.
async fn fetch_one<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
    att: &Attachment,
) -> Result<(), TransportError> {
    let signals = core.blob_fetch_signals();
    // A cancel that arrived before the driver reached this row. Checked here as
    // well as inside the select because the select's own check races only the
    // transfer, not the queue.
    if signals.is_cancelled(att.id) {
        note_cancelled(core, att);
        return Ok(());
    }
    let Some(relay_id) = att.relay_blob_id() else {
        // Filtered out by `requested_blob_fetches`, so this is defensive rather
        // than reachable. Answered instead of asserted: the request is the
        // user's, and dropping one silently is the failure mode of the feature.
        park(core, att, "not_on_relay");
        return Ok(());
    };

    match fetch_or_cancel(signals, transport, att.id, &relay_id).await {
        Fetched::Cancelled => {
            note_cancelled(core, att);
            Ok(())
        }
        // Not committed yet, or not this account's. Ordinary rather than
        // wrong — the commonest instance is a device still uploading — so it
        // counts an attempt and waits for the next drain.
        Fetched::Body(None) => {
            count_failure(core, att, "not_on_relay", None);
            Ok(())
        }
        Fetched::Body(Some(body)) => {
            match core.store_fetched_blob(att, &body) {
                Ok(true) => {
                    let _ = core.clear_blob_fetch(att.id);
                    signals.clear_cancel(att.id);
                    signals.publish(att.id, AttachmentFetchOutcome::Fetched);
                    tracing::info!(
                        ev = "sync.blob.fetched",
                        attachment_h = hex_short(att.id.bytes()),
                        blob_h = hex_short(&att.blob_id),
                        n_bytes = att.size_bytes,
                        "an attachment a client asked for is now readable here"
                    );
                }
                Ok(false) => count_failure(core, att, "rejected", None),
                Err(e) => count_failure(core, att, "not_stored", Some(&e.to_string())),
            }
            Ok(())
        }
        Fetched::Failed(TransportError::Unsupported) => Err(TransportError::Unsupported),
        Fetched::Failed(e) => {
            count_failure(core, att, "transport", Some(&e.to_string()));
            Ok(())
        }
    }
}

/// What one attempt at a requested blob produced.
enum Fetched {
    /// The relay answered: the concatenated ciphertext, or `None` for a blob it
    /// does not hold.
    Body(Option<Vec<u8>>),
    /// A client cancelled while the body was in flight.
    Cancelled,
    /// The transport failed.
    Failed(TransportError),
}

/// `GET /blobs/{id}`, abandoned if the attachment is cancelled first.
///
/// The loop exists because [`Notify`] is edge-triggered and the set is the
/// truth: a wake means *somebody* was cancelled, so this re-reads whether it
/// was this one and goes back to waiting if not. Arming the notification before
/// reading the set is what closes the lost-wakeup window — a cancel landing
/// between the read and the arm would otherwise wake nobody, and the user would
/// watch a Cancel button do nothing until the download finished on its own.
async fn fetch_or_cancel<T: Transport + ?Sized>(
    signals: &BlobFetchSignals,
    transport: &mut T,
    id: EntityRef,
    relay_id: &[u8; 16],
) -> Fetched {
    let body = transport.blob_fetch(relay_id);
    tokio::pin!(body);
    loop {
        let wake = signals.cancel.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        if signals.is_cancelled(id) {
            return Fetched::Cancelled;
        }
        tokio::select! {
            biased;
            () = &mut wake => {}
            r = &mut body => {
                return match r {
                    Ok(b) => Fetched::Body(b),
                    Err(e) => Fetched::Failed(e),
                }
            }
        }
    }
}

/// Settle a cancelled transfer: discard its debris, park the row, clear the
/// signal.
///
/// Publishes nothing. [`Core::cancel_attachment_fetch`] already did, at the
/// moment the user pressed the button, which is the moment they should have
/// been released — this is the driver catching up with a decision already
/// taken.
fn note_cancelled(core: &Core, att: &Attachment) {
    let discarded = core.discard_partial_blob(att).unwrap_or(false);
    let _ = core.mark_blob_fetch_partial(att.id);
    core.blob_fetch_signals().clear_cancel(att.id);
    tracing::info!(
        ev = "sync.blob.fetch_cancelled",
        attachment_h = hex_short(att.id.bytes()),
        blob_h = hex_short(&att.blob_id),
        mode = if discarded { "discarded" } else { "clean" },
        "a client cancelled an attachment download; it is marked partial and restarts from byte 0"
    );
}

/// Count one failed attempt, and park the request if that was the last one.
fn count_failure(core: &Core, att: &Attachment, reason: &'static str, cause: Option<&str>) {
    let attempts = core.note_blob_fetch_attempt(att.id).unwrap_or(u32::MAX);
    if attempts >= MAX_FETCH_ATTEMPTS {
        park(core, att, reason);
        return;
    }
    tracing::warn!(
        ev = "sync.blob.fetch_failed",
        err_code = "SYNC_NETWORK_UNAVAILABLE",
        err_kind = "transient",
        retryable = true,
        attachment_h = hex_short(att.id.bytes()),
        blob_h = hex_short(&att.blob_id),
        attempt = attempts,
        reason = reason,
        cause = cause.unwrap_or("-"),
        "a requested attachment's bytes have not arrived; the request stands"
    );
}

/// Park a request as `partial` and tell whoever is waiting.
fn park(core: &Core, att: &Attachment, reason: &'static str) {
    let _ = core.discard_partial_blob(att);
    if core.mark_blob_fetch_partial(att.id).unwrap_or(false) {
        core.blob_fetch_signals()
            .publish(att.id, AttachmentFetchOutcome::Unavailable);
    }
    core.blob_fetch_signals().clear_cancel(att.id);
    tracing::warn!(
        ev = "sync.blob.fetch_abandoned",
        err_code = "SYNC_NETWORK_UNAVAILABLE",
        err_kind = "transient",
        retryable = true,
        attachment_h = hex_short(att.id.bytes()),
        blob_h = hex_short(&att.blob_id),
        reason = reason,
        "a requested attachment download is out of attempts; it is marked partial"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_sync::AUTO_FETCH_MAX_BYTES;
    use crate::commands::Command;
    use crate::config::CoreConfig;
    use crate::unlock::Unlock;
    use std::sync::Arc;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::TaskDraft;

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
            title: "Countersign the lease".into(),
            ..Default::default()
        }))
        .await
        .expect("create task")
        .entity
    }

    /// Attach `bytes` and then take the chunks away, which is exactly the state
    /// a replica that received the metadata op and not the ciphertext is in.
    async fn a_remote_attachment(
        core: &Core,
        dir: &std::path::Path,
        bytes: &[u8],
    ) -> sunrise_domain::Attachment {
        let task = a_task(core).await;
        let att = core
            .attach_file(task, "lease.pdf".into(), "application/pdf".into(), bytes)
            .await
            .expect("attach");
        std::fs::remove_dir_all(dir.join("blobs")).expect("drop the chunks");
        att
    }

    /// The defect issue #227 reports, stated as the difference between the two
    /// queues: the automatic one will not look at an attachment over the
    /// threshold, and the requested one does not care what size it is.
    ///
    /// Over the threshold by one byte rather than by a comfortable margin,
    /// because the boundary is the thing being asserted.
    #[tokio::test]
    async fn a_requested_fetch_ignores_the_auto_fetch_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let big = vec![7u8; usize::try_from(AUTO_FETCH_MAX_BYTES).unwrap() + 1];
        let att = a_remote_attachment(&core, dir.path(), &big).await;

        assert!(
            core.attachments_awaiting_bytes().expect("scan").is_empty(),
            "an attachment over the threshold is never fetched unasked — that is the \
             threshold working, and before #227 it was also the end of the road"
        );
        assert!(
            core.requested_blob_fetches().expect("scan").is_empty(),
            "nobody has asked for it yet"
        );

        core.enqueue_blob_fetch(&att).expect("request");
        let wanted = core.requested_blob_fetches().expect("scan");
        assert_eq!(wanted.len(), 1);
        assert_eq!(wanted[0].id, att.id);
        assert!(
            wanted[0].size_bytes > AUTO_FETCH_MAX_BYTES,
            "the queue the driver reads must be able to name a blob the threshold refused"
        );
    }

    /// The document's sentence: "The 'Cancel' button during transfer aborts and
    /// marks the attachment `partial: true` in cache."
    #[tokio::test]
    async fn cancelling_marks_the_request_partial_and_stops_the_driver_seeing_it() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let att = a_remote_attachment(&core, dir.path(), b"%PDF-1.7 a lease").await;

        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Idle,
            "an attachment nobody has asked for is not partial, it is untouched"
        );

        core.enqueue_blob_fetch(&att).expect("request");
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Requested
        );

        core.cancel_attachment_fetch(att.id).expect("cancel");
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Partial
        );
        assert!(
            core.requested_blob_fetches().expect("scan").is_empty(),
            "a cancelled request must stop being work the driver picks up"
        );
    }

    /// "Re-tapping a `partial: true` attachment retries from byte 0."
    ///
    /// The row is left behind on cancel rather than deleted — that is what
    /// makes the state visible — so the second press has to be able to move it
    /// back. An insert that yielded to the existing row would make Download do
    /// nothing at all the second time, which is the one failure a user cannot
    /// distinguish from the feature not existing.
    #[tokio::test]
    async fn a_cancelled_fetch_can_be_asked_for_again() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let att = a_remote_attachment(&core, dir.path(), b"%PDF-1.7 a lease").await;

        core.enqueue_blob_fetch(&att).expect("request");
        core.note_blob_fetch_attempt(att.id).expect("one refusal");
        core.cancel_attachment_fetch(att.id).expect("cancel");

        core.enqueue_blob_fetch(&att).expect("ask again");
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Requested
        );
        let wanted = core.requested_blob_fetches().expect("scan");
        assert_eq!(wanted.len(), 1, "the second press must reach the driver");
        assert_eq!(wanted[0].id, att.id);

        let attempts: i64 = core
            .db()
            .conn()
            .query_row(
                "SELECT attempts FROM blob_fetches WHERE attachment_id = ?",
                rusqlite::params![&att.id.bytes()[..]],
                |r| r.get(0),
            )
            .expect("read the row");
        assert_eq!(
            attempts, 0,
            "the attempt count describes one transfer, and this is a new one"
        );
    }

    /// What "leaves the store in a state the next attempt can start from"
    /// means with chunk granularity and no resume: nothing of a half-written
    /// blob survives, and a whole one is untouchable.
    #[tokio::test]
    async fn an_abandoned_transfer_leaves_no_chunks_behind_and_a_finished_one_keeps_all_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let bytes: Vec<u8> = (0..sunrise_crypto::blob_chunk::CHUNK_PLAINTEXT_LEN * 2 + 11)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "lease.pdf".into(), "application/pdf".into(), &bytes)
            .await
            .expect("attach");
        assert_eq!(att.chunk_count, 3);

        // A complete blob is what a *finished* fetch leaves, and discarding it
        // would be deleting the user's attachment on a late cancel.
        assert!(
            !core.discard_partial_blob(&att).expect("discard"),
            "a blob with every chunk present is not partial"
        );
        assert!(core.attachment_is_local(&att).expect("locality"));

        // A process killed inside `store_fetched_blob`'s write loop leaves this
        // shape: some chunks, not all.
        let store = BlobStore::new(dir.path()).expect("store");
        let chunk0 = store
            .get_chunk(&att.blob_id, 0)
            .expect("read")
            .expect("chunk 0");
        std::fs::remove_dir_all(dir.path().join("blobs")).expect("drop the chunks");
        store.put_chunk(&att.blob_id, 0, &chunk0).expect("re-write");

        assert!(
            core.discard_partial_blob(&att).expect("discard"),
            "a partly-written blob is debris the next attempt must not inherit"
        );
        assert!(store.get_chunk(&att.blob_id, 0).expect("read").is_none());
    }

    /// The cancel that loses its race.
    ///
    /// A transfer that finished a microsecond before the button was pressed has
    /// already published `Fetched` and deleted its row. Marking it `partial`
    /// then would contradict an answer the client already has — and would
    /// leave a permanent `partial` badge on an attachment that opens.
    #[tokio::test]
    async fn a_cancel_that_arrives_after_the_bytes_did_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let att = a_remote_attachment(&core, dir.path(), b"%PDF-1.7 a lease").await;

        core.enqueue_blob_fetch(&att).expect("request");
        core.clear_blob_fetch(att.id)
            .expect("the transfer finished");

        assert!(
            !core.mark_blob_fetch_partial(att.id).expect("mark"),
            "there is no running request to cancel"
        );
        core.cancel_attachment_fetch(att.id).expect("cancel");
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Idle
        );
    }

    /// An attachment whose bytes are already here needs no relay, no driver and
    /// no row — which is what makes this safe to call from a view that does not
    /// track what it holds. It also clears a stale `partial` mark, so a
    /// cancelled download that later arrived by the automatic queue stops
    /// advertising itself as interrupted.
    #[tokio::test]
    async fn fetching_an_attachment_that_is_already_here_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let task = a_task(&core).await;
        let att = core
            .attach_file(task, "lease.pdf".into(), "application/pdf".into(), b"here")
            .await
            .expect("attach");

        core.enqueue_blob_fetch(&att).expect("request");
        core.cancel_attachment_fetch(att.id).expect("cancel");
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Partial
        );

        core.fetch_attachment(att.id).await.expect("already here");
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Idle,
            "a `partial` mark must not outlive the bytes arriving"
        );
    }

    /// A vault with no sync session is answered rather than left waiting.
    ///
    /// The CLI opens exactly this vault for every subcommand but `sync`, and a
    /// Download that hung there would be indistinguishable from a slow relay.
    #[tokio::test]
    async fn fetching_without_a_sync_session_is_refused_rather_than_awaited() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let att = a_remote_attachment(&core, dir.path(), b"%PDF-1.7 a lease").await;

        assert!(matches!(
            core.fetch_attachment(att.id).await,
            Err(AttachError::SyncOffline { .. })
        ));
        assert_eq!(
            core.attachment_fetch_state(att.id).expect("state"),
            AttachmentFetchState::Idle,
            "a refused request must not leave a row claiming one is outstanding"
        );
    }

    /// An attachment written before `ciphertext_hash` existed cannot be named
    /// on the relay, so Download is a button that would never work and says so.
    #[tokio::test]
    async fn an_attachment_that_was_never_uploaded_cannot_be_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_vault(dir.path()).await;
        let att = a_remote_attachment(&core, dir.path(), b"%PDF-1.7 a lease").await;
        core.db()
            .conn()
            .execute(
                "UPDATE attachments SET ciphertext_hash = X'' WHERE id = ?",
                rusqlite::params![&att.id.bytes()[..]],
            )
            .expect("blank the hash, as a pre-0024 row reads back");

        assert!(matches!(
            core.fetch_attachment(att.id).await,
            Err(AttachError::NotOnRelay { .. })
        ));
    }
}
