//! The rendezvous through the real router, on a hand-moved clock.

use super::{MAX_PAIR_MESSAGE, SESSION_TTL_MS};
use crate::api::error::codes;
use crate::api::testing::{code_of, Client, Res, BEARER, SECOND_BEARER};
use crate::state::{Clock, ServerState};
use crate::{ServerConfig, StaticVerifier, Subject};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use kynos::http::{Method, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const T0_MS: u64 = 1_800_000_000_000;

#[derive(Debug)]
struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// Two accounts, `alice` behind [`BEARER`] and `bob` behind
/// [`SECOND_BEARER`], and a clock the test moves.
fn rendezvous() -> (Client, Arc<TestClock>) {
    let clock = Arc::new(TestClock(AtomicU64::new(T0_MS)));
    let state =
        ServerState::with_clock(ServerConfig::default(), clock.clone()).with_verifier(Arc::new(
            StaticVerifier::default()
                .with("test", Subject::new("https://idp.example", "alice"))
                .with("second", Subject::new("https://idp.example", "bob")),
        ));
    (Client::from_state(state), clock)
}

fn pair_id(seed: u8) -> String {
    URL_SAFE_NO_PAD.encode([seed; 16])
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The new device, and the existing one.
const NEW: &str = "new_device";
const EXISTING: &str = "existing_device";

async fn send_as(
    client: &Client,
    bearer: &str,
    pair: &str,
    role: &str,
    index: u32,
    message: &[u8],
) -> Res {
    client
        .send_as(
            Method::POST,
            "/api/v1/pairing/send",
            Some(bearer),
            Some(&serde_json::json!({
                "pair_id": pair,
                "role": role,
                "index": index,
                "message": b64(message),
            })),
        )
        .await
}

/// Send `message` as `role`'s message number `index`, as [`BEARER`].
async fn send(client: &Client, pair: &str, role: &str, index: u32, message: &[u8]) -> Res {
    send_as(client, BEARER, pair, role, index, message).await
}

async fn receive_as(client: &Client, bearer: &str, pair: &str, role: &str, after: u32) -> Res {
    client
        .send_as(
            Method::POST,
            "/api/v1/pairing/receive",
            Some(bearer),
            Some(&serde_json::json!({ "pair_id": pair, "role": role, "after": after })),
        )
        .await
}

async fn receive(client: &Client, pair: &str, role: &str, after: u32) -> Vec<Vec<u8>> {
    let res = receive_as(client, BEARER, pair, role, after).await;
    res.assert_status(StatusCode::OK);
    res.json()["messages"]
        .as_array()
        .expect("a message list")
        .iter()
        .map(|m| URL_SAFE_NO_PAD.decode(m.as_str().unwrap()).unwrap())
        .collect()
}

fn outcomes(client: &Client, result: &str) -> u64 {
    client
        .metrics
        .get_with("sunrise_pairing_total", &[("result", result)])
}

/// The whole exchange the protocol runs — three handshake messages, then the
/// offer, the request and the grant — crosses in order, each side reading only
/// what the other wrote.
#[tokio::test]
async fn a_pairing_crosses_the_relay_in_both_directions() {
    let (client, _) = rendezvous();
    let pair = pair_id(1);

    let opened = send(&client, &pair, NEW, 0, b"noise-1").await;
    opened.assert_status(StatusCode::OK);
    assert_eq!(opened.json()["sent"], 1);
    assert_eq!(opened.json()["expires_at_ms"], T0_MS + SESSION_TTL_MS);

    assert_eq!(
        receive(&client, &pair, EXISTING, 0).await,
        vec![b"noise-1".to_vec()]
    );
    assert!(
        receive(&client, &pair, NEW, 0).await.is_empty(),
        "a side never reads its own messages back"
    );

    send(&client, &pair, EXISTING, 0, b"noise-2")
        .await
        .assert_status(StatusCode::OK);
    assert_eq!(
        receive(&client, &pair, NEW, 0).await,
        vec![b"noise-2".to_vec()]
    );
    send(&client, &pair, NEW, 1, b"noise-3")
        .await
        .assert_status(StatusCode::OK);
    send(&client, &pair, EXISTING, 1, b"offer")
        .await
        .assert_status(StatusCode::OK);
    send(&client, &pair, NEW, 2, b"request")
        .await
        .assert_status(StatusCode::OK);
    let last = send(&client, &pair, EXISTING, 2, b"grant").await;
    last.assert_status(StatusCode::OK);
    assert_eq!(last.json()["sent"], 3);

    assert_eq!(
        receive(&client, &pair, EXISTING, 1).await,
        vec![b"noise-3".to_vec(), b"request".to_vec()],
        "the cursor skips what the caller already holds"
    );
    assert_eq!(
        receive(&client, &pair, NEW, 1).await,
        vec![b"offer".to_vec(), b"grant".to_vec()]
    );
    assert!(receive(&client, &pair, NEW, 9).await.is_empty());

    assert_eq!(outcomes(&client, "opened"), 1);
    assert_eq!(outcomes(&client, "relayed"), 5);
}

/// A send whose answer was lost is retried with the same index and message,
/// and the other side still reads it once.
#[tokio::test]
async fn a_resent_message_is_acknowledged_and_not_buffered_twice() {
    let (client, _) = rendezvous();
    let pair = pair_id(8);
    send(&client, &pair, NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);
    let again = send(&client, &pair, NEW, 0, b"noise-1").await;
    again.assert_status(StatusCode::OK);
    assert_eq!(again.json()["sent"], 1);

    assert_eq!(
        receive(&client, &pair, EXISTING, 0).await,
        vec![b"noise-1".to_vec()]
    );
    assert_eq!(outcomes(&client, "opened"), 1, "a resend is not an attempt");
    assert_eq!(outcomes(&client, "relayed"), 0);
}

/// An index whose slot holds a different message, or that skips past the next
/// free slot, means the two sides disagree about the exchange, and the session
/// is dropped.
#[tokio::test]
async fn a_conflicting_or_skipping_index_drops_the_session() {
    let (client, _) = rendezvous();

    let pair = pair_id(9);
    send(&client, &pair, NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);
    let conflict = send(&client, &pair, NEW, 0, b"something else").await;
    conflict.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&conflict), codes::RELAY_PAIR_SESSION_GONE);
    receive_as(&client, BEARER, &pair, EXISTING, 0)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    let pair = pair_id(10);
    send(&client, &pair, NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);
    send(&client, &pair, EXISTING, 1, b"offer")
        .await
        .assert_status(StatusCode::NOT_FOUND);
    receive_as(&client, BEARER, &pair, NEW, 0)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // And a later message cannot open a session: the one it belonged to is
    // gone, and a fresh one would leave its sender waiting for nobody.
    send(&client, &pair_id(11), NEW, 1, b"noise-3")
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(outcomes(&client, "opened"), 2);
}

