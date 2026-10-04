//! The push module's tests: the planner as a value, the worker over a real
//! store with a fake provider and a hand-moved clock, delivery and its
//! retries, and backpressure. [`apns`] holds the provider's own tests, the
//! key-file refusals and log redaction.

mod apns;

use super::dispatch::Delivery;
use super::*;
use crate::config::{ApnsConfig, ApnsEnvironment, ServerConfig};
use crate::state::{Clock, ServerState};
use crate::store::NewDevice;
use crate::Subject;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const T0_MS: u64 = 1_704_067_200_000;
const STREAM_A: [u8; 16] = [0xa1; 16];
const STREAM_B: [u8; 16] = [0xb2; 16];
const KEY_ID: &str = "ABC123DEFG";
const TEAM_ID: &str = "DEF123GHIJ";
const TOPIC: &str = "dev.sunrise.app";

/// A throwaway P-256 key, generated for these tests and used nowhere else.
const TEST_P8: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgidGvSBti8ruWgMx0
mZuydi+DB/0Jtx5iI+Qp7R+wv5+hRANCAASG8uP2a6lNOVMGqiUNAEQsBlG5+ehl
5IfQvAbcx2wPqTPUlS4c1T1XdrNxwdQF7fDIg9zvocP6RoeNQmrwppLB
-----END PRIVATE KEY-----
";

/// A clock a test moves by hand.
#[derive(Debug)]
struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }
    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

/// A provider that records what it was asked to send and answers from a
/// script, `Ok` once the script runs out.
#[derive(Debug, Default)]
struct Fake {
    sent: parking_lot::Mutex<Vec<PushIntent>>,
    script: parking_lot::Mutex<VecDeque<Result<(), PushError>>>,
}

impl Fake {
    fn scripted(script: Vec<Result<(), PushError>>) -> Arc<Self> {
        Arc::new(Self {
            sent: parking_lot::Mutex::default(),
            script: parking_lot::Mutex::new(script.into()),
        })
    }
    fn sent(&self) -> usize {
        self.sent.lock().len()
    }
}

#[async_trait]
impl PushProvider for Fake {
    fn platform(&self) -> PushPlatform {
        PushPlatform::Apns
    }
    async fn send(&self, intent: &PushIntent) -> Result<(), PushError> {
        self.sent.lock().push(intent.clone());
        self.script.lock().pop_front().unwrap_or(Ok(()))
    }
}

/// One account with a phone and a laptop, each holding an APNs token.
struct Fixture {
    state: ServerState,
    clock: Arc<TestClock>,
    account_id: String,
    phone: String,
    laptop: String,
}

fn device(seed: u8) -> NewDevice {
    NewDevice {
        device_pub_s: format!("pub-{seed}"),
        vault_device_id: None,
        device_pub_d: None,
        device_cert: None,
        nickname: format!("device-{seed}"),
        platform: "ios".to_owned(),
        app_version: None,
    }
}

fn fixture_with(provider: Arc<dyn PushProvider>, tuning: Tuning) -> Fixture {
    let clock = TestClock::at(T0_MS);
    let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
        .with_push(Dispatcher::with_tuning(provider, tuning));
    let account = state
        .store
        .resolve_account(&Subject::new("https://idp.example", "alice"), true, T0_MS)
        .unwrap();
    let phone = state
        .store
        .register_device(&account.account_id, &device(1), T0_MS)
        .unwrap()
        .device_id;
    let laptop = state
        .store
        .register_device(&account.account_id, &device(2), T0_MS)
        .unwrap()
        .device_id;
    for (id, token) in [(&phone, "aa01"), (&laptop, "bb02")] {
        state
            .store
            .upsert_push_token(id, "apns", token, T0_MS)
            .unwrap();
    }
    Fixture {
        state,
        clock,
        account_id: account.account_id,
        phone,
        laptop,
    }
}

fn fixture() -> Fixture {
    fixture_with(Arc::new(Fake::default()), Tuning::default())
}

