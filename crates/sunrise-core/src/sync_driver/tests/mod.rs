//! Driver tests against an in-process fake relay over a channel transport
//! (no sockets). Each `on_connect`/`inbound`/`duplicate`/`reconnect`/
//! `submit_while_live` test drives the real driver task; the fake server
//! scripts the protocol side.
//!
//! Every test that waits on the driver runs on paused time
//! (`start_paused`). Nothing here leaves the runtime: the core's storage
//! runs inline and the relay is a channel, so the clock only moves once
//! every task is parked. A deadline is then virtual and cannot expire
//! because the host is slow, and an absence asserted after a sleep is a
//! fact about the run — everything the driver could do before that instant
//! has been done — rather than a bet that it would have happened by then.
//!
//! This file holds what the suites share: the channel transport, the fake
//! server, the second-device builder and the assertion helpers. Each suite is a
//! child module, so it reads all of it through `use super::*`.

mod blob_uploads;
mod credential;
mod frames;
mod relay_revocations;
mod replay_ledger;
mod scheduling;
mod session;
mod session_end;
mod snapshot_recovery;

use super::{
    mark_renewals_current, note_marked_at_connect, run, BoxTransport, ClosePayload, ConnectFuture,
    CredentialRead, SessionEnd, SyncConfig, SyncShared, TokenSource, TransportFactory,
};
use crate::config::{Clock, Rng};
use crate::{Command, Core, CoreConfig, DomainEvent, Query, QueryResult, SystemRng, Unlock};
use async_trait::async_trait;
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::TaskDraft;
use sunrise_error::ErrorCode;
use sunrise_sync::{RevokeOutcome, SyncState, Transport, TransportError};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, Capability, CapabilityBits, CaughtUpPayload,
    ErrorPayload, FrameFlags, HelloAck, MsgKind, OpBatchPayload, SubscribePayload,
};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;

const ROOT: [u8; 32] = [0x5a; 32];
const T0: u64 = 1_700_000_000_000;
/// The event whose placement this change is about.
const MARKED_AT_CONNECT: &str = "sync.credential.marked_at_connect";

#[derive(Debug)]
struct TestClock(u64);
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

fn make_cfg(dir: &Path) -> CoreConfig {
    CoreConfig {
        sync: Some(SyncConfig::new("ws://unused/sync")),
        ..CoreConfig::with_clock(
            dir.to_path_buf(),
            "0.1.0+test",
            Arc::new(TestClock(T0)),
            Arc::new(SystemRng),
        )
    }
}

async fn open_arc(dir: &Path) -> Arc<Core> {
    Arc::new(
        Core::open(
            make_cfg(dir),
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    )
}

/// Open a core whose anti-entropy timer fires on a test timescale rather
/// than [`DEFAULT_RESYNC_INTERVAL`].
async fn open_arc_with_resync(dir: &Path, interval: Duration) -> Arc<Core> {
    let mut cfg = make_cfg(dir);
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_resync_interval(interval));
    Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    )
}

// ---- channel-backed duplex transport ----

struct ChannelTransport {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// How long an `OpBatch` send holds the caller after the frame is
    /// delivered. Zero is an ordinary channel. Non-zero is the SSE
    /// transport's shape: the send is a `POST` whose reply is the ack, so
    /// the ack is already queued by the time the send returns.
    batch_send_hold: Duration,
    /// Whether this transport has the snapshot routes, and the record its
    /// relay pins, if any (ADR-0068).
    serves_snapshots: bool,
    pinned: Option<Vec<u8>>,
    /// The blob id the pin names, when it is not the record's own content
    /// address: a relay whose pin and blob disagree. `None` derives it
    /// from `pinned`, as an honest relay's does.
    pinned_blob_id: Option<[u8; 16]>,
}