/// The existing device joins a session; it cannot open one. Its message to a
/// pair id nobody opened is refused, and no session appears.
#[tokio::test]
async fn the_existing_device_cannot_open_a_session() {
    let (client, _) = rendezvous();
    let pair = pair_id(2);

    let res = send(&client, &pair, EXISTING, 0, b"noise-2").await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&res), codes::RELAY_PAIR_SESSION_GONE);

    let res = receive_as(&client, BEARER, &pair, NEW, 0).await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(outcomes(&client, "gone"), 2);
}

/// A fourth message from one role is past what the protocol sends, and the
/// spec drops the session rather than buffering it.
#[tokio::test]
async fn a_fourth_message_from_one_role_drops_the_session() {
    let (client, _) = rendezvous();
    let pair = pair_id(3);
    for (i, m) in [b"a", b"b", b"c"].into_iter().enumerate() {
        send(&client, &pair, NEW, u32::try_from(i).unwrap(), m)
            .await
            .assert_status(StatusCode::OK);
    }
    let res = send(&client, &pair, NEW, 3, b"d").await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&res), codes::RELAY_PAIR_SESSION_GONE);

    receive_as(&client, BEARER, &pair, EXISTING, 0)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// The session ends 300 s after it opened, whatever state it is in, and is
/// counted as expired once.
#[tokio::test]
async fn a_session_expires_three_hundred_seconds_after_it_opened() {
    let (client, clock) = rendezvous();
    let pair = pair_id(4);
    send(&client, &pair, NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);

    clock.advance(SESSION_TTL_MS - 1);
    assert_eq!(receive(&client, &pair, EXISTING, 0).await.len(), 1);

    clock.advance(1);
    let res = receive_as(&client, BEARER, &pair, EXISTING, 0).await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&res), codes::RELAY_PAIR_SESSION_GONE);
    assert_eq!(outcomes(&client, "expired"), 1);

    let res = send(&client, &pair, EXISTING, 0, b"noise-2").await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(outcomes(&client, "expired"), 1, "counted once");
}

/// Another account can neither read a session nor write into it, and is told
/// exactly what it would be told about a pair id that never existed.
#[tokio::test]
async fn another_account_cannot_reach_a_session() {
    let (client, _) = rendezvous();
    let pair = pair_id(5);
    send(&client, &pair, NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);

    let read = receive_as(&client, SECOND_BEARER, &pair, EXISTING, 0).await;
    read.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&read), codes::RELAY_PAIR_SESSION_GONE);

    let never = receive_as(&client, SECOND_BEARER, &pair_id(99), EXISTING, 0).await;
    assert_eq!(read.status, never.status);
    assert_eq!(code_of(&read), code_of(&never));

    let write = send_as(&client, SECOND_BEARER, &pair, EXISTING, 0, b"x").await;
    write.assert_status(StatusCode::NOT_FOUND);
    let write = send_as(&client, SECOND_BEARER, &pair, NEW, 1, b"x").await;
    write.assert_status(StatusCode::NOT_FOUND);

    assert!(
        receive(&client, &pair, NEW, 0).await.is_empty(),
        "nothing the other account sent was buffered"
    );
    assert_eq!(
        receive(&client, &pair, EXISTING, 0).await,
        vec![b"noise-1".to_vec()],
        "and the session survived it"
    );
}

