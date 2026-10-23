//! A restore reports its replay as n of N where the relay states N, and as n
//! alone where it does not (#459), against a real relay.
//!
//! Until the relay stated a backlog, a restore could count its replay and
//! not total it: the Apple sheet read "n changes so far" with no bar that
//! could fill. The relay now sends a `backlog` event before each stream's
//! replay, the driver keeps the count in `SyncStatus`, and the recovery flow
//! reports `Replaying { applied, total }`.
//!
//! "Where it does not" is a relay released before the event. It is simulated
//! here by a transport that drops the frame the event becomes, in front of
//! the same real relay. That is exactly what an older client-side reader sees
//! of a newer relay and what a newer reader sees of an older one: no frame.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sunrise_client_core::recovery::{recover_account, DeviceLabel, RecoveryProgress};
use sunrise_core::{BoxTransport, Core, CoreConfig, SyncConfig, TransportFactory};
use sunrise_core_bindings::{
    CoreQuery, CoreQueryResult, RecoveryListener, RecoveryStep, ReplayListener, ReplayProgress,
    SunriseCore,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_e2e::{signed_ws_factory, spawn_relay_with};
use sunrise_server::ServerConfig;
use sunrise_sync::{CredentialRead, SseTransport, Transport, TransportError};
use sunrise_wire_protocol::{decode_frame, BacklogPayload, MsgKind};

const BEARER: &str = "self-host";
const FOUNDING_ROOT: [u8; 32] = [0x5e; 32];
const RECOVERED_ROOT: [u8; 32] = [0x5f; 32];
const TASKS: usize = 6;

/// Found an account through the seam, write [`TASKS`] tasks, push them, and
/// return the recovery code.
async fn found(dir: &std::path::Path, base_url: &str) -> String {
    let core = SunriseCore::open(
        dir.to_string_lossy().into_owned(),
        FOUNDING_ROOT.to_vec(),
        "e2e".into(),
        None,
    )
    .await
    .expect("the app opens a vault");
    let outcome = core
        .bootstrap_account(
            base_url.to_owned(),
            BEARER.to_owned(),
            "alice@example.com".to_owned(),
            "Alice's Mac".to_owned(),
            1,
        )
        .await
        .expect("the app publishes its vault");
    for n in 0..TASKS {
        core.capture(format!("Task {n}"), "UTC".into())
            .await
            .expect("capture");
    }
    core.start_sync(
        base_url.to_owned(),
        Some(BEARER.to_owned()),
        Some(outcome.device_id),
    )
    .expect("sync starts");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(CoreQueryResult::SyncStatus { status }) =
                core.query(CoreQuery::SyncStatus).await
            {
                if status.outbox_pending == 0 && status.state == sunrise_sync::SyncState::Live {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the founding device never pushed its ops");
    core.shutdown().await;
    outcome
        .recovery_code
        .expect("a founding device produces a code")
}

/// A transport in front of the real one that drops every backlog statement:
/// what a relay that predates the event looks like from the driver.
struct NoBacklog(SseTransport);

fn is_backlog(frame: &[u8]) -> bool {
    decode_frame(frame).is_ok_and(|(head, payload)| {
        head.msg_kind == MsgKind::StreamUpdate && BacklogPayload::decode(&payload).is_ok()
    })
}

#[async_trait]
impl Transport for NoBacklog {
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.0.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            match self.0.recv_frame().await? {
                Some(frame) if is_backlog(&frame) => {}
                other => return Ok(other),
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.0.close().await
    }
}

fn no_backlog_factory(
    addr: SocketAddr,
    signer: Arc<dyn sunrise_sync::DeviceSigner>,
) -> TransportFactory {
    let url = format!("http://{addr}");
    Arc::new(move |read: CredentialRead| {
        let url = url.clone();
        let signer = Arc::clone(&signer);
        Box::pin(async move {
            let t =
                SseTransport::connect_with_bearer(&url, read.bearer()).with_device_signer(signer);
            Ok(Box::new(NoBacklog(t)) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

/// Restore from `code` through the client layer, and return every replay
/// step it reported as `(applied, total)`.
async fn restore(addr: SocketAddr, code: &str, stated: bool) -> Vec<(u64, Option<u64>)> {
    let base_url = format!("http://{addr}");
    let relay = sunrise_relay_client::RelayRecovery::new(&base_url, BEARER).expect("a client");
    let dir = tempfile::tempdir().expect("a vault dir");
    let mut cfg = CoreConfig::production(dir.path().to_path_buf(), "e2e".to_owned());
    cfg.sync = Some(SyncConfig::new(base_url.clone()));
    let start_sync = move |core: &Arc<Core>, relay_device_id: &str| {
        core.sync_credential().set(Some(BEARER.to_owned()));
        let signer = core.device_signer(relay_device_id);
        let factory = if stated {
            signed_ws_factory(addr, signer)
        } else {
            no_backlog_factory(addr, signer)
        };
        core.start_sync(factory).map_err(|e| e.to_string())
    };
    let mut seen = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(90),
        recover_account(
            &relay,
            code,
            cfg,
            VaultRootKey::from_bytes(RECOVERED_ROOT),
            DeviceLabel {
                nickname: "Alice's new laptop".to_owned(),
                platform: "linux".to_owned(),
                app_version: None,
            },
            &start_sync,
            60_000,
            &mut |p| {
                if let RecoveryProgress::Replaying { applied, total } = p {
                    seen.push((applied, total));
                }
            },
        ),
    )
    .await
    .expect("the recovery must not hang")
    .expect("the words restore the account");
    seen
}

/// Every report is consistent with itself and with the ones before it: the
/// count never goes back, and no total is below what has already arrived.
fn assert_consistent(seen: &[(u64, Option<u64>)]) {
    for pair in seen.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "the count went back: {seen:?}");
    }
    for (applied, total) in seen {
        if let Some(total) = total {
            assert!(applied <= total, "a total below the count: {seen:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_reports_n_of_n_where_the_relay_states_its_backlog() {
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| s).await;
    let founding = tempfile::tempdir().expect("a vault dir");
    let code = found(founding.path(), &format!("http://{addr}")).await;
    drop(founding);

    let seen = restore(addr, &code, true).await;
    assert!(!seen.is_empty(), "no replay was reported");
    assert_consistent(&seen);
    let (_, total) = seen.last().copied().expect("a report");
    let total = total.expect("the relay states its backlog, so there is a total");
    assert!(
        total >= TASKS as u64,
        "every task written before the loss is an op in the replay: {seen:?}"
    );
    relay.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_reports_n_alone_where_the_relay_states_nothing() {
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| s).await;
    let founding = tempfile::tempdir().expect("a vault dir");
    let code = found(founding.path(), &format!("http://{addr}")).await;
    drop(founding);

    let seen = restore(addr, &code, false).await;
    assert!(!seen.is_empty(), "no replay was reported");
    assert_consistent(&seen);
    assert!(
        seen.iter().all(|(_, total)| total.is_none()),
        "a total with no statement to draw it from: {seen:?}"
    );
    assert!(
        seen.last()
            .is_some_and(|(applied, _)| *applied >= TASKS as u64),
        "the count still runs: {seen:?}"
    );
    relay.abort();
}

/// Every replay step the seam reported, both ways.
#[derive(Default)]
struct Heard {
    steps: Mutex<Vec<RecoveryStep>>,
    replay: Mutex<Vec<ReplayProgress>>,
}

impl RecoveryListener for Heard {
    fn on_step(&self, step: RecoveryStep) {
        if let Ok(mut g) = self.steps.lock() {
            g.push(step);
        }
    }
}

impl ReplayListener for Heard {
    fn on_replay(&self, progress: ReplayProgress) {
        if let Ok(mut g) = self.replay.lock() {
            g.push(progress);
        }
    }
}

/// The seam's additive constructor hands the total to its replay listener,
/// and the step listener still hears every step it heard before.
#[tokio::test(flavor = "multi_thread")]
async fn the_seam_hands_the_total_to_a_replay_listener() {
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| s).await;
    let base_url = format!("http://{addr}");
    let founding = tempfile::tempdir().expect("a vault dir");
    let code = found(founding.path(), &base_url).await;
    drop(founding);

    let restored = tempfile::tempdir().expect("a vault dir");
    let heard = Arc::new(Heard::default());
    let core = tokio::time::timeout(
        Duration::from_secs(90),
        SunriseCore::recover_account_with_replay(
            restored.path().to_string_lossy().into_owned(),
            RECOVERED_ROOT.to_vec(),
            "e2e".into(),
            base_url,
            BEARER.to_owned(),
            code,
            "Alice's new iPhone".into(),
            Arc::clone(&heard) as Arc<dyn RecoveryListener>,
            Arc::clone(&heard) as Arc<dyn ReplayListener>,
            60_000,
        ),
    )
    .await
    .expect("the recovery must not hang")
    .expect("the words restore the account");

    let steps = heard.steps.lock().expect("lock").clone();
    let replay = heard.replay.lock().expect("lock").clone();
    assert_eq!(steps.last(), Some(&RecoveryStep::CaughtUp), "{steps:?}");
    let counted: Vec<u64> = steps
        .iter()
        .filter_map(|s| match s {
            RecoveryStep::Replaying { applied } => Some(*applied),
            _ => None,
        })
        .collect();
    assert_eq!(
        counted,
        replay.iter().map(|p| p.applied).collect::<Vec<_>>(),
        "each replay step reaches both listeners"
    );
    assert!(
        replay.last().and_then(|p| p.total).is_some(),
        "the relay states its backlog: {replay:?}"
    );
    core.shutdown().await;
    relay.abort();
}
