//! Driving the sync engine from a client: the transport factory every client
//! dials with, the bounded "bring this vault level once" a backgrounded app
//! runs, and the wait for a replay or an outbox to catch up.
//!
//! `docs/07-clients/mobile-ios.md` §Background sync and §Push handling. iOS
//! hands an app a short, OS-chosen budget and suspends it again when the app
//! reports done or the budget ends. The long-lived driver
//! [`sunrise_core::Core::start_sync`] starts is the wrong shape for that on its
//! own: it never reports "done", and a session it holds from before the app was
//! suspended can look live while the socket under it is long dead.
//! [`sync_once`] is the bounded shape.
//!
//! # One driver, not two
//!
//! [`sync_once`] does not run a second sync engine beside the driver. It
//! starts the driver if nothing has, makes the driver open a **fresh**
//! session, and waits until that session reports what `sunrise sync --once`
//! waits for: `Live` with nothing left in the outbox. Two engines would race
//! over the same outbox and cursors; one driver has one of each.
//!
//! "Fresh" is what [`SyncLink`] is for. Every transport the driver dials is
//! wrapped so that a kick ends the session the wrapper belongs to, and every
//! dial is counted. [`sync_once`] reads the count, kicks, and accepts `Live`
//! only from a session dialled after the read — so a session that was `Live`
//! before the app was suspended cannot answer for one that has caught up now.
//!
//! # Expiry
//!
//! When the OS ends the budget, the client cancels the [`SyncCancel`] it
//! passed in, and [`sync_once`] returns at once with `completed == false`. A
//! caller that drops the future instead gets the same result.
//!
//! Nothing here holds a transaction: the wait only reads status, and the
//! driver applies each inbound op through `Core::apply_remote_all`, which
//! commits it whole or not at all while the database lock is held, never
//! across an await. Dropping the wait therefore leaves no half-applied batch,
//! and the next run picks up from the cursors the committed ops advanced — at
//! worst replaying an op the relay sent twice, which the op log absorbs as a
//! duplicate.
//!
//! # Time
//!
//! Every deadline here is either the core's injected clock
//! ([`Core::now_ms`]) or a tokio timer; nothing reads the host's clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    BoxTransport, ConnectFuture, Core, CoreError, CredentialRead, Query, QueryResult, SyncStatus,
    TransportFactory,
};
use sunrise_sync::{
    BlobCommit, DeviceSigner, RevokeOutcome, SseTransport, SyncState, Transport, TransportError,
};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::watch;

/// How often a wait re-reads the status when no update arrives. The status
/// broadcast is what normally wakes [`sync_once`]; this is the floor for the
/// one transition the broadcast does not carry — the outbox count, which
/// `Core::query` reads from the database.
pub const STATUS_POLL: Duration = Duration::from_millis(200);

/// The kick and the dial count every transport the driver dials shares.
///
/// Built once per open vault and cloned into each transport factory, so a
/// driver started on launch and the [`sync_once`] a background task runs later
/// see the same count and answer the same kick.
#[derive(Clone)]
pub struct SyncLink {
    /// Bumped to end every session dialled before the bump.
    kick: Arc<watch::Sender<u64>>,
    /// Transports dialled since the vault opened.
    dialled: Arc<AtomicU64>,
}

