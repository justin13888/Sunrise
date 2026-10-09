//! The APNs provider against a local HTTP/2 server, the key-file refusals,
//! log redaction, and the registration type's serde and `Debug` forms. A child
//! of the push tests so it shares their fixture, fake clock and delivery
//! helpers.

use super::*;

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
        .await
        .unwrap();
    let provider = Arc::new(apns(&mock, f.clock.clone()));
    delivery(&f, provider)
        .deliver(&intent(&f.laptop, TOKEN_HEX))
        .await;
    assert_eq!(mock.seen().len(), 1, "an unregistered token is not retried");
    assert!(f
        .state
        .store
        .push_tokens(&f.laptop)
        .await
        .unwrap()
        .is_empty());
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