#[async_trait]
impl Transport for ChannelTransport {
    async fn snapshot_latest(
        &mut self,
        _stream_id: &[u8; 16],
    ) -> Result<Option<sunrise_sync::SnapshotPin>, TransportError> {
        if !self.serves_snapshots {
            return Err(TransportError::Unsupported);
        }
        Ok(self.pinned.as_ref().map(|r| sunrise_sync::SnapshotPin {
            blob_id: self
                .pinned_blob_id
                .unwrap_or_else(|| crate::snapshot_sync::blob_id_of(r)),
            size_bytes: r.len() as u64,
        }))
    }
    async fn blob_fetch(&mut self, _blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        if !self.serves_snapshots {
            return Err(TransportError::Unsupported);
        }
        Ok(self.pinned.clone())
    }
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        let is_batch = !self.batch_send_hold.is_zero()
            && decode_frame(&frame).is_ok_and(|(h, _)| h.msg_kind == MsgKind::OpBatch);
        self.tx
            .send(frame)
            .map_err(|_| TransportError::Unavailable("peer gone".into()))?;
        if is_batch {
            tokio::time::sleep(self.batch_send_hold).await;
        }
        Ok(())
    }
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(self.rx.recv().await)
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        self.rx.close();
        Ok(())
    }
}

fn duplex() -> (ChannelTransport, ChannelTransport) {
    let (a_tx, b_rx) = mpsc::unbounded_channel();
    let (b_tx, a_rx) = mpsc::unbounded_channel();
    (
        ChannelTransport {
            tx: a_tx,
            rx: a_rx,
            batch_send_hold: Duration::ZERO,
            serves_snapshots: false,
            pinned: None,
            pinned_blob_id: None,
        },
        ChannelTransport {
            tx: b_tx,
            rx: b_rx,
            batch_send_hold: Duration::ZERO,
            serves_snapshots: false,
            pinned: None,
            pinned_blob_id: None,
        },
    )
}

// ---- fake server ----

#[derive(Clone, Default)]
struct Script {
    /// Remote OpBatches to inject after the first Subscribe: `(stream, envelopes)`.
    inject: Vec<([u8; 16], Vec<Vec<u8>>)>,
    /// Simulate a transport drop right after the first Subscribe.
    close_after_subscribe: bool,
    /// Receive this many OpBatches without acking them — a lossy link that
    /// is not a broken one, which is precisely the case issue #20 names.
    swallow_first_batches: usize,
    /// Inject these OpBatches on the SECOND and later Subscribe frames, so
    /// a test can tell an in-session resync from the initial subscribe.
    inject_on_resync: Vec<([u8; 16], Vec<Vec<u8>>)>,
    /// Send a `SYNC_CURSOR_GAP` Error before `CaughtUp`, exactly as the
    /// relay does when a subscriber's cursor predates the retained ring.
    gap_on_subscribe: bool,
    /// Whether that gap names its stream, as the SSE transport's does, and
    /// the client transport has the snapshot routes (ADR-0068).
    snapshots: SnapshotRoutes,
    /// Hold every client `OpBatch` send this long after the relay has it,
    /// as a `POST` over a link with this round trip does.
    client_batch_send_hold: Duration,
}

/// The snapshot routes a fake relay serves.
#[derive(Clone, Default)]
enum SnapshotRoutes {
    /// None: a gap names no stream and the transport has no routes.
    #[default]
    Absent,
    /// The routes, pinning this record, if any.
    Pinned(Option<Vec<u8>>),
}

struct ServerInner {
    /// Attempts whose **factory was called**, not attempts that dialled.
    /// The closure counts and pops its script on entry, so a future `run`
    /// drops un-polled — shutdown winning the connect select — has still
    /// consumed a script and moved this. Every assertion on it is relative
    /// or `>=` for that reason.
    connect_count: u64,
    scripts: VecDeque<Script>,
    /// The bearer each of those attempts was handed, in order — what a
    /// real relay would have seen in its `Authorization` header.
    presented: Vec<Option<String>>,
}
type SharedServer = Arc<parking_lot::Mutex<ServerInner>>;

struct RecvBatch {
    ops: Vec<Vec<u8>>,
}

/// Bearer tokens the fake relay received in `0x12 RefreshToken` frames,
/// in order. Shared so a test can assert the renewal reached a *live*
/// session rather than the next reconnect.
type SeenRefreshes = Arc<parking_lot::Mutex<Vec<String>>>;

