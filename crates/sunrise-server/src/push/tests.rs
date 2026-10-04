//! The push module's tests: the planner as a value, the worker over a real
//! store with a fake provider and a hand-moved clock, delivery and its
//! retries, the APNs provider against a local HTTP/2 server, the key-file
//! refusals, backpressure, and log redaction.

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

// -- APNs against a local HTTP/2 server ---------------------------------------

/// One request the mock received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

/// An h2c server answering from a script, `200` once it runs out.
struct MockApns {
    endpoint: String,
    seen: Arc<parking_lot::Mutex<Vec<Seen>>>,
}

impl MockApns {
    async fn start(replies: Vec<(u16, &'static str)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let replies = Arc::new(parking_lot::Mutex::new(VecDeque::from(replies)));
        let (log, script) = (Arc::clone(&seen), Arc::clone(&replies));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (log, script) = (Arc::clone(&log), Arc::clone(&script));
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let (log, script) = (Arc::clone(&log), Arc::clone(&script));
                        async move {
                            use http_body_util::BodyExt as _;
                            let (parts, body) = req.into_parts();
                            let body = body.collect().await.unwrap().to_bytes().to_vec();
                            log.lock().push(Seen {
                                method: parts.method.to_string(),
                                path: parts.uri.path().to_owned(),
                                headers: parts
                                    .headers
                                    .iter()
                                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                                    .collect(),
                                body,
                            });
                            let (status, reply) = script.lock().pop_front().unwrap_or((200, ""));
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder()
                                    .status(status)
                                    .body(http_body_util::Full::new(bytes::Bytes::from(reply)))
                                    .unwrap(),
                            )
                        }
                    },
                );
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http2::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
                });
            }
        });
        Self { endpoint, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }
}

fn apns_config(key_path: std::path::PathBuf) -> ApnsConfig {
    ApnsConfig {
        key_path,
        key_id: KEY_ID.to_owned(),
        team_id: TEAM_ID.to_owned(),
        topic: TOPIC.to_owned(),
        environment: ApnsEnvironment::Sandbox,
    }
}

fn test_der() -> Vec<u8> {
    use base64::Engine as _;
    let b64: String = TEST_P8
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap()
}

fn apns(mock: &MockApns, clock: Arc<TestClock>) -> ApnsProvider {
    ApnsProvider::build(
        &test_der(),
        &apns_config("unused".into()),
        mock.endpoint.clone(),
        true,
        clock,
    )
}

/// Verify `jwt` against the test key's public half and return its header and
/// claims.
fn verify(jwt: &str) -> (jsonwebtoken::Header, serde_json::Value) {
    let der = test_der();
    // A PKCS#8 P-256 key ends with its uncompressed public point.
    let public = jsonwebtoken::DecodingKey::from_ec_der(&der[der.len() - 65..]);
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    validation.validate_exp = false;
    validation.required_spec_claims.clear();
    let data = jsonwebtoken::decode::<serde_json::Value>(jwt, &public, &validation)
        .expect("the provider token verifies under the key's public half");
    (data.header, data.claims)
}

const TOKEN_HEX: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

#[tokio::test]
async fn an_apns_push_is_exactly_the_documented_request() {
    let mock = MockApns::start(vec![]).await;
    let clock = TestClock::at(T0_MS);
    let provider = apns(&mock, clock);
    provider.send(&intent("dev", TOKEN_HEX)).await.unwrap();

    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, format!("/3/device/{TOKEN_HEX}"));
    assert_eq!(req.body, br#"{"aps":{"content-available":1}}"#);
    assert_eq!(req.headers["apns-push-type"], "background");
    assert_eq!(req.headers["apns-priority"], "5");
    assert_eq!(req.headers["apns-topic"], TOPIC);
    assert_eq!(req.headers["content-type"], "application/json");

    let jwt = req.headers["authorization"]
        .strip_prefix("bearer ")
        .expect("a bearer provider token");
    let (header, claims) = verify(jwt);
    assert_eq!(header.alg, jsonwebtoken::Algorithm::ES256);
    assert_eq!(header.kid.as_deref(), Some(KEY_ID));
    assert_eq!(claims["iss"], TEAM_ID);
    assert_eq!(claims["iat"], T0_MS / 1000);
    assert_eq!(
        claims.as_object().unwrap().len(),
        2,
        "iss and iat, nothing else: {claims}"
    );
}

