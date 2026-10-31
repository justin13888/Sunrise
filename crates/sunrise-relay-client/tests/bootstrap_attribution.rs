//! Which half of the bootstrap failed, as a caller is told it (issue #697).
//!
//! `BootstrapError` exists so a caller can tell a failed account call from a
//! failed device call, so these drive `bootstrap` and `register_device` against
//! a relay on a loopback socket that answers from a script. Each failure is
//! scripted identically on either route: only the half it hits differs, so the
//! arm that comes back is down to attribution and nothing else.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sunrise_relay_client::{
    bootstrap, register_device, AccountCreateRequest, BootstrapError, DeviceIdentity,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ACCOUNTS: &str = "/api/v1/accounts";
const DEVICES: &str = "/api/v1/devices";
const BEARER: &str = "canned-access-token";

/// What the canned relay does with a request it has read.
#[derive(Clone, Copy)]
enum Reply {
    /// `201` with the route's success body.
    Created,
    /// `403`: the server refused the request.
    Refused,
    /// `201` with a body no generated model decodes.
    Undecodable,
    /// Close the connection without a response.
    Hangup,
}

/// One request as the relay read it.
struct Recorded {
    path: String,
    authorization: Option<String>,
    body: Value,
}

/// A loopback relay that answers each route from its script and records what it
/// was asked. It lives as long as the test's runtime.
struct CannedRelay {
    base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl CannedRelay {
    async fn start(accounts: Reply, devices: Reply) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binds a loopback port");
        let base_url = format!("http://{}", listener.local_addr().expect("has an address"));
        let requests = Arc::new(Mutex::new(Vec::new()));

        let log = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accepts");
                let log = Arc::clone(&log);
                tokio::spawn(answer(stream, accounts, devices, log));
            }
        });

        Self { base_url, requests }
    }

    /// The paths asked for, in the order the relay read them.
    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("log lock")
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }
}

/// Read one request, record it, and answer it from the script.
async fn answer(
    mut stream: TcpStream,
    accounts: Reply,
    devices: Reply,
    log: Arc<Mutex<Vec<Recorded>>>,
) {
    let mut raw = Vec::new();
    let head_len = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("reads a request");
        if read == 0 {
            // An idle connection closed by the client: no request to answer.
            return;
        }
        raw.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8(raw[..head_len].to_vec()).expect("the head is text");
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let content_length: usize = header("content-length")
        .map_or(0, |value| value.parse().expect("a numeric content-length"));
    let authorization = header("authorization");

    while raw.len() < head_len + content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("reads the body");
        assert_ne!(read, 0, "the client closed mid-body");
        raw.extend_from_slice(&chunk[..read]);
    }

    let path = head
        .split_whitespace()
        .nth(1)
        .expect("a request line")
        .to_owned();
    let reply = match path.as_str() {
        ACCOUNTS => accounts,
        DEVICES => devices,
        other => panic!("bootstrap has no business calling {other}"),
    };
    let body = serde_json::from_slice(&raw[head_len..]).expect("the request body is JSON");
    let success = success_body(&path);
    log.lock().expect("log lock").push(Recorded {
        path,
        authorization,
        body,
    });

    let (status, content_type, payload) = match reply {
        Reply::Created => ("201 Created", "application/json", success),
        Reply::Refused => (
            "403 Forbidden",
            "application/problem+json",
            json!({ "type": "about:blank", "title": "Forbidden", "status": 403 }).to_string(),
        ),
        Reply::Undecodable => (
            "201 Created",
            "application/json",
            "{\"not\":\"the model\"}".to_owned(),
        ),
        Reply::Hangup => return,
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("writes the response");
}

fn success_body(path: &str) -> String {
    if path == ACCOUNTS {
        json!({
            "identity_id": "CANNED-ACCOUNT",
            "email": "normalized@example.test",
            "tier": "free",
            "device_count": 1,
            "created_at_ms": 1,
        })
    } else {
        json!({ "device_id": "CANNED-DEVICE" })
    }
    .to_string()
}

fn account_at(terms_at_ms: u64) -> AccountCreateRequest {
    AccountCreateRequest {
        email: "user@example.test".to_owned(),
        identity_signing_pub: [1; 32],
        identity_dh_pub: [2; 32],
        recovery_blob: None,
        terms_at_ms,
    }
}

fn account() -> AccountCreateRequest {
    account_at(1_704_067_200_000)
}

fn device() -> DeviceIdentity {
    DeviceIdentity {
        device_pub_s: [3; 32],
        device_pub_d: None,
        device_cert: None,
        vault_device_id: None,
        nickname: "canned".to_owned(),
        platform: "linux".to_owned(),
        app_version: None,
    }
}

