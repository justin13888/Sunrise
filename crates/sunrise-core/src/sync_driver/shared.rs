//! The driver's live state, shared with `Core` across the task boundary.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};

use crate::events::SyncStatus;
use sunrise_sync::SyncState;

#[derive(Debug)]
struct SharedState {
    state: SyncState,
    outbox_pending: u64,
    last_sync_ms: Option<u64>,
    /// Distinct originating device ids observed in inbound ops this session.
    peers: HashSet<[u8; 16]>,
    /// How far this session's replay has got, and how far it has to go.
    replay: ReplayLedger,
}

/// A session's catch-up, counted in ops (#459).
///
/// The relay states, before it replays a stream, how many ops that replay
/// will deliver (`BacklogPayload`). Each statement replaces the last for its
/// stream, because a re-`Subscribe` reopens the stream and the relay states
/// again from the new cursors; each replayed `OpBatch` counts down its
/// stream's share; `CaughtUp` zeroes it. A relay that predates the statement
/// leaves `remaining` empty, which is "no total", not "nothing left".
#[derive(Debug, Default)]
struct ReplayLedger {
    /// Ops the relay has said it will still send, per stream it has stated.
    remaining: HashMap<[u8; 16], u64>,
    /// Ops received while the session was catching up.
    applied: u64,
}

/// Live, shared sync state. Read by `Core::query(SyncStatus)`; written by the
/// driver, which broadcasts a fresh [`SyncStatus`] snapshot on every change.
#[derive(Debug)]
pub(crate) struct SyncShared {
    inner: Mutex<SharedState>,
    sync_tx: broadcast::Sender<SyncStatus>,
    /// Poked by `Core::submit` (when sync is active) so the driver drains the
    /// outbox immediately instead of waiting for the next inbound frame.
    submit: Notify,
    /// Poked by `Core::wake_sync` so a driver waiting out a reconnect delay
    /// dials now. `notify_one`, so a wake that lands just before the driver
    /// starts waiting is kept rather than lost.
    pub(super) reconnect: Notify,
    shutdown_notify: Notify,
    shutdown: AtomicBool,
    active: AtomicBool,
}

impl SyncShared {
    pub(crate) fn new(sync_tx: broadcast::Sender<SyncStatus>, pending: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SharedState {
                state: SyncState::Disconnected,
                outbox_pending: pending,
                last_sync_ms: None,
                peers: HashSet::new(),
                replay: ReplayLedger::default(),
            }),
            sync_tx,
            submit: Notify::new(),
            reconnect: Notify::new(),
            shutdown_notify: Notify::new(),
            shutdown: AtomicBool::new(false),
            active: AtomicBool::new(false),
        })
    }

    fn status_of(s: &SharedState) -> SyncStatus {
        SyncStatus {
            state: s.state,
            outbox_pending: u32::try_from(s.outbox_pending).unwrap_or(u32::MAX),
            peer_devices: u32::try_from(s.peers.len()).unwrap_or(u32::MAX),
            last_sync_ms: s.last_sync_ms,
            replay_applied: s.replay.applied,
            replay_remaining: Self::remaining_of(s),
        }
    }

    /// What the relay has stated is still to come this session: zero once
    /// every stated stream has caught up, and `None` before any statement,
    /// which is all a relay too old to state one ever leaves.
    ///
    /// Not cleared at `Live`. A poller can miss the whole catch-up between two
    /// reads, and the snapshot after it must still say the total was known.
    fn remaining_of(s: &SharedState) -> Option<u64> {
        if s.replay.remaining.is_empty() {
            return None;
        }
        Some(s.replay.remaining.values().sum())
    }

    /// A fresh session: nothing applied, nothing stated.
    pub(super) fn reset_replay(&self) {
        self.inner.lock().replay = ReplayLedger::default();
    }

    /// The relay stated that `stream`'s replay will deliver `ops`.
    pub(super) fn note_backlog(&self, stream: [u8; 16], ops: u64) {
        self.inner.lock().replay.remaining.insert(stream, ops);
    }

    /// `ops` arrived on `stream`.
    pub(super) fn note_replayed(&self, stream: [u8; 16], ops: u64) {
        let mut s = self.inner.lock();
        if s.state == SyncState::CatchingUp {
            s.replay.applied = s.replay.applied.saturating_add(ops);
        }
        if let Some(left) = s.replay.remaining.get_mut(&stream) {
            *left = left.saturating_sub(ops);
        }
    }

    /// `stream`'s replay is complete, whatever was stated for it.
    pub(super) fn note_stream_caught_up(&self, stream: [u8; 16]) {
        if let Some(left) = self.inner.lock().replay.remaining.get_mut(&stream) {
            *left = 0;
        }
    }

    /// `(replay_applied, replay_remaining)`, served to `Core::query(SyncStatus)`
    /// beside [`Self::status_fields`].
    pub(crate) fn replay_fields(&self) -> (u64, Option<u64>) {
        let s = self.inner.lock();
        (s.replay.applied, Self::remaining_of(&s))
    }

    pub(crate) fn set_state(&self, new: SyncState) {
        let mut s = self.inner.lock();
        if s.state == new {
            return;
        }
        s.state = new;
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    /// Latch [`SyncState::Degraded`]: the relay has reported ops it can no
    /// longer supply.
    ///
    /// Deliberately *not* cleared within the session — nothing that happens on
    /// this connection can refill the hole, so any later `Live` would be a
    /// lie. It does clear on reconnect, because the next `Subscribe` re-asks
    /// the question: if the ops are still unavailable the relay reports the
    /// gap again and this re-latches, and if a durable log has since made them
    /// servable the state was correctly transient.
    pub(crate) fn mark_degraded(&self) {
        let mut s = self.inner.lock();
        if s.state == SyncState::Degraded {
            return;
        }
        s.state = SyncState::Degraded;
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    pub(crate) fn set_pending(&self, n: u64) {
        let mut s = self.inner.lock();
        if s.outbox_pending == n {
            return;
        }
        s.outbox_pending = n;
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    pub(super) fn note_peer(&self, device_id: [u8; 16]) {
        let mut s = self.inner.lock();
        if !s.peers.insert(device_id) {
            return;
        }
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    pub(super) fn mark_synced(&self, ms: u64) {
        let mut s = self.inner.lock();
        s.last_sync_ms = Some(ms);
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    /// Fields served to `Core::query(SyncStatus)` alongside the DB-authoritative
    /// outbox count: `(state, last_sync_ms, peer_devices)`.
    pub(crate) fn status_fields(&self) -> (SyncState, Option<u64>, u32) {
        let s = self.inner.lock();
        (
            s.state,
            s.last_sync_ms,
            u32::try_from(s.peers.len()).unwrap_or(u32::MAX),
        )
    }

    pub(crate) fn current_state(&self) -> SyncState {
        self.inner.lock().state
    }

    pub(crate) fn mark_active(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) fn poke_submit(&self) {
        self.submit.notify_one();
    }

    pub(super) async fn submit_notified(&self) {
        self.submit.notified().await;
    }

    /// End the reconnect delay the driver is waiting out, or the next one it
    /// starts. One permit at most, however many wakes land: each skips one
    /// delay, never the schedule.
    pub(crate) fn wake_reconnect(&self) {
        self.reconnect.notify_one();
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
    }

    pub(super) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Resolves when shutdown has been requested. Lost-wakeup-safe: re-checks
    /// the flag after arming the notification.
    pub(crate) async fn shutdown_notified(&self) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let fut = self.shutdown_notify.notified();
            tokio::pin!(fut);
            fut.as_mut().enable();
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            fut.await;
        }
    }
}