/// How many Subscribe frames one fake server has seen, across all its
/// connections. Shared so a test can assert a re-subscribe happened
/// *within* a session rather than via a reconnect.
type SubCount = Arc<std::sync::atomic::AtomicU64>;

fn harness(
    scripts: Vec<Script>,
) -> (
    TransportFactory,
    SharedServer,
    mpsc::UnboundedReceiver<RecvBatch>,
) {
    let (f, s, rx, _) = harness_counting(scripts);
    (f, s, rx)
}

fn harness_counting(
    scripts: Vec<Script>,
) -> (
    TransportFactory,
    SharedServer,
    mpsc::UnboundedReceiver<RecvBatch>,
    SubCount,
) {
    let (f, s, rx, subs, _) = harness_full(scripts, true);
    (f, s, rx, subs)
}

#[allow(clippy::type_complexity)]
fn harness_full(
    scripts: Vec<Script>,
    negotiate_refresh: bool,
) -> (
    TransportFactory,
    SharedServer,
    mpsc::UnboundedReceiver<RecvBatch>,
    SubCount,
    SeenRefreshes,
) {
    let server: SharedServer = Arc::new(parking_lot::Mutex::new(ServerInner {
        connect_count: 0,
        scripts: scripts.into_iter().collect(),
        presented: Vec::new(),
    }));
    let (batch_tx, batch_rx) = mpsc::unbounded_channel();
    let subs: SubCount = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let refreshes: SeenRefreshes = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let factory_server = server.clone();
    let factory_subs = subs.clone();
    let factory_refreshes = refreshes.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        let batch_tx = batch_tx.clone();
        let subs = factory_subs.clone();
        let refreshes = factory_refreshes.clone();
        // Counted in the closure's own body rather than in the future it
        // returns, so a test that sees `connect_count` move knows `run`
        // has already read this attempt's credential.
        let script = {
            let mut s = factory_server.lock();
            s.connect_count += 1;
            s.presented.push(read.bearer().map(ToOwned::to_owned));
            s.scripts.pop_front().unwrap_or_default()
        };
        let fut = async move {
            let (mut client_end, server_end) = duplex();
            client_end.batch_send_hold = script.client_batch_send_hold;
            if let SnapshotRoutes::Pinned(record) = &script.snapshots {
                client_end.serves_snapshots = true;
                client_end.pinned.clone_from(record);
            }
            tokio::spawn(run_fake_server(
                server_end,
                script,
                batch_tx,
                subs,
                refreshes,
                negotiate_refresh,
            ));
            Ok(Box::new(client_end) as BoxTransport)
        };
        Box::pin(fut) as ConnectFuture
    });
    (factory, server, batch_rx, subs, refreshes)
}

/// Push each `(stream, envelopes)` group to the client as one `OpBatch`.
/// Returns false once the client end is gone, so callers stop the server.
async fn send_op_batches(t: &mut ChannelTransport, groups: &[([u8; 16], Vec<Vec<u8>>)]) -> bool {
    for (stream_id, envs) in groups {
        let payload = OpBatchPayload {
            ops: envs.clone(),
            batch_id: 0,
            stream_id: *stream_id,
        };
        let bytes = payload.encode().unwrap();
        let f = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &bytes).unwrap();
        if t.send_frame(f).await.is_err() {
            return false;
        }
    }
    true
}

