//! Session ends the relay chooses (#278).

use super::*;

/// One scripted answer to `recv_frame`.
type Recv = Result<Option<Vec<u8>>, TransportError>;

/// A transport that answers `recv_frame` from a script, then waits
/// forever, and accepts every send.
///
/// The fake relay above cannot stand in here: its duplex only ever yields
/// `Ok`, and a refused stream is a [`TransportError::Server`] out of
/// `recv_frame`, which is the value these tests are about.
struct ScriptedTransport(VecDeque<Recv>);

#[async_trait]
impl Transport for ScriptedTransport {
    async fn send_frame(&mut self, _frame: Vec<u8>) -> Result<(), TransportError> {
        Ok(())
    }
    async fn recv_frame(&mut self) -> Recv {
        match self.0.pop_front() {
            Some(r) => r,
            None => std::future::pending().await,
        }
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

// Returns a script entry, so it is shaped like one: the `Ok` is what the
// scripted transport hands `recv_frame`, not a fallibility this has.
#[allow(clippy::unnecessary_wraps)]
fn hello_ack() -> Recv {
    let ack = HelloAck {
        server_app_v: "scripted".into(),
        wire_proto: 1,
        crypto_suite: 1,
        doc_schema_floor: 1,
        capabilities: 0,
        server_time_ms: T0,
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&ack, &mut buf).unwrap();
    Ok(Some(
        encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &buf).unwrap(),
    ))
}

#[allow(clippy::unnecessary_wraps)] // a script entry; see `hello_ack`
fn close_frame(code: ErrorCode) -> Recv {
    let payload = ClosePayload {
        code,
        reason: format!("scripted {code}"),
    }
    .encode()
    .unwrap();
    Ok(Some(
        encode_frame(MsgKind::Close, FrameFlags::EMPTY, &payload).unwrap(),
    ))
}

fn refusal(code: ErrorCode) -> Recv {
    Err(TransportError::Server {
        code: code.as_str(),
        message: "scripted refusal".into(),
    })
}

/// A factory that hands each connect the next script (an empty one once
/// they run out), and the number of connects it has served.
fn scripted(scripts: Vec<Vec<Recv>>) -> (TransportFactory, Arc<AtomicU64>) {
    let scripts = Arc::new(parking_lot::Mutex::new(
        scripts.into_iter().collect::<VecDeque<_>>(),
    ));
    let connects = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&connects);
    let factory: TransportFactory = Arc::new(move |_: CredentialRead| {
        counted.fetch_add(1, AtomicOrdering::SeqCst);
        let script = scripts.lock().pop_front().unwrap_or_default();
        Box::pin(async move {
            Ok(Box::new(ScriptedTransport(script.into_iter().collect())) as BoxTransport)
        }) as ConnectFuture
    });
    (factory, connects)
}

/// `run` against `factory`, under a subscriber the test can read, with a
/// receiver that sees every state the driver broadcasts.
fn drive(
    core: &Arc<Core>,
    factory: TransportFactory,
) -> (
    Arc<SyncShared>,
    EventLog,
    broadcast::Receiver<crate::SyncStatus>,
    tokio::task::JoinHandle<()>,
) {
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let log = EventLog::default();
    let (status_tx, status_rx) = broadcast::channel(256);
    let shared = SyncShared::new(status_tx, 0);
    shared.mark_active();
    let rng: Arc<dyn Rng> = Arc::new(SystemRng);
    let driver = tokio::spawn(
        run(Arc::downgrade(core), Arc::clone(&shared), factory, rng)
            .with_subscriber(tracing_subscriber::registry().with(log.clone())),
    );
    (shared, log, status_rx, driver)
}

async fn until(what: &str, cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(600), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw: {what}"));
}

/// Every state the driver broadcast so far.
fn states_seen(rx: &mut broadcast::Receiver<crate::SyncStatus>) -> Vec<SyncState> {
    let mut out = Vec::new();
    while let Ok(s) = rx.try_recv() {
        out.push(s.state);
    }
    out
}

/// `(result, err_code)` of every `ev` event, in order.
fn fields_of(log: &EventLog, ev: &str) -> Vec<(Option<String>, Option<String>)> {
    log.0
        .lock()
        .iter()
        .filter(|e| e.ev.as_deref() == Some(ev))
        .map(|e| (e.result.clone(), e.err_code.clone()))
        .collect()
}

fn named(result: &str, code: Option<ErrorCode>) -> (Option<String>, Option<String>) {
    (Some(result.into()), code.map(|c| c.as_str().to_owned()))
}