/// Ten pair attempts an hour per account; the eleventh is a `429` whose
/// `Retry-After` is when the first ages out, and it opens no session.
#[tokio::test]
async fn the_eleventh_pair_attempt_in_an_hour_is_refused() {
    let (client, clock) = rendezvous();
    for seed in 0..10u8 {
        send(&client, &pair_id(100 + seed), NEW, 0, b"noise-1")
            .await
            .assert_status(StatusCode::OK);
        clock.advance(1000);
    }
    let refused = send(&client, &pair_id(110), NEW, 0, b"noise-1").await;
    refused.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code_of(&refused), codes::RATE_LIMITED);
    assert_eq!(
        refused.headers["retry-after"].to_str().unwrap(),
        (3600 - 10).to_string()
    );
    assert_eq!(outcomes(&client, "rate_limited"), 1);
    receive_as(&client, BEARER, &pair_id(110), EXISTING, 0)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Messages inside an open session are not attempts.
    send(&client, &pair_id(100), NEW, 1, b"noise-3")
        .await
        .assert_status(StatusCode::OK);

    // Another account has its own budget.
    send_as(&client, SECOND_BEARER, &pair_id(111), NEW, 0, b"n")
        .await
        .assert_status(StatusCode::OK);

    clock.advance(3600 * 1000);
    send(&client, &pair_id(112), NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);
}

/// An abort drops the session for both sides, and answers the same whether or
/// not there was one.
#[tokio::test]
async fn an_abort_drops_the_session_and_reveals_nothing() {
    let (client, _) = rendezvous();
    let pair = pair_id(6);
    send(&client, &pair, NEW, 0, b"noise-1")
        .await
        .assert_status(StatusCode::OK);

    let abort = |bearer: &'static str, pair: String| {
        let client = &client;
        async move {
            client
                .send_as(
                    Method::POST,
                    "/api/v1/pairing/abort",
                    Some(bearer),
                    Some(&serde_json::json!({ "pair_id": pair, "role": EXISTING })),
                )
                .await
        }
    };
    abort(SECOND_BEARER, pair.clone())
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        receive(&client, &pair, EXISTING, 0).await.len(),
        1,
        "another account's abort touches nothing"
    );

    abort(BEARER, pair.clone())
        .await
        .assert_status(StatusCode::NO_CONTENT);
    receive_as(&client, BEARER, &pair, NEW, 0)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    abort(BEARER, pair_id(77))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(outcomes(&client, "aborted"), 1);
}

/// A malformed pair id, a message that is not base64url, an empty one and one
/// over 64 KiB are each refused before the rendezvous is touched.
#[tokio::test]
async fn malformed_requests_are_refused_without_opening_anything() {
    let (client, _) = rendezvous();
    let short = URL_SAFE_NO_PAD.encode([1u8; 8]);
    send(&client, &short, NEW, 0, b"x")
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let not_b64 = client
        .send(
            Method::POST,
            "/api/v1/pairing/send",
            Some(&serde_json::json!({
                "pair_id": pair_id(7),
                "role": NEW,
                "index": 0,
                "message": "not base64!",
            })),
        )
        .await;
    not_b64.assert_status(StatusCode::BAD_REQUEST);

    send(&client, &pair_id(7), NEW, 0, b"")
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    send(
        &client,
        &pair_id(7),
        NEW,
        0,
        &vec![0u8; MAX_PAIR_MESSAGE + 1],
    )
    .await
    .assert_status(StatusCode::BAD_REQUEST);

    send(&client, &pair_id(7), NEW, 0, &vec![0u8; MAX_PAIR_MESSAGE])
        .await
        .assert_status(StatusCode::OK);
    assert_eq!(outcomes(&client, "opened"), 1);
}

/// The rendezvous unit, below the router: the session table is bounded, and a
/// full table refuses with the wait until its first session expires.
#[test]
fn a_full_session_table_refuses_until_a_session_expires() {
    let r = super::Rendezvous::new();
    let open = |id: [u8; 16], now_ms: u64| {
        r.send(
            "acct",
            None,
            id,
            super::PairRole::NewDevice,
            0,
            vec![1],
            now_ms,
        )
    };
    for i in 0..super::MAX_SESSIONS {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&u64::try_from(i).unwrap().to_be_bytes());
        open(id, T0_MS).unwrap();
    }
    assert_eq!(r.len(), super::MAX_SESSIONS);
    assert_eq!(
        open([0xff; 16], T0_MS + 1000),
        Err(super::Refusal::Limited {
            retry_after_ms: SESSION_TTL_MS - 1000
        })
    );
    let admitted = open([0xff; 16], T0_MS + SESSION_TTL_MS).unwrap();
    assert_eq!(admitted.expired, super::MAX_SESSIONS);
    assert_eq!(r.len(), 1);
}