/// The fake server's Subscribe handling: optional resync injection, then a
/// `CaughtUp` per newly-subscribed stream. Returns false once the client is
/// gone.
async fn serve_subscribe(
    t: &mut ChannelTransport,
    script: &Script,
    payload: &[u8],
    subscribed: &mut HashSet<[u8; 16]>,
    sub_count: &SubCount,
    injected: &mut bool,
) -> bool {
    let n = sub_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Ok(sub) = SubscribePayload::decode(payload) {
        // Only on a re-subscribe, so a test can attribute the
        // delivery to an in-session resync and nothing else.
        if n > 0 && !send_op_batches(t, &script.inject_on_resync).await {
            return false;
        }
        for entry in &sub.streams {
            if subscribed.insert(entry.stream_id) {
                // Ordered exactly as the relay orders it: the
                // gap lands BEFORE CaughtUp, so a client that
                // ignores it goes on to read "caught up" as
                // "complete".
                if script.gap_on_subscribe {
                    let e = ErrorPayload {
                        code: ErrorCode::SyncCursorGap,
                        reason: "retained ring no longer covers stream".into(),
                        stream_id: matches!(script.snapshots, SnapshotRoutes::Pinned(_))
                            .then_some(entry.stream_id),
                    };
                    let bytes = e.encode().unwrap();
                    let f = encode_frame(MsgKind::Error, FrameFlags::EMPTY, &bytes).unwrap();
                    if t.send_frame(f).await.is_err() {
                        return false;
                    }
                }
                let cu = CaughtUpPayload {
                    stream_id: entry.stream_id,
                };
                let bytes = cu.encode().unwrap();
                let f = encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &bytes).unwrap();
                if t.send_frame(f).await.is_err() {
                    return false;
                }
            }
        }
    }
    if !*injected {
        *injected = true;
        if !send_op_batches(t, &script.inject).await {
            return false;
        }
        if script.close_after_subscribe {
            return false; // simulate a transport drop
        }
    }
    true
}

async fn run_fake_server(
    mut t: ChannelTransport,
    script: Script,
    batch_tx: mpsc::UnboundedSender<RecvBatch>,
    sub_count: SubCount,
    refreshes: SeenRefreshes,
    negotiate_refresh: bool,
) {
    // Expect Hello, reply HelloAck.
    let Ok(Some(frame)) = t.recv_frame().await else {
        return;
    };
    let Ok((h, _)) = decode_frame(&frame) else {
        return;
    };
    if h.msg_kind != MsgKind::Hello {
        return;
    }
    let ack = HelloAck {
        server_app_v: "fake".into(),
        wire_proto: 1,
        crypto_suite: 1,
        doc_schema_floor: 1,
        capabilities: if negotiate_refresh {
            CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0
        } else {
            0
        },
        server_time_ms: T0,
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&ack, &mut buf).unwrap();
    let ack_frame = encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &buf).unwrap();
    if t.send_frame(ack_frame).await.is_err() {
        return;
    }

    let mut subscribed: HashSet<[u8; 16]> = HashSet::new();
    let mut injected = false;
    let mut swallowed = 0usize;
    loop {
        let frame = match t.recv_frame().await {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => return,
        };
        let Ok((hh, payload)) = decode_frame(&frame) else {
            continue;
        };
        match hh.msg_kind {
            MsgKind::Subscribe => {
                if !serve_subscribe(
                    &mut t,
                    &script,
                    &payload,
                    &mut subscribed,
                    &sub_count,
                    &mut injected,
                )
                .await
                {
                    return;
                }
            }
            MsgKind::OpBatch => {
                if let Ok(batch) = OpBatchPayload::decode(&payload) {
                    let _ = batch_tx.send(RecvBatch {
                        ops: batch.ops.clone(),
                    });
                    if swallowed < script.swallow_first_batches {
                        swallowed += 1;
                        continue; // received, deliberately not acked
                    }
                    let ack = AckPayload {
                        batch_id: batch.batch_id,
                        stream_id: batch.stream_id,
                        server_first_seen_ms: T0,
                    };
                    let bytes = ack.encode().unwrap();
                    let f = encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &bytes).unwrap();
                    if t.send_frame(f).await.is_err() {
                        return;
                    }
                }
            }
            MsgKind::RefreshToken => {
                if let Ok(p) = sunrise_wire_protocol::RefreshTokenPayload::decode(&payload) {
                    refreshes.lock().push(p.token);
                }
            }
            MsgKind::Close => return,
            _ => {}
        }
    }
}

// ---- remote-op builder: a second Core (device B, same vault root) ----

/// Create `titles.len()` tasks on device B's inbox and return
/// `(B's cert, inbox stream id, sealed envelope per task in seq order)`.
/// A second device on the same account, reduced to what a test needs of
/// it: the pairing payload that makes the receiver a sibling, and the
/// vault-meta ops that announce it.
struct RemotePeer {
    /// Encoded `PairingPayload` — the identity and every Stream key.
    bundle: Vec<u8>,
    /// The peer's vault-meta ops, which carry its `device_cert` op.
    meta: ([u8; 16], Vec<Vec<u8>>),
}