/// The mapping itself, against the codes the relay's stream closes with
/// (`AUTH_TOKEN_EXPIRED`, `AUTH_DEVICE_REVOKED`,
/// `RELAY_STORAGE_UNAVAILABLE`), `AUTH_TOKEN_INVALID`, and the one the
/// transport substitutes for a code it cannot read. A close parks the
/// driver exactly when the catalogue says retrying will not help.
#[test]
fn only_a_non_retryable_close_is_terminal() {
    let end_for = |code| {
        let Ok(Some(frame)) = close_frame(code) else {
            unreachable!()
        };
        let (_, payload) = decode_frame(&frame).unwrap();
        SessionEnd::from_close(&payload)
    };
    for code in [
        ErrorCode::AuthTokenExpired,
        ErrorCode::RelayStorageUnavailable,
    ] {
        assert!(
            !end_for(code).is_terminal(),
            "{code} is retryable and must reconnect"
        );
    }
    for code in [
        ErrorCode::AuthDeviceRevoked,
        ErrorCode::AuthTokenInvalid,
        ErrorCode::InternalUnknownCode,
    ] {
        assert!(end_for(code).is_terminal(), "{code} must stop the driver");
    }
    assert!(
        matches!(SessionEnd::from_close(&[0xff]), SessionEnd::Disconnected),
        "an undecodable close is a drop, not a verdict"
    );
    assert!(matches!(
        SessionEnd::from_recv_error(TransportError::Unavailable("gone".into())),
        SessionEnd::Disconnected
    ));
    assert!(matches!(
        SessionEnd::from_recv_error(TransportError::Server {
            code: ErrorCode::AuthDeviceSigInvalid.as_str(),
            message: String::new(),
        }),
        SessionEnd::Refused { code, .. } if code == "AUTH_DEVICE_SIG_INVALID"
    ));
}