#[tokio::test]
async fn the_provider_token_is_reused_then_re_signed() {
    let mock = MockApns::start(vec![
        (200, ""),
        (200, ""),
        (403, r#"{"reason":"ExpiredProviderToken"}"#),
        (200, ""),
        (200, ""),
    ])
    .await;
    let clock = TestClock::at(T0_MS);
    let provider = apns(&mock, clock.clone());
    let bearer = |n: usize| mock.seen()[n].headers["authorization"].clone();

    provider.send(&intent("dev", TOKEN_HEX)).await.unwrap();
    clock.set(T0_MS + 39 * 60_000);
    provider.send(&intent("dev", TOKEN_HEX)).await.unwrap();
    assert_eq!(bearer(0), bearer(1), "reused inside forty minutes");

    // APNs calls it expired early: the retry carries a fresh one.
    clock.set(T0_MS + 39 * 60_000 + 1_000);
    assert!(matches!(
        provider.send(&intent("dev", TOKEN_HEX)).await,
        Err(PushError::Unavailable(_))
    ));
    provider.send(&intent("dev", TOKEN_HEX)).await.unwrap();
    assert_ne!(bearer(2), bearer(3));

    clock.set(T0_MS + 120 * 60_000);
    provider.send(&intent("dev", TOKEN_HEX)).await.unwrap();
    assert_ne!(bearer(3), bearer(4), "re-signed once forty minutes old");
}

#[tokio::test]
async fn each_apns_status_is_classified_for_the_dispatcher() {
    let mock = MockApns::start(vec![
        (410, r#"{"reason":"Unregistered"}"#),
        (400, r#"{"reason":"BadDeviceToken"}"#),
        (400, r#"{"reason":"TopicDisallowed"}"#),
        (429, r#"{"reason":"TooManyRequests"}"#),
        (503, r#"{"reason":"ServiceUnavailable"}"#),
    ])
    .await;
    let provider = apns(&mock, TestClock::at(T0_MS));
    let mut outcomes = Vec::new();
    for _ in 0..5 {
        outcomes.push(provider.send(&intent("dev", TOKEN_HEX)).await);
    }
    assert_eq!(
        outcomes,
        vec![
            Err(PushError::Unregistered("Unregistered".into())),
            Err(PushError::Unregistered("BadDeviceToken".into())),
            Err(PushError::Rejected("400 TopicDisallowed".into())),
            Err(PushError::Throttled("TooManyRequests".into())),
            Err(PushError::Unavailable("503 ServiceUnavailable".into())),
        ]
    );
}

#[tokio::test]
async fn a_token_that_cannot_be_hex_is_never_sent() {
    let mock = MockApns::start(vec![]).await;
    let provider = apns(&mock, TestClock::at(T0_MS));
    assert!(matches!(
        provider.send(&intent("dev", "../../3/device/x")).await,
        Err(PushError::Unregistered(_))
    ));
    assert!(mock.seen().is_empty());
}

#[tokio::test]
async fn a_410_from_apns_deletes_the_token_row() {
    let mock = MockApns::start(vec![(410, r#"{"reason":"Unregistered"}"#)]).await;
    let f = fixture();
    f.state
        .store
        .upsert_push_token(&f.laptop, "apns", TOKEN_HEX, T0_MS)
        .unwrap();
    let provider = Arc::new(apns(&mock, f.clock.clone()));
    delivery(&f, provider)
        .deliver(&intent(&f.laptop, TOKEN_HEX))
        .await;
    assert_eq!(mock.seen().len(), 1, "an unregistered token is not retried");
    assert!(f.state.store.push_tokens(&f.laptop).unwrap().is_empty());
    assert_eq!(count(&f, "rejected"), 1);
}

// -- the key file -------------------------------------------------------------

#[cfg(unix)]
fn key_file(dir: &tempfile::TempDir, contents: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.path().join("AuthKey.p8");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn a_key_file_others_can_read_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = TestClock::at(T0_MS);

    let open = key_file(&dir, TEST_P8, 0o644);
    let refused = ApnsProvider::from_config(&apns_config(open), clock.clone());
    assert!(
        matches!(
            refused,
            Err(PushSetupError::KeyPermissions { mode: 0o644, .. })
        ),
        "{refused:?}"
    );

    let private = key_file(&dir, TEST_P8, 0o600);
    assert!(ApnsProvider::from_config(&apns_config(private.clone()), clock.clone()).is_ok());

    // The whole startup path refuses it too, which is what `main` maps to 78.
    key_file(&dir, TEST_P8, 0o640);
    let config = ServerConfig {
        push: crate::config::PushConfig {
            apns: Some(apns_config(private)),
        },
        ..ServerConfig::default()
    };
    assert!(matches!(
        ServerState::try_new(config),
        Err(crate::state::StartError::Push(
            PushSetupError::KeyPermissions { .. }
        ))
    ));
}

#[cfg(unix)]
#[test]
fn a_key_file_that_is_not_a_p8_key_is_refused_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = TestClock::at(T0_MS);
    for junk in [
        "not a key",
        "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
    ] {
        let path = key_file(&dir, junk, 0o600);
        let refused = ApnsProvider::from_config(&apns_config(path), clock.clone());
        assert!(
            matches!(refused, Err(PushSetupError::BadKey { .. })),
            "{refused:?}"
        );
    }
    let missing = ApnsProvider::from_config(&apns_config(dir.path().join("absent.p8")), clock);
    assert!(matches!(missing, Err(PushSetupError::KeyUnreadable { .. })));
}

// -- log redaction ------------------------------------------------------------

/// A token is written in no log record, whatever happens to the push. Driven
/// through the real APNs provider, so its own error text is what is checked.
#[test]
fn no_log_record_carries_the_token() {
    use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};
    const SENTINEL: &str = "5e171e15e171e15e171e15e171e15e171e15e171e15e171e15e171e15e171e1";

    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: "trace".to_owned(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tracing::dispatcher::with_default(&dispatch, || {
        rt.block_on(async {
            let mock = MockApns::start(vec![
                (400, r#"{"reason":"BadDeviceToken"}"#),
                (403, r#"{"reason":"TopicDisallowed"}"#),
                (500, r#"{"reason":"InternalServerError"}"#),
                (500, r#"{"reason":"InternalServerError"}"#),
                (500, r#"{"reason":"InternalServerError"}"#),
            ])
            .await;
            let f = fixture();
            let provider: Arc<dyn PushProvider> = Arc::new(apns(&mock, f.clock.clone()));
            for _ in 0..3 {
                delivery(&f, Arc::clone(&provider))
                    .deliver(&intent(&f.laptop, SENTINEL))
                    .await;
            }
            assert_eq!(mock.seen().len(), 5);
        });
    });
    let out = cap.contents();
    assert!(out.contains("srv.push.token_unregistered"), "{out}");
    assert!(out.contains("srv.push.delivery_failed"), "{out}");
    assert!(
        !out.contains(SENTINEL),
        "a push token reached a log record: {out}"
    );
    assert!(!out.contains(&SENTINEL[..16]), "a prefix of one did: {out}");
}

#[test]
fn a_registrations_debug_form_redacts_the_token() {
    let shown = format!("{:?}", intent("dev", "feedface").registration);
    assert!(!shown.contains("feedface"), "{shown}");
}

/// The `device_id_hex` alias is a documented compatibility promise —
/// "accepted so clients written against the pre-persistence shape keep
/// parsing" — and nothing tested it, so deleting the attribute would have
/// broken exactly those clients silently.
#[test]
fn the_pre_persistence_device_id_spelling_still_parses() {
    let legacy: PushTokenRegistration = serde_json::from_str(
        r#"{"device_id_hex":"0000000000000000000000000Z","platform":"apns","token":"t"}"#,
    )
    .expect("the alias must keep parsing");
    assert_eq!(legacy.device_id, "0000000000000000000000000Z");

    // The current spelling reaches the same field, so the alias is an
    // addition rather than a replacement.
    let current: PushTokenRegistration = serde_json::from_str(
        r#"{"device_id":"0000000000000000000000000Z","platform":"apns","token":"t"}"#,
    )
    .expect("the current spelling parses");
    assert_eq!(current.device_id, legacy.device_id);
}