/// Build a peer device's ops out of process, the way a real second device
/// would produce them.
///
/// Before ADR-0024 this returned a certificate and the receiver trusted it
/// with one command, because a shared vault root already implied a shared
/// key schedule. It does not any more: two vaults on one root are two
/// accounts. So the peer hands over a real pairing payload, and its cert
/// reaches the receiver the way it reaches every other replica — as a
/// `device_cert` op in the vault-meta stream, self-authenticating against
/// the account identity.
async fn make_remote_tasks(titles: &[&str]) -> (RemotePeer, [u8; 16], Vec<Vec<u8>>) {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(
        make_cfg(dir.path()),
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(ROOT),
            paired: None,
        },
    )
    .await
    .unwrap();
    for title in titles {
        core.submit(Command::CreateTask(TaskDraft {
            title: (*title).into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    }
    let bundle = sunrise_pairing::encode_pairing_payload(
        &core
            .pair_device_in_process("joiner".into(), "test".into(), [0x71; 32], [0x72; 32])
            .unwrap(),
    )
    .unwrap();
    let groups = core.sync_outbox_grouped(&HashSet::new()).unwrap();
    let mut meta: Option<([u8; 16], Vec<Vec<u8>>)> = None;
    let mut tasks: Option<([u8; 16], Vec<Vec<u8>>)> = None;
    for (stream, ops) in groups {
        let envs: Vec<Vec<u8>> = ops.into_iter().map(|(_, env)| env).collect();
        if stream == [0u8; 16] {
            meta = Some((stream, envs));
        } else {
            tasks = Some((stream, envs));
        }
    }
    let meta = meta.expect("the peer announced itself in the meta stream");
    let (stream, envs) = tasks.expect("all tasks land on the inbox stream");
    core.close().await.unwrap();
    drop(dir);
    (RemotePeer { bundle, meta }, stream, envs)
}

/// Ack this vault's opening announcement out of band.
///
/// A freshly opened vault is no longer empty. `Core::open` publishes this
/// device's identity-signed certificate and the identity-sealed copies of
/// the first Stream keys it mints, and those queue in the outbox like any
/// other op — which is the point: a peer that never receives them can
/// neither verify this device's envelopes nor recover its content.
///
/// The tests below are about the driver's handling of *one* op the test
/// submitted, so the announcement is acked directly rather than counted.
/// `paired_devices_converge` and `device_revocation` in `sunrise-e2e` are
/// where the announcement travelling for real is asserted.
fn drain_announcement(core: &Core) {
    let ids: Vec<[u8; 16]> = core
        .sync_outbox_grouped(&std::collections::HashSet::new())
        .unwrap()
        .into_iter()
        .flat_map(|(_, ops)| ops.into_iter().map(|(op_id, _)| op_id))
        .collect();
    core.sync_mark_acked(&ids).unwrap();
}

/// Open a core that is already a sibling of `peer`.
async fn open_arc_paired(dir: &Path, peer: &RemotePeer) -> Arc<Core> {
    open_paired(make_cfg(dir), peer).await
}

/// [`open_arc_paired`] with an anti-entropy timer on a test timescale.
async fn open_arc_paired_with_resync(
    dir: &Path,
    peer: &RemotePeer,
    interval: Duration,
) -> Arc<Core> {
    let mut cfg = make_cfg(dir);
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_resync_interval(interval));
    open_paired(cfg, peer).await
}

async fn open_paired(cfg: CoreConfig, peer: &RemotePeer) -> Arc<Core> {
    let payload = sunrise_pairing::decode_pairing_payload(&peer.bundle).unwrap();
    Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: Some(Box::new(payload)),
            },
        )
        .await
        .unwrap(),
    )
}

// ---- assertion helpers (event-driven, generous timeouts) ----