impl Fixture {
    /// A batch the phone published to `stream`.
    fn wake(&self, stream: [u8; 16]) -> Wake {
        Wake {
            account_id: self.account_id.clone(),
            stream_id: stream,
            origin: Some(self.phone.clone()),
            kind: PushKind::Sync,
        }
    }

    fn rate_limited(&self) -> u64 {
        self.state.metrics.get_with(
            "sunrise_push_dispatch_total",
            &[("provider", "apns"), ("result", "rate_limited")],
        )
    }
}

fn devices(intents: &[PushIntent]) -> Vec<&str> {
    intents
        .iter()
        .map(|i| i.registration.device_id.as_str())
        .collect()
}

// -- the planner as a value -------------------------------------------------

#[test]
fn the_first_op_sends_and_the_rest_of_its_window_coalesce() {
    let mut p = Planner::new(&Tuning::default());
    assert_eq!(p.offer("d", STREAM_A, PushKind::Sync, 0), Offer::Send);
    assert_eq!(
        p.offer("d", STREAM_A, PushKind::Sync, 29_999),
        Offer::Coalesced
    );
    // Another stream is another key.
    assert_eq!(p.offer("d", STREAM_B, PushKind::Sync, 10), Offer::Send);
    // The window is 30 s from the push, not from the last op.
    assert!(p.due(29_999).is_empty());
    assert_eq!(
        p.due(30_000),
        vec![("d".to_owned(), STREAM_A, PushKind::Sync)]
    );
    // STREAM_B had no follower, so its window closes with nothing owed.
    assert!(p.due(30_010).is_empty());
}

#[test]
fn the_cap_is_ten_in_any_sixty_seconds() {
    let mut p = Planner::new(&Tuning::default());
    for t in 0..10 {
        assert!(p.admit("d", t), "push {t} is under the cap");
    }
    assert!(
        !p.admit("d", 59_999),
        "the eleventh inside the minute is refused"
    );
    assert!(p.admit("other", 59_999), "the cap is per device");
    assert!(p.admit("d", 60_000), "the first push has aged out");
}

#[test]
fn presence_lasts_until_the_last_stream_closes() {
    let presence = Presence::default();
    let first = presence.hold("d");
    let second = presence.hold("d");
    drop(first);
    assert!(presence.is_online("d"));
    drop(second);
    assert!(!presence.is_online("d"));
}

// -- the worker over a real store -------------------------------------------

#[test]
fn an_op_for_an_offline_device_wakes_it_exactly_once() {
    let f = fixture();
    let mut worker = f.state.push.test_worker(&f.state).unwrap();
    let intents = worker.plan_wake(&f.wake(STREAM_A));
    assert_eq!(
        devices(&intents),
        vec![f.laptop.as_str()],
        "the laptop is woken, and the phone that sent the batch is not"
    );
    assert_eq!(intents[0].registration.token, "bb02");
    assert_eq!(intents[0].registration.platform, PushPlatform::Apns);
}

#[test]
fn a_device_with_a_stream_open_is_not_woken() {
    let f = fixture();
    let mut worker = f.state.push.test_worker(&f.state).unwrap();
    let online = f.state.push.presence().hold(&f.laptop);
    assert!(worker.plan_wake(&f.wake(STREAM_A)).is_empty());
    drop(online);
    assert_eq!(
        devices(&worker.plan_wake(&f.wake(STREAM_A))),
        vec![f.laptop.as_str()],
        "closing the stream makes it wakeable again, with no window held over"
    );
}

#[test]
fn n_ops_inside_the_window_coalesce_to_one_push() {
    let f = fixture();
    let mut worker = f.state.push.test_worker(&f.state).unwrap();
    let mut sent = worker.plan_wake(&f.wake(STREAM_A)).len();
    for i in 1..=9 {
        f.clock.set(T0_MS + i * 3_000);
        sent += worker.plan_wake(&f.wake(STREAM_A)).len();
    }
    assert_eq!(sent, 1, "ten ops in 27 s are one push");

    // The ops after the push are owed one trailing push when the window ends,
    // because the device may have synced and slept before they landed.
    f.clock.set(T0_MS + 30_000);
    assert_eq!(devices(&worker.plan_due()), vec![f.laptop.as_str()]);
    f.clock.set(T0_MS + 60_000);
    assert!(
        worker.plan_due().is_empty(),
        "nothing arrived after the trailing push, so nothing more is owed"
    );
}

