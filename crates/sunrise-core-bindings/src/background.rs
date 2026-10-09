//! What a mobile client runs when the OS wakes it in the background:
//! one bounded sync, and the push-token upload that makes the wake possible.
//!
//! `docs/07-clients/mobile-ios.md` §Background sync and §Push handling. iOS
//! hands an app a short, OS-chosen budget — an app refresh task or a silent
//! push — and suspends it again when the app reports done or the budget ends.
//! The long-lived driver [`SunriseCore::start_sync`] starts is the wrong shape
//! for that on its own: it never reports "done", and a session it holds from
//! before the app was suspended can look live while the socket under it is
//! long dead. [`SunriseCore::sync_once`] is the bounded shape.
//!
//! # One driver, not two
//!
//! `sync_once` does not run a second sync engine beside the driver. It starts
//! the driver if nothing has, makes the driver open a **fresh** session, and
//! waits until that session reports what `sunrise sync --once` waits for:
//! `Live` with nothing left in the outbox. Two engines would race over the
//! same outbox and cursors; one driver has one of each.
//!
//! "Fresh" is what the crate-private `SyncLink` is for. Every transport the driver dials is
//! wrapped so that a kick ends the session the wrapper belongs to, and every
//! dial is counted. `sync_once` reads the count, kicks, and accepts `Live`
//! only from a session dialled after the read — so a session that was `Live`
//! before the app was suspended cannot answer for one that has caught up now.
//!
//! # Expiry
//!
//! When the OS ends the budget, the foreign side cancels its task and UniFFI
//! drops this future. Nothing here holds a transaction: the wait only reads
//! status, and the driver applies each inbound op through
//! `Core::apply_remote_all`, which commits it whole or not at all while the
//! database lock is held, never across an await. Dropping the wait therefore
//! leaves no half-applied batch, and the next run picks up from the cursors
//! the committed ops advanced — at worst replaying an op the relay sent twice,
//! which the op log absorbs as a duplicate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{BoxTransport, Query, QueryResult, SyncStatus};
use sunrise_sync::{BlobCommit, RevokeOutcome, SyncState, Transport, TransportError};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::watch;

use crate::{BindingError, SunriseCore};

/// How often [`SunriseCore::sync_once`] re-reads the status when no update
/// arrives. The status broadcast is what normally wakes it; this is the floor
/// for the one transition the broadcast does not carry — the outbox count,
/// which `Core::query` reads from the database.
const STATUS_POLL: Duration = Duration::from_millis(200);

/// The kick and the dial count every transport the driver dials shares.
///
/// Built once per open vault and cloned into each transport factory, so a
/// driver started by [`SunriseCore::start_sync`] on launch and the
/// [`SunriseCore::sync_once`] a background task runs later see the same
/// count and answer the same kick.
#[derive(Clone)]
pub(crate) struct SyncLink {
    /// Bumped to end every session dialled before the bump.
    kick: Arc<watch::Sender<u64>>,
    /// Transports dialled since the vault opened.
    dialled: Arc<AtomicU64>,
}

impl SyncLink {
    pub(crate) fn new() -> Self {
        Self {
            kick: Arc::new(watch::Sender::new(0)),
            dialled: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Wrap one freshly dialled transport, and count it.
    pub(crate) fn wrap(&self, inner: BoxTransport) -> BoxTransport {
        self.dialled.fetch_add(1, Ordering::SeqCst);
        let kick = self.kick.subscribe();
        let born = *kick.borrow();
        Box::new(KickableTransport { inner, kick, born })
    }

    fn dialled(&self) -> u64 {
        self.dialled.load(Ordering::SeqCst)
    }

    /// End every session dialled before now. The next dial is unaffected.
    fn kick(&self) {
        self.kick
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}

impl std::fmt::Debug for SyncLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncLink")
            .field("epoch", &*self.kick.borrow())
            .field("dialled", &self.dialled())
            .finish()
    }
}

/// A transport that reports its stream ended once it is kicked.
///
/// "Ended" is `Ok(None)`, the same answer a relay that closed the stream
/// gives, so the driver reacts the way it reacts to any dropped link:
/// tear the session down and dial again on its first backoff step. Nothing
/// about the driver changes for this to work, which is the point — the kick
/// rides a path every disconnection already exercises.
struct KickableTransport {
    inner: BoxTransport,
    kick: watch::Receiver<u64>,
    /// The epoch when this transport was dialled. A kick is any epoch past it.
    born: u64,
}

#[async_trait::async_trait]
impl Transport for KickableTransport {
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let Self { inner, kick, born } = self;
        let born = *born;
        let kicked = async {
            // A sender that is gone can never kick, so a closed channel waits
            // forever rather than ending the session.
            if kick.wait_for(|epoch| *epoch != born).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = kicked => Ok(None),
            frame = inner.recv_frame() => frame,
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.inner.close().await
    }

