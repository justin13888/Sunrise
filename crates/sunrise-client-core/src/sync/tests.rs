use super::*;

fn status(state: SyncState, outbox_pending: u32) -> SyncStatus {
    SyncStatus {
        state,
        outbox_pending,
        peer_devices: 0,
        last_sync_ms: None,
        replay_applied: 0,
        replay_remaining: None,
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

/// What the two tests below wait against: a clock that never moves, so
/// the budget can never run out, and only the state the wait reads can
/// end it.
#[derive(Debug)]
pub(crate) struct Stuck;
impl sunrise_core::Clock for Stuck {
    fn now_ms(&self) -> u64 {
        1_767_225_600_000
    }
    fn timezone(&self) -> String {
        "UTC".to_owned()
    }
}

/// A clock that moves one second each time it is read.
#[derive(Debug)]
pub(crate) struct Stepping(pub(crate) AtomicU64);
impl sunrise_core::Clock for Stepping {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(1_000, Ordering::Relaxed)
    }
    fn timezone(&self) -> String {
        "UTC".to_owned()
    }
}

pub(crate) async fn vault_at(
    dir: &std::path::Path,
    clock: Arc<dyn sunrise_core::Clock>,
) -> Arc<Core> {
    let core = Core::open(
        sunrise_core::CoreConfig::with_clock(
            dir.to_path_buf(),
            "test",
            clock,
            Arc::new(sunrise_core::SystemRng),
        ),
        sunrise_core::Unlock::DevicePaired {
            root: sunrise_crypto::keys::VaultRootKey::from_bytes([4; 32]),
            paired: None,
        },
    )
    .await
    .unwrap();
    core.sync_credential().set(Some("token".into()));
    Arc::new(core)
}

/// A relay that answers with `frames` and then holds the stream open, so
/// the session ends only when one of them ends it.
struct Scripted(std::collections::VecDeque<Vec<u8>>);

#[async_trait::async_trait]
impl Transport for Scripted {
    async fn send_frame(&mut self, _frame: Vec<u8>) -> Result<(), TransportError> {
        Ok(())
    }
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        match self.0.pop_front() {
            Some(frame) => Ok(Some(frame)),
            None => std::future::pending().await,
        }
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

pub(crate) fn relay(frames: Vec<Vec<u8>>) -> TransportFactory {
    Arc::new(move |_: CredentialRead| {
        let script = Scripted(frames.clone().into());
        Box::pin(async move { Ok(Box::new(script) as BoxTransport) }) as ConnectFuture
    })
}

pub(crate) fn hello_ack() -> Vec<u8> {
    use sunrise_wire_protocol::{encode_frame, FrameFlags, HelloAck, MsgKind};
    let ack = HelloAck {
        server_app_v: "scripted".into(),
        wire_proto: 1,
        crypto_suite: 1,
        doc_schema_floor: 1,
        capabilities: 0,
        server_time_ms: sunrise_core::Clock::now_ms(&Stuck),
    };
    let body = sunrise_cbor::encode_canonical(&ack).unwrap();
    encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &body).unwrap()
}

/// A close the catalogue does not mark retryable, which parks the driver
/// in `Stopped` until the credential is replaced.
pub(crate) fn terminal_close() -> Vec<u8> {
    use sunrise_wire_protocol::{encode_frame, ClosePayload, FrameFlags, MsgKind};
    let body = ClosePayload {
        code: sunrise_error::ErrorCode::AuthDeviceRevoked,
        reason: "scripted".into(),
    }
    .encode()
    .unwrap();
    encode_frame(MsgKind::Close, FrameFlags::EMPTY, &body).unwrap()
}

/// A cursor gap naming no stream, which latches `Degraded` for the session.
pub(crate) fn cursor_gap() -> Vec<u8> {
    use sunrise_wire_protocol::{encode_frame, ErrorPayload, FrameFlags, MsgKind};
    let body = ErrorPayload {
        code: sunrise_error::ErrorCode::SyncCursorGap,
        reason: "scripted".into(),
        stream_id: None,
    }
    .encode()
    .unwrap();
    encode_frame(MsgKind::Error, FrameFlags::EMPTY, &body).unwrap()
}

/// `Stopped` and `Degraded` end the wait at once and hand back the status
/// that ended it, though the clock says the hour-long budget has not
/// begun to run.
#[tokio::test]
async fn a_refused_or_degraded_session_ends_the_wait_with_its_state() {
    for (frames, want) in [
        (vec![hello_ack(), terminal_close()], SyncState::Stopped),
        (vec![hello_ack(), cursor_gap()], SyncState::Degraded),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let core = vault_at(dir.path(), Arc::new(Stuck)).await;
        core.start_sync(relay(frames)).unwrap();
        let waited = tokio::time::timeout(
            Duration::from_secs(30),
            wait_until_caught_up(&core, 3_600_000, &mut |_| {}),
        )
        .await
        .unwrap_or_else(|_| panic!("a {want:?} session left the wait running"));
        let last = waited.unwrap().expect_err("not live");
        assert_eq!(last.state, want);
        assert!(ends_the_wait(last.state));
        core.shutdown().await;
    }
}

/// The other arm: a session that may still go live is waited for until
/// the budget passes on the core's clock, and the status read last says
/// it was neither of the states that end the wait early.
#[tokio::test]
async fn a_session_that_may_still_go_live_waits_out_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let core = vault_at(dir.path(), Arc::new(Stepping(AtomicU64::new(0)))).await;
    let mut reads = 0;
    let last = wait_until_caught_up(&core, 3_000, &mut |_| reads += 1)
        .await
        .unwrap()
        .expect_err("nothing is syncing");
    assert!(reads > 1, "it waited rather than reading once: {reads}");
    assert!(!ends_the_wait(last.state), "{:?}", last.state);
    core.shutdown().await;
}