/// Every way a call can fail, scripted on the account half: the error is
/// `Account`, and the device half is never attempted.
#[tokio::test]
async fn an_account_failure_is_attributed_to_the_account_and_stops_there() {
    for failure in [Reply::Refused, Reply::Undecodable, Reply::Hangup] {
        let relay = CannedRelay::start(failure, Reply::Created).await;

        let error = bootstrap(&relay.base_url, BEARER, account(), device())
            .await
            .expect_err("the account call failed");

        assert!(
            matches!(error, BootstrapError::Account(_)),
            "attributed to the wrong half: {error}"
        );
        assert!(
            !relay.paths().iter().any(|path| path == DEVICES),
            "the device call must not follow a failed account call"
        );
    }
}

/// The same failures on the device half, after the account call succeeded: the
/// error is `Device`.
#[tokio::test]
async fn a_device_failure_after_a_good_account_call_is_attributed_to_the_device() {
    for failure in [Reply::Refused, Reply::Undecodable, Reply::Hangup] {
        let relay = CannedRelay::start(Reply::Created, failure).await;

        let error = bootstrap(&relay.base_url, BEARER, account(), device())
            .await
            .expect_err("the device call failed");

        assert!(
            matches!(error, BootstrapError::Device(_)),
            "attributed to the wrong half: {error}"
        );
        assert_eq!(
            relay.paths().first().map(String::as_str),
            Some(ACCOUNTS),
            "the account call comes first"
        );
    }
}

/// `register_device` alone, as a recovery calls it, is the device half and
/// never touches the account route.
#[tokio::test]
async fn register_device_on_its_own_fails_as_the_device_half() {
    for failure in [Reply::Refused, Reply::Undecodable, Reply::Hangup] {
        let relay = CannedRelay::start(Reply::Created, failure).await;

        let error = register_device(&relay.base_url, BEARER, device())
            .await
            .expect_err("the device call failed");

        assert!(
            matches!(error, BootstrapError::Device(_)),
            "attributed to the wrong half: {error}"
        );
        assert!(!relay.paths().iter().any(|path| path == ACCOUNTS));
    }
}

/// A base URL the client cannot be built from fails before any request, and is
/// still attributed to the half the caller asked for.
#[tokio::test]
async fn an_unusable_base_url_is_attributed_to_the_half_that_built_the_client() {
    let error = bootstrap("not a url", BEARER, account(), device())
        .await
        .expect_err("no client can be built");
    assert!(
        matches!(error, BootstrapError::Account(_)),
        "attributed to the wrong half: {error}"
    );

    let error = register_device("not a url", BEARER, device())
        .await
        .expect_err("no client can be built");
    assert!(
        matches!(error, BootstrapError::Device(_)),
        "attributed to the wrong half: {error}"
    );
}

/// A timestamp past `i64::MAX` is refused rather than wrapped, and refused
/// before anything reaches the relay.
#[tokio::test]
async fn terms_at_ms_above_i64_max_is_refused_before_any_request() {
    let first_refused = u64::try_from(i64::MAX).expect("i64::MAX fits") + 1;
    for terms_at_ms in [first_refused, u64::MAX] {
        let relay = CannedRelay::start(Reply::Created, Reply::Created).await;

        let error = bootstrap(&relay.base_url, BEARER, account_at(terms_at_ms), device())
            .await
            .expect_err("the timestamp is not representable");

        match error {
            BootstrapError::Account(message) => assert_eq!(
                message, "terms_at_ms is not a representable timestamp",
                "for {terms_at_ms}"
            ),
            BootstrapError::Device(_) => panic!("attributed to the wrong half: {error}"),
        }
        assert!(
            relay.paths().is_empty(),
            "nothing may reach the relay for {terms_at_ms}"
        );
    }
}

/// `i64::MAX` itself is the last value that goes through, unchanged.
#[tokio::test]
async fn terms_at_ms_at_i64_max_is_sent_as_is() {
    let relay = CannedRelay::start(Reply::Created, Reply::Created).await;
    let largest = u64::try_from(i64::MAX).expect("i64::MAX fits");

    bootstrap(&relay.base_url, BEARER, account_at(largest), device())
        .await
        .expect("the timestamp is representable");

    let requests = relay.requests.lock().expect("log lock");
    assert_eq!(requests[0].path, ACCOUNTS);
    assert_eq!(requests[0].body["terms_at_ms"], json!(i64::MAX));
}

/// The success the failures are measured against: both calls in order, each
/// with the bearer, and the outcome read from the right response.
#[tokio::test]
async fn a_good_bootstrap_calls_both_halves_in_order_and_reports_what_they_returned() {
    let relay = CannedRelay::start(Reply::Created, Reply::Created).await;

    let outcome = bootstrap(&relay.base_url, BEARER, account(), device())
        .await
        .expect("both calls succeed");

    assert_eq!(outcome.identity_id, "CANNED-ACCOUNT");
    assert_eq!(outcome.email, "normalized@example.test");
    assert_eq!(outcome.device_id, "CANNED-DEVICE");

    assert_eq!(relay.paths(), [ACCOUNTS, DEVICES]);
    let expected = format!("Bearer {BEARER}");
    for request in relay.requests.lock().expect("log lock").iter() {
        assert_eq!(
            request.authorization.as_deref(),
            Some(expected.as_str()),
            "{} carries the bearer",
            request.path
        );
    }
}