    async fn revoke_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        self.inner.revoke_device(device_id).await
    }

    async fn blob_init(
        &mut self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        self.inner
            .blob_init(stream_id, chunk_count, size_bytes)
            .await
    }

    async fn blob_put_chunk(
        &mut self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        self.inner.blob_put_chunk(upload_id, chunk_idx, bytes).await
    }

    async fn blob_finalize(
        &mut self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        self.inner
            .blob_finalize(upload_id, ciphertext_hash, chunk_hashes)
            .await
    }

    async fn blob_fetch(&mut self, blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.blob_fetch(blob_id).await
    }
}

/// What one [`SunriseCore::sync_once`] achieved.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SyncOnceOutcome {
    /// A session dialled during this call caught up on every stream and
    /// emptied the outbox. `false` when the budget ran out first, or the
    /// relay refused in a way reconnecting will not fix.
    pub completed: bool,
    /// Changes the vault published while this call ran, inbound and local
    /// alike. Non-zero is what an iOS fetch handler reports as `.newData`.
    pub changes: u64,
    /// Local ops still waiting for the relay's ack when the call returned.
    pub outbox_pending: u32,
    /// The driver's state when the call returned.
    pub state: SyncState,
}

/// Whether a status settles a `sync_once`: `Some(true)` caught up,
/// `Some(false)` will not catch up in this call, `None` keep waiting.
///
/// Only a session dialled after the call began may settle it. Before that,
/// `Live` is the session the app was suspended with, and `Stopped` or
/// `Degraded` are what an earlier session left behind — the fresh credential
/// `sync_once` just wrote may already be about to clear them.
fn settled(status: &SyncStatus, fresh: bool) -> Option<bool> {
    if !fresh {
        return None;
    }
    match status.state {
        SyncState::Live if status.outbox_pending == 0 => Some(true),
        // A terminal refusal parks the driver until the credential changes,
        // and a cursor gap latches for the life of the session: neither ends
        // inside this budget, so waiting it out only spends the OS's time.
        SyncState::Stopped | SyncState::Degraded => Some(false),
        _ => None,
    }
}