/// Driver-level: a terminal close stops the reconnect loop, and a new
/// credential — the user signing in again — is what restarts it.
///
/// Before #278 the `Close` arm returned the same value as a dropped
/// socket, so this driver would have reconnected on the backoff schedule
/// against a relay that had revoked the device: hundreds of attempts in
/// the virtual hour below instead of none. The second session closes
/// before its handshake completes, which is the handshake's own `Close`
/// arm, and must stop the driver too.
///
/// The third session is the one that answers its handshake, and it is
/// there for the renewal the park consumed. `park_until_renewed` moves the
/// driver's watch handle to the new version, so the next connect's mark
/// finds nothing outstanding; only `run` carrying that version forward as
/// `marked_at_connect` makes the handshake report it. Drop the carry and
/// `sync.credential.marked_at_connect` never appears. The second session
/// never handshakes, so it must leave the mark outstanding rather than
/// report it, and the third reports the version it actually presented.
#[tokio::test(start_paused = true)]
async fn a_terminal_close_parks_the_driver_until_the_credential_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let credential = TokenSource::new(Some("first-token".into()));
    let mut cfg = make_cfg(dir.path());
    cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
        )
        .await
        .unwrap(),
    );
    let (factory, connects) = scripted(vec![
        vec![hello_ack(), close_frame(ErrorCode::AuthDeviceRevoked)],
        vec![close_frame(ErrorCode::AuthTokenInvalid)],
        vec![hello_ack(), close_frame(ErrorCode::AuthDeviceRevoked)],
    ]);
    let (shared, log, _rx, driver) = drive(&core, factory);
    let n = || connects.load(AtomicOrdering::SeqCst);

    until("the first terminal close", || {
        shared.current_state() == SyncState::Stopped
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(n(), 1, "a stopped driver must not reconnect on a timer");
    assert_eq!(shared.current_state(), SyncState::Stopped);

    credential.set(Some("second-token".into()));
    until("the reconnect a new credential buys", || n() == 2).await;
    until("the second terminal close", || {
        shared.current_state() == SyncState::Stopped
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        n(),
        2,
        "and it stops again when that session is refused too"
    );

    credential.set(Some("third-token".into()));
    until("the reconnect the third credential buys", || n() == 3).await;
    until("the third terminal close", || {
        shared.current_state() == SyncState::Stopped
    })
    .await;

    shared.request_shutdown();
    timeout(Duration::from_secs(60), driver)
        .await
        .expect("a stopped driver must still honour shutdown")
        .unwrap();

    assert_eq!(
        fields_of(&log, "sync.session.closed"),
        vec![
            named("stopped", Some(ErrorCode::AuthDeviceRevoked)),
            named("stopped", Some(ErrorCode::AuthTokenInvalid)),
            named("stopped", Some(ErrorCode::AuthDeviceRevoked)),
        ]
    );
    // The credential events in order, with the version each names.
    let credential_events: Vec<(String, Option<u64>)> = log
        .0
        .lock()
        .iter()
        .filter_map(|e| match e.ev.as_deref() {
            Some(ev @ ("sync.session.resumed" | MARKED_AT_CONNECT)) => {
                Some((ev.to_owned(), e.to_v))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        credential_events,
        vec![
            ("sync.session.resumed".to_owned(), Some(1)),
            ("sync.session.resumed".to_owned(), Some(2)),
            (MARKED_AT_CONNECT.to_owned(), Some(2)),
        ],
        "each resume names its credential, the unanswered second session \
         reports nothing, and the answered third reports the renewal the \
         park consumed"
    );
    core.shutdown().await;
}

/// Driver-level: a retryable close reconnects, and so does a close
/// whose payload does not decode. None may ever read as `Stopped`.
///
/// `RELAY_STORAGE_UNAVAILABLE` is in the set because the relay sends it
/// for a failed durable-log read and logs it `transient`/`retryable`
/// itself: a driver that parked on it would strand every device that
/// opened a stream during the fault until its user signed in again.
#[tokio::test(start_paused = true)]
async fn a_recoverable_close_reconnects_and_names_its_code() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let garbage = Ok(Some(
        encode_frame(MsgKind::Close, FrameFlags::EMPTY, &[0xff]).unwrap(),
    ));
    let (factory, connects) = scripted(vec![
        vec![hello_ack(), close_frame(ErrorCode::AuthTokenExpired)],
        vec![hello_ack(), garbage],
        vec![hello_ack(), close_frame(ErrorCode::RelayStorageUnavailable)],
    ]);
    let (shared, log, mut rx, driver) = drive(&core, factory);

    until("three reconnects", || {
        connects.load(AtomicOrdering::SeqCst) >= 4
    })
    .await;
    shared.request_shutdown();
    timeout(Duration::from_secs(60), driver)
        .await
        .expect("the driver never stopped")
        .unwrap();

    let states = states_seen(&mut rx);
    assert!(
        !states.contains(&SyncState::Stopped),
        "no close here is terminal, saw {states:?}"
    );
    assert_eq!(
        fields_of(&log, "sync.session.closed")[..3],
        [
            named("failed", Some(ErrorCode::AuthTokenExpired)),
            named("failed", None),
            named("failed", Some(ErrorCode::RelayStorageUnavailable)),
        ]
    );
    assert_eq!(
        fields_of(&log, "sync.session.error")[..2],
        [
            named("failed", Some(ErrorCode::AuthTokenExpired)),
            named("failed", Some(ErrorCode::RelayStorageUnavailable)),
        ],
        "a retryable close is logged as one, never as `stopped`"
    );
    let storage_error = log
        .0
        .lock()
        .iter()
        .find(|e| {
            e.ev.as_deref() == Some("sync.session.error")
                && e.err_code.as_deref() == Some(ErrorCode::RelayStorageUnavailable.as_str())
        })
        .cloned()
        .expect("the storage close is logged");
    assert_eq!(
        (storage_error.err_kind.as_deref(), storage_error.retryable),
        (Some("transient"), Some(true)),
        "the client's line agrees with codes.toml and with the relay's own log"
    );
    core.shutdown().await;
}

/// Driver-level: a refused stream reaches the log under the relay's own
/// code, whether the refusal lands during the handshake or mid-session,
/// and the driver reconnects as it does for a drop.
///
/// Before #278 both `recv_frame` sites discarded the error unread, so
/// these two sessions logged exactly what a dropped TCP connection logs.
#[tokio::test(start_paused = true)]
async fn a_refused_stream_is_logged_under_the_relays_code() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let (factory, connects) = scripted(vec![
        vec![refusal(ErrorCode::AuthTokenInvalid)],
        vec![hello_ack(), refusal(ErrorCode::AuthDeviceSigInvalid)],
    ]);
    let (shared, log, mut rx, driver) = drive(&core, factory);

    until("two reconnects", || {
        connects.load(AtomicOrdering::SeqCst) >= 3
    })
    .await;
    shared.request_shutdown();
    timeout(Duration::from_secs(60), driver)
        .await
        .expect("the driver never stopped")
        .unwrap();

    assert!(!states_seen(&mut rx).contains(&SyncState::Stopped));
    assert_eq!(
        fields_of(&log, "sync.session.closed")[..2],
        [
            named("failed", Some(ErrorCode::AuthTokenInvalid)),
            named("failed", Some(ErrorCode::AuthDeviceSigInvalid)),
        ]
    );
    assert_eq!(
        fields_of(&log, "sync.session.error")[..2],
        [
            named("failed", Some(ErrorCode::AuthTokenInvalid)),
            named("failed", Some(ErrorCode::AuthDeviceSigInvalid)),
        ]
    );
    core.shutdown().await;
}