impl Default for SyncLink {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncLink {
    /// A link nothing has dialled through.
    #[must_use]
    pub fn new() -> Self {
        Self {
            kick: Arc::new(watch::Sender::new(0)),
            dialled: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Wrap one freshly dialled transport, and count it.
    #[must_use]
    pub fn wrap(&self, inner: BoxTransport) -> BoxTransport {
        self.dialled.fetch_add(1, Ordering::SeqCst);
        let kick = self.kick.subscribe();
        let born = *kick.borrow();
        Box::new(KickableTransport { inner, kick, born })
    }

    /// Transports dialled through this link so far.
    #[must_use]
    pub fn dialled(&self) -> u64 {
        self.dialled.load(Ordering::SeqCst)
    }

    /// End every session dialled before now. The next dial is unaffected.
    pub fn kick(&self) {
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
/// gives, so the driver reacts the way it reacts to any dropped link: tear the
/// session down and dial again on its first backoff step. Nothing about the
/// driver changes for this to work, which is the point — the kick rides a path
/// every disconnection already exercises.
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

/// Build the factory the sync driver dials with, once per connection attempt
/// (initial connect and every reconnect): the real [`SseTransport`] to `url`,
/// Server-Sent Events downstream and typed `POST` upstream (ADR-0023).
///
/// The factory holds no bearer: the driver reads the core's sync credential on
/// every attempt and hands the read over, so a reconnect after a renewal
/// presents the *current* token, and the driver knows which version each
/// attempt carried. `signer` — the ADR-0022 device binding — is cloned per
/// attempt for the same reason: a reconnect is a new transport and has to
/// carry the binding too, and the route where that matters most is the
/// revocation `DELETE`, which runs on whatever connection the driver holds at
/// the time.
///
/// With a `link`, every transport goes out through it, which is what lets
/// [`sync_once`] count and end them.
#[must_use]
pub fn sse_factory(
    url: &str,
    signer: Option<Arc<dyn DeviceSigner>>,
    link: Option<SyncLink>,
) -> TransportFactory {
    let url = url.to_string();
    Arc::new(move |read: CredentialRead| {
        let url = url.clone();
        let signer = signer.clone();
        let link = link.clone();
        Box::pin(async move {
            let t = SseTransport::connect_with_bearer(&url, read.bearer());
            let t = match signer {
                Some(s) => t.with_device_signer(s),
                None => t,
            };
            let t: BoxTransport = Box::new(t);
            Ok(match link {
                Some(link) => link.wrap(t),
                None => t,
            })
        }) as ConnectFuture
    })
}

/// A client's way to end a [`sync_once`] early.
///
/// One-way and sticky: a handle cancelled before [`sync_once`] is called makes
/// that call return at once, and a cancelled handle stays cancelled.
#[derive(Debug)]
pub struct SyncCancel {
    cancelled: watch::Sender<bool>,
}

impl Default for SyncCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncCancel {
    /// A handle nobody has cancelled yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: watch::Sender::new(false),
        }
    }

    /// End the [`sync_once`] this handle was passed to, or the next one.
    /// Idempotent.
    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
    }

    /// Whether [`Self::cancel`] has been called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Resolves once the handle is cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.cancelled.subscribe();
        // The sender lives as long as `self`, so the wait cannot fail; if it
        // somehow did, treating it as a cancel ends the call rather than
        // stranding it.
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// What one [`sync_once`] achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOnceReport {
    /// A session dialled during this call caught up on every stream and
    /// emptied the outbox. `false` when the budget ran out first, or the
    /// relay refused in a way reconnecting will not fix.
    pub completed: bool,
    /// Changes the vault published while this call ran, inbound and local
    /// alike.
    pub changes: u64,
    /// Local ops still waiting for the relay's ack when the call returned.
    pub outbox_pending: u32,
    /// The driver's state when the call returned.
    pub state: SyncState,
}

/// Why the sync status could not be read.
#[derive(Debug, thiserror::Error)]
pub enum SyncStatusError {
    /// The core refused the read: closed, chiefly.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The status query answered with something else.
    #[error("SyncStatus answered with {0}")]
    Unexpected(String),
}

/// The driver's status now, outbox count included.
///
/// # Errors
/// [`SyncStatusError`].
pub async fn sync_status_now(core: &Core) -> Result<SyncStatus, SyncStatusError> {
    match core.query(Query::SyncStatus).await? {
        QueryResult::SyncStatus(status) => Ok(status),
        other => Err(SyncStatusError::Unexpected(format!("{other:?}"))),
    }
}

/// Whether a status settles a [`sync_once`]: `Some(true)` caught up,
/// `Some(false)` will not catch up in this call, `None` keep waiting.
///
/// Only a session dialled after the call began may settle it. Before that,
/// `Live` is the session the app was suspended with, and `Stopped` or
/// `Degraded` are what an earlier session left behind — the fresh credential
/// the call just wrote may already be about to clear them.
#[must_use]
pub fn settled(status: &SyncStatus, fresh: bool) -> Option<bool> {
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

/// Bring `core` level with the relay once, within `budget_ms`, and report
/// what happened.
///
/// `start` starts the driver if nothing has and hands it the current bearer;
/// it must dial through `link`. Then every session dialled before the call is
/// ended, and this waits for a session dialled after it to report `Live` with
/// an empty outbox. Bounded by `budget_ms`, and idempotent: a second call with
/// nothing new on either side completes with `changes == 0`. The driver keeps
/// running when this returns.
///
/// Cancelling `cancel` ends the wait at once with `completed == false`, and
/// nothing else. A handle already cancelled when the call begins starts
/// nothing.
///
/// # Errors
/// What `start` and a status read raise. An unreachable relay is not an
/// error: it is `completed == false` with the state the driver reached.
pub async fn sync_once<E: From<SyncStatusError>>(
    core: &Core,
    link: &SyncLink,
    budget_ms: u64,
    cancel: &SyncCancel,
    start: impl FnOnce() -> Result<(), E>,
) -> Result<SyncOnceReport, E> {
    // Subscribed before anything moves, so nothing the driver does from here
    // on is missed by either count.
    let mut changes = core.changes();
    let mut updates = core.sync_status();
    let dialled_before = link.dialled();

    let completed = if cancel.is_cancelled() {
        false
    } else {
        start()?;
        // Nothing dialled yet means no session from before this call exists:
        // the driver `start` may just have started is dialling the fresh one,
        // and kicking it would only cost a reconnect.
        if dialled_before > 0 {
            link.kick();
        }
        // A driver the app was suspended in the middle of a reconnect delay
        // would otherwise finish that delay — up to thirty seconds, more than
        // the whole budget — before dialling the fresh session.
        core.wake_sync();

        let wait = async {
            loop {
                // The dial count before the status, never after: a reconnect
                // landing between the two reads would otherwise let the kicked
                // session's stale `Live` pass as fresh.
                let fresh = link.dialled() > dialled_before;
                let status = sync_status_now(core).await?;
                if let Some(done) = settled(&status, fresh) {
                    return Ok::<bool, SyncStatusError>(done);
                }
                tokio::select! {
                    _ = updates.recv() => {}
                    () = tokio::time::sleep(STATUS_POLL) => {}
                }
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            done = tokio::time::timeout(Duration::from_millis(budget_ms), wait) => {
                done.unwrap_or(Ok(false))?
            }
        }
    };

    let mut changed = 0u64;
    loop {
        match changes.try_recv() {
            Ok(_) => changed = changed.saturating_add(1),
            // Dropped for falling behind is still a change that happened.
            Err(TryRecvError::Lagged(skipped)) => changed = changed.saturating_add(skipped),
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    let status = sync_status_now(core).await?;
    Ok(SyncOnceReport {
        completed,
        changes: changed,
        outbox_pending: status.outbox_pending,
        state: status.state,
    })
}

/// Wait, against the core's clock, until the driver already running reports
/// `Live` with an empty outbox, or `budget_ms` passes.
///
/// `on_status` hears every status read, before it is judged, so a caller can
/// report progress — an outbox draining, a replay applying — as it goes.
/// `Ok(Err(last))` is the budget running out, carrying the last status read.
///
/// What `sunrise sync --once` and a recovery's replay wait (§Recovery flow
/// step 8) both are. Unlike [`sync_once`] it accepts the session that is
/// already up: both callers started the driver themselves a moment before.
///
/// # Errors
/// A status read that failed.
pub async fn wait_until_caught_up(
    core: &Core,
    budget_ms: u64,
    on_status: &mut (dyn FnMut(&SyncStatus) + Send),
) -> Result<Result<(), SyncStatus>, SyncStatusError> {
    let deadline = core.now_ms().saturating_add(budget_ms);
    loop {
        let status = sync_status_now(core).await?;
        on_status(&status);
        if status.state == SyncState::Live && status.outbox_pending == 0 {
            return Ok(Ok(()));
        }
        if core.now_ms() >= deadline {
            return Ok(Err(status));
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The cancel is sticky: a wait that starts after it resolves at once,
    /// and one already waiting is woken.
    #[tokio::test]
    async fn a_cancel_ends_a_wait_before_or_after_it_starts() {
        let early = SyncCancel::new();
        early.cancel();
        tokio::time::timeout(Duration::from_secs(5), early.cancelled())
            .await
            .expect("a handle cancelled first resolves at once");

        let late = Arc::new(SyncCancel::new());
        assert!(!late.is_cancelled());
        let waiter = {
            let late = Arc::clone(&late);
            tokio::spawn(async move { late.cancelled().await })
        };
        late.cancel();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("a waiting handle is woken")
            .expect("the waiter ran");
        assert!(late.is_cancelled());
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
        let ended = tokio::time::timeout(Duration::from_secs(5), stale.recv_frame())
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
        let waited = tokio::time::timeout(Duration::from_millis(100), fresh.recv_frame()).await;
        assert!(
            waited.is_err(),
            "the fresh session keeps waiting on its stream"
        );
    }

    /// A vault with no driver running is never `Live`, so the wait runs out
    /// its budget against the core's clock and hands back the last status.
    #[tokio::test]
    async fn a_wait_with_no_driver_runs_out_and_reports_the_last_status() {
        #[derive(Debug)]
        struct Frozen;
        impl sunrise_core::Clock for Frozen {
            fn now_ms(&self) -> u64 {
                1_767_225_600_000
            }
            fn timezone(&self) -> String {
                "UTC".to_owned()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(
            sunrise_core::CoreConfig::with_clock(
                dir.path().to_path_buf(),
                "test",
                Arc::new(Frozen),
                Arc::new(sunrise_core::SystemRng),
            ),
            sunrise_core::Unlock::DevicePaired {
                root: sunrise_crypto::keys::VaultRootKey::from_bytes([4; 32]),
                paired: None,
            },
        )
        .await
        .unwrap();
        let mut seen = 0;
        let last = wait_until_caught_up(&core, 0, &mut |_| seen += 1)
            .await
            .unwrap()
            .expect_err("nothing is syncing");
        assert_eq!(seen, 1, "a zero budget reads once and stops");
        assert_ne!(last.state, SyncState::Live);
        core.shutdown().await;
    }
}