impl SunriseCore {
    async fn sync_status_now(&self) -> Result<SyncStatus, BindingError> {
        match self.inner.query(Query::SyncStatus).await? {
            QueryResult::SyncStatus(status) => Ok(status),
            other => Err(BindingError::Core(format!(
                "SyncStatus answered with {other:?}"
            ))),
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Bring this vault level with the relay once, within `budget_ms`, and
    /// report what happened.
    ///
    /// The background-sync entry point (`docs/07-clients/mobile-ios.md`
    /// §Background sync, §Push handling). `url`, `bearer` and
    /// `relay_device_id` mean what they mean to [`Self::start_sync`], which
    /// this calls first: the driver is started if nothing has, and the bearer
    /// is replaced either way, so a token renewed just before the call is the
    /// one presented.
    ///
    /// Then every session dialled before the call is ended, and this waits for
    /// a session dialled after it to report `Live` with an empty outbox: every
    /// subscribed stream caught up, and every local op acked. A session from
    /// before the app was suspended does not count, however live it looks.
    ///
    /// Bounded by `budget_ms`, and idempotent: a second call with nothing new
    /// on either side completes with `changes == 0`. The driver keeps running
    /// when this returns, exactly as [`Self::start_sync`] left it; a
    /// suspended process simply stops scheduling it.
    ///
    /// Cancelling the call — what an expired iOS task does — stops the wait
    /// and nothing else. No batch is half applied: see the module docs.
    ///
    /// # Errors
    ///
    /// Only what [`Self::start_sync`] and a status read can raise — a closed
    /// vault, chiefly. An unreachable relay is not an error: it is
    /// `completed == false` with the state the driver reached.
    pub async fn sync_once(
        &self,
        url: String,
        bearer: Option<String>,
        relay_device_id: Option<String>,
        budget_ms: u64,
    ) -> Result<SyncOnceOutcome, BindingError> {
        // Subscribed before anything moves, so nothing the driver does from
        // here on is missed by either count.
        let mut changes = self.inner.changes();
        let mut updates = self.inner.sync_status();
        let dialled_before = self.sync_link.dialled();

        self.start_sync(url, bearer, relay_device_id)?;
        // Nothing dialled yet means no session from before this call exists:
        // the driver this call may just have started is dialling the fresh
        // one, and kicking it would only cost a reconnect.
        if dialled_before > 0 {
            self.sync_link.kick();
        }

        let wait = async {
            loop {
                let status = self.sync_status_now().await?;
                let fresh = self.sync_link.dialled() > dialled_before;
                if let Some(done) = settled(&status, fresh) {
                    return Ok::<bool, BindingError>(done);
                }
                tokio::select! {
                    _ = updates.recv() => {}
                    () = tokio::time::sleep(STATUS_POLL) => {}
                }
            }
        };
        let completed = tokio::time::timeout(Duration::from_millis(budget_ms), wait)
            .await
            .unwrap_or(Ok(false))?;

        let mut changed = 0u64;
        loop {
            match changes.try_recv() {
                Ok(_) => changed = changed.saturating_add(1),
                // Dropped for falling behind is still a change that happened.
                Err(TryRecvError::Lagged(skipped)) => changed = changed.saturating_add(skipped),
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let status = self.sync_status_now().await?;
        Ok(SyncOnceOutcome {
            completed,
            changes: changed,
            outbox_pending: status.outbox_pending,
            state: status.state,
        })
    }

    /// File this device's APNs token with the relay at `relay_url`, so the
    /// relay can wake it with a content-less push
    /// (`docs/06-server/push-notifications.md`).
    ///
    /// `relay_device_id` is the id [`Self::register_relay_device`] or
    /// [`Self::bootstrap_account`] returned: the request is signed as that
    /// device, and the relay refuses to file a token under a device the
    /// caller's account does not own. `token` is the APNs device token as
    /// lowercase hex.
    ///
    /// Idempotent at the relay — the row is keyed by device, so a repeat
    /// replaces it with itself and a rotated token replaces the old one.
    ///
    /// # Errors
    ///
    /// [`BindingError::Relay`] with what the relay or the request builder
    /// said.
    pub async fn register_push_token(
        &self,
        relay_url: String,
        bearer: String,
        relay_device_id: String,
        token: String,
    ) -> Result<(), BindingError> {
        let core = Arc::clone(&self.inner);
        sunrise_relay_client::register_push_token(
            relay_url.trim(),
            &bearer,
            sunrise_relay_client::PushTokenRegistration {
                relay_device_id,
                platform: sunrise_relay_client::api::types::PushPlatform::Apns,
                token,
            },
            self.inner.now_ms(),
            move |message| core.sign_with_device_key(message),
        )
        .await
        .map_err(|e| BindingError::Relay(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{settled, SyncLink};
    use sunrise_core::SyncStatus;
    use sunrise_sync::{SyncState, Transport, TransportError};

    fn status(state: SyncState, outbox_pending: u32) -> SyncStatus {
        SyncStatus {
            state,
            outbox_pending,
            peer_devices: 0,
            last_sync_ms: None,
        }
    }

    /// Nothing settles before a fresh session exists: `Live` there is the
    /// session the app was suspended with.
    #[test]
    fn only_a_fresh_session_settles_the_wait() {
        for state in [
            SyncState::Live,
            SyncState::Stopped,
            SyncState::Degraded,
            SyncState::Disconnected,
            SyncState::CatchingUp,
        ] {
            assert_eq!(settled(&status(state, 0), false), None, "{state:?}");
        }
    }

    #[test]
    fn live_with_an_empty_outbox_is_caught_up() {
        assert_eq!(settled(&status(SyncState::Live, 0), true), Some(true));
        assert_eq!(
            settled(&status(SyncState::Live, 3), true),
            None,
            "an op still unacked keeps the wait open"
        );
        assert_eq!(settled(&status(SyncState::CatchingUp, 0), true), None);
        assert_eq!(settled(&status(SyncState::Disconnected, 0), true), None);
    }

    #[test]
    fn a_refusal_or_a_gap_ends_the_wait_without_completing() {
        assert_eq!(settled(&status(SyncState::Stopped, 0), true), Some(false));
        assert_eq!(settled(&status(SyncState::Degraded, 0), true), Some(false));
    }

    /// A transport whose stream never yields: what a socket that died while
    /// the app was suspended looks like from here.
    struct Silent;

    #[async_trait::async_trait]
    impl Transport for Silent {
        async fn send_frame(&mut self, _frame: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            std::future::pending().await
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    /// The kick is what rescues a session stuck on a dead stream: the wrapped
    /// transport reports the stream ended, which the driver answers by
    /// dialling again.
    #[tokio::test]
    async fn a_kick_ends_a_session_dialled_before_it() {
        let link = SyncLink::new();
        let mut stale = link.wrap(Box::new(Silent));
        assert_eq!(link.dialled(), 1);
        link.kick();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), stale.recv_frame())
            .await
            .expect("a kicked transport answers at once");
        assert!(matches!(ended, Ok(None)), "{ended:?}");
    }

    /// A transport dialled after the kick is the fresh session the kick asked
    /// for, and must not be ended by it.
    #[tokio::test]
    async fn a_kick_spares_a_session_dialled_after_it() {
        let link = SyncLink::new();
        link.kick();
        let mut fresh = link.wrap(Box::new(Silent));
        assert_eq!(link.dialled(), 1);
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(100), fresh.recv_frame()).await;
        assert!(
            waited.is_err(),
            "the fresh session keeps waiting on its stream"
        );
    }
}