#[test]
fn the_per_device_cap_holds_across_streams() {
    let f = fixture();
    let mut worker = f.state.push.test_worker(&f.state).unwrap();
    let mut sent = 0;
    for s in 0..12u8 {
        sent += worker.plan_wake(&f.wake([s; 16])).len();
    }
    assert_eq!(sent, 10, "twelve streams in one instant are capped at ten");
    assert_eq!(f.rate_limited(), 2);
    f.clock.set(T0_MS + 60_000);
    assert_eq!(worker.plan_wake(&f.wake([0xee; 16])).len(), 1);
}

#[test]
fn a_revoked_device_is_never_woken() {
    let f = fixture();
    let mut worker = f.state.push.test_worker(&f.state).unwrap();
    // A window with a trailing push owed, then the revocation.
    assert_eq!(worker.plan_wake(&f.wake(STREAM_A)).len(), 1);
    f.clock.set(T0_MS + 1_000);
    assert!(worker.plan_wake(&f.wake(STREAM_A)).is_empty());
    f.state
        .store
        .revoke_device(&f.account_id, &f.laptop, T0_MS + 2_000)
        .unwrap();
    f.clock.set(T0_MS + 30_000);
    assert!(worker.plan_due().is_empty(), "not by the trailing push");
    assert!(
        worker.plan_wake(&f.wake(STREAM_B)).is_empty(),
        "and not by a fresh op"
    );
}

#[test]
fn a_disabled_dispatcher_has_no_worker_and_counts_nothing() {
    let state = ServerState::new(ServerConfig::default());
    assert!(state.push.provider().is_none());
    assert!(state.push.test_worker(&state).is_none());
    state.push.notify(
        &state,
        Wake {
            account_id: "a".into(),
            stream_id: STREAM_A,
            origin: None,
            kind: PushKind::Sync,
        },
    );
    assert!(!state
        .metrics
        .render()
        .contains("sunrise_push_dispatch_total"));
}

/// A provider that hands every intent to the test as it is sent.
#[derive(Debug)]
struct Channel(tokio::sync::mpsc::UnboundedSender<PushIntent>);

#[async_trait]
impl PushProvider for Channel {
    fn platform(&self) -> PushPlatform {
        PushPlatform::Apns
    }
    async fn send(&self, intent: &PushIntent) -> Result<(), PushError> {
        let _ = self.0.send(intent.clone());
        Ok(())
    }
}

async fn next(sent: &mut tokio::sync::mpsc::UnboundedReceiver<PushIntent>) -> PushIntent {
    tokio::time::timeout(Duration::from_secs(5), sent.recv())
        .await
        .expect("a push within five seconds")
        .unwrap()
}

/// The spawned worker, not a hand-driven one: its tick is what sends the
/// trailing push once the clock passes the window.
#[tokio::test]
async fn the_running_worker_sends_the_trailing_push_when_the_window_closes() {
    let (tx, mut sent) = tokio::sync::mpsc::unbounded_channel();
    let f = fixture_with(
        Arc::new(Channel(tx)),
        Tuning {
            tick: Duration::from_millis(10),
            ..Tuning::default()
        },
    );
    f.state.push.notify(&f.state, f.wake(STREAM_A));
    assert_eq!(next(&mut sent).await.registration.device_id, f.laptop);

    f.clock.set(T0_MS + 1_000);
    f.state.push.notify(&f.state, f.wake(STREAM_A));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sent.recv())
            .await
            .is_err(),
        "coalesced into the open window"
    );

    f.clock.set(T0_MS + 30_000);
    assert_eq!(next(&mut sent).await.registration.device_id, f.laptop);
}

// -- backpressure -----------------------------------------------------------