async fn collect_until_live(rx: &mut broadcast::Receiver<crate::SyncStatus>) -> Vec<SyncState> {
    let mut seen = Vec::new();
    timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(s) => {
                    if seen.last() != Some(&s.state) {
                        seen.push(s.state);
                    }
                    if s.state == SyncState::Live {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await
    .expect("timed out waiting for Live");
    seen
}

async fn wait_created(rx: &mut broadcast::Receiver<DomainEvent>) {
    timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(DomainEvent::Created(_)) => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("changes closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for Created");
}

async fn inbox_len(core: &Core) -> usize {
    match core.query(Query::Inbox).await.unwrap() {
        QueryResult::StreamTasks(v) => v.len(),
        other => panic!("expected StreamTasks, got {other:?}"),
    }
}

/// One captured event: the `ev` name it carried, and its `to_v` if it had
/// one.
///
/// `sunrise-core` captures no `tracing` output anywhere else, and
/// `sync.credential.marked_at_connect` is this change's whole
/// user-visible surface, so both branches of its trigger are asserted
/// against a real subscriber rather than argued. Following
/// `crates/sunrise-log/tests/redaction.rs`, the code under test runs
/// *under* a subscriber instead of a formatted line being matched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LoggedEvent {
    ev: Option<String>,
    to_v: Option<u64>,
    /// `sync.backoff`'s two fields, which
    /// [`an_unanswered_reconnect_climbs_the_backoff_schedule`] reads.
    attempt: Option<u64>,
    delay_ms: Option<u64>,
    /// `sync.session.closed` and `sync.session.error`'s two fields, which
    /// the session-end tests below read.
    err_code: Option<String>,
    result: Option<String>,
    /// `sync.session.error`'s `err_kind` and `retryable`, which must agree
    /// with the catalogue entry for its `err_code`.
    err_kind: Option<String>,
    retryable: Option<bool>,
}

impl tracing::field::Visit for LoggedEvent {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() == "retryable" {
            self.retryable = Some(value);
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "to_v" => self.to_v = Some(value),
            "attempt" => self.attempt = Some(value),
            "delay_ms" => self.delay_ms = Some(value),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "ev" => self.ev = Some(value.to_string()),
            "err_code" => self.err_code = Some(value.to_string()),
            "result" => self.result = Some(value.to_string()),
            "err_kind" => self.err_kind = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Clone, Default)]
struct EventLog(Arc<parking_lot::Mutex<Vec<LoggedEvent>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut logged = LoggedEvent::default();
        event.record(&mut logged);
        self.0.lock().push(logged);
    }
}

/// Every event `f` emitted, in order.
fn captured_events(f: impl FnOnce()) -> Vec<LoggedEvent> {
    use tracing_subscriber::layer::SubscriberExt as _;
    let log = EventLog::default();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(log.clone()), f);
    let events = log.0.lock().clone();
    events
}

/// Drain every state the core broadcast since `rx` subscribed, and fail
/// if one was `Live` or if the channel lagged past one unread.
///
/// The polls above sample the state; this reads every transition, so a
/// `Live` that came and went between two samples is caught too.
fn assert_never_live(rx: &mut broadcast::Receiver<crate::SyncStatus>) {
    let mut seen = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(s) => seen.push(s.state),
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(e) => panic!("the status channel {e:?}, so a state may have gone unread"),
        }
    }
    assert!(
        seen.contains(&SyncState::Degraded),
        "the gap was broadcast, saw {seen:?}"
    );
    assert!(
        !seen.contains(&SyncState::Live),
        "a gap the relay cannot fill must never be broadcast as Live, saw {seen:?}"
    );
}

async fn sync_state(core: &Core) -> SyncState {
    match core.query(Query::SyncStatus).await.unwrap() {
        QueryResult::SyncStatus(s) => s.state,
        other => panic!("expected sync status, got {other:?}"),
    }
}

/// Poll until the driver reports `want`.
async fn wait_state(core: &Core, want: SyncState) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if sync_state(core).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "never reached {want:?}; last = {:?}",
        sync_state(core).await
    );
}

/// Poll until the inbox holds exactly `n` tasks.
async fn wait_inbox_len(core: &Core, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if inbox_len(core).await == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("inbox never reached {n}; last = {}", inbox_len(core).await);
}

/// Poll until the DB-authoritative outbox count reaches zero.
async fn wait_pending_zero(core: &Core) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if core.sync_pending().unwrap() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("outbox never drained");
}