/// A full queue drops the wake, counts it, and returns at once. On a
/// current-thread runtime the lazily spawned worker cannot run until this test
/// yields, so the queue fills exactly.
#[tokio::test]
async fn a_full_queue_drops_wakes_and_counts_them() {
    let f = fixture_with(
        Arc::new(Fake::default()),
        Tuning {
            queue: 2,
            ..Tuning::default()
        },
    );
    for _ in 0..5 {
        f.state.push.notify(&f.state, f.wake(STREAM_A));
    }
    assert_eq!(
        f.state.metrics.get_with(
            "sunrise_push_dispatch_total",
            &[("provider", "apns"), ("result", "dropped")]
        ),
        3
    );
}

// -- delivery ---------------------------------------------------------------

fn quick() -> Tuning {
    Tuning {
        backoff: Duration::ZERO,
        ..Tuning::default()
    }
}

fn intent(device_id: &str, token: &str) -> PushIntent {
    PushIntent {
        registration: PushTokenRegistration {
            device_id: device_id.to_owned(),
            platform: PushPlatform::Apns,
            token: token.to_owned(),
        },
        kind: PushKind::Sync,
    }
}

fn delivery(f: &Fixture, provider: Arc<dyn PushProvider>) -> Delivery {
    Delivery::new(
        provider,
        Arc::clone(&f.state.store),
        f.state.metrics.clone(),
        quick(),
    )
}

fn count(f: &Fixture, result: &str) -> u64 {
    f.state.metrics.get_with(
        "sunrise_push_dispatch_total",
        &[("provider", "apns"), ("result", result)],
    )
}

#[tokio::test]
async fn a_server_error_is_retried_until_it_clears() {
    let f = fixture();
    let fake = Fake::scripted(vec![
        Err(PushError::Unavailable("503".into())),
        Err(PushError::Timeout),
        Ok(()),
    ]);
    delivery(&f, fake.clone())
        .deliver(&intent(&f.laptop, "bb02"))
        .await;
    assert_eq!(fake.sent(), 3);
    assert_eq!(count(&f, "ok"), 1);
    assert_eq!(count(&f, "failed") + count(&f, "timeout"), 0);
    assert_eq!(
        f.state.metrics.histogram_count(
            "sunrise_push_dispatch_duration_seconds",
            &[("provider", "apns")]
        ),
        3,
        "every attempt is timed"
    );
}

#[tokio::test]
async fn throttling_that_outlasts_the_retries_is_counted_as_rate_limited() {
    let f = fixture();
    let fake = Fake::scripted(vec![Err(PushError::Throttled("TooManyRequests".into())); 3]);
    delivery(&f, fake.clone())
        .deliver(&intent(&f.laptop, "bb02"))
        .await;
    assert_eq!(fake.sent(), 3);
    assert_eq!(count(&f, "rate_limited"), 1);
    assert_eq!(
        f.state.store.push_tokens(&f.laptop).unwrap().len(),
        1,
        "throttling says nothing about the token"
    );
}

#[tokio::test]
async fn a_rejection_is_not_retried() {
    let f = fixture();
    let fake = Fake::scripted(vec![Err(PushError::Rejected("400 BadTopic".into()))]);
    delivery(&f, fake.clone())
        .deliver(&intent(&f.laptop, "bb02"))
        .await;
    assert_eq!(fake.sent(), 1);
    assert_eq!(count(&f, "rejected"), 1);
}

#[tokio::test]
async fn an_unregistered_token_is_deleted_unless_it_was_replaced() {
    let f = fixture();
    let dead = || Fake::scripted(vec![Err(PushError::Unregistered("Unregistered".into()))]);

    // The device registered a new token between the send and the answer: the
    // replacement is kept.
    delivery(&f, dead())
        .deliver(&intent(&f.laptop, "0dd0"))
        .await;
    assert_eq!(f.state.store.push_tokens(&f.laptop).unwrap().len(), 1);

    delivery(&f, dead())
        .deliver(&intent(&f.laptop, "bb02"))
        .await;
    assert!(f.state.store.push_tokens(&f.laptop).unwrap().is_empty());
    assert_eq!(count(&f, "rejected"), 2);
}
