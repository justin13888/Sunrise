//! A Stream routed to a second account through a viewer grant (ADR-0074):
//! the owner registers it, one other account claims it, that account's
//! session reads the owner's channel, and nothing it publishes reaches it.

use super::*;
use crate::api::testing::Res;

const ALICE: &str = "Bearer test";
const BOB: &str = "Bearer second";
const CAROL: &str = "Bearer third";

/// A relay with three accounts — alice owns, bob views, carol is a third
/// party — a clock the test moves, and a re-check fast enough to observe.
fn relay(clock: Arc<TestClock>) -> Client {
    relay_with_state(clock).0
}

/// [`relay`], keeping a handle on the state so a test can reach the store
/// under the seam.
fn relay_with_state(clock: Arc<TestClock>) -> (Client, ServerState) {
    let state = ServerState::with_clock(
        ServerConfig {
            device_recheck_ms: 20,
            ..ServerConfig::default()
        },
        clock,
    )
    .with_verifier(Arc::new(
        StaticVerifier::default()
            .with("test", Subject::new(ISSUER, "alice"))
            .with("second", Subject::new(ISSUER, "bob"))
            .with("third", Subject::new(ISSUER, "carol")),
    ));
    (Client::from_state(state.clone()), state)
}

fn token(tag: u8) -> String {
    hex::encode([tag; 32])
}

async fn create(client: &Client, bearer: &str, claim: &str, expires: Option<u64>) -> Res {
    client
        .send_as(
            Method::POST,
            "/api/v1/sync/grants",
            Some(bearer),
            Some(&serde_json::json!({
                "stream_id": stream_hex(),
                "claim": claim,
                "expires_at_ms": expires,
            })),
        )
        .await
}

async fn claim(client: &Client, bearer: &str, claim: &str) -> Res {
    client
        .send_as(
            Method::POST,
            "/api/v1/sync/grants/claim",
            Some(bearer),
            Some(&serde_json::json!({ "claim": claim })),
        )
        .await
}

async fn withdraw(client: &Client, bearer: &str, grant_id: &str) -> Res {
    client
        .send_as(
            Method::DELETE,
            &format!("/api/v1/sync/grants/{grant_id}"),
            Some(bearer),
            None,
        )
        .await
}

/// A grant alice registered and bob claimed, by id.
async fn granted(client: &Client, expires: Option<u64>) -> String {
    let made = create(client, ALICE, &token(1), expires).await;
    made.assert_status(StatusCode::CREATED);
    let held = claim(client, BOB, &token(1)).await;
    held.assert_status(StatusCode::OK);
    assert_eq!(held.json()["grant_id"], made.json()["grant_id"]);
    held.json()["grant_id"].as_str().unwrap().to_owned()
}

async fn subscribe_to(client: &Client, bearer: &str, id: &str, streams: serde_json::Value) -> Res {
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(bearer),
            Some(&serde_json::json!({ "streams": streams })),
            &[("x-sunrise-session", id)],
        )
        .await
}

fn through(grant_id: &str) -> serde_json::Value {
    serde_json::json!([{ "stream_id": stream_hex(), "grant": grant_id }])
}

fn own() -> serde_json::Value {
    serde_json::json!([{ "stream_id": stream_hex() }])
}

/// The frame a stream delivered, decoded back to the ops it carries.
fn delivered_ops(body: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter(|v| v["kind"] == "ops")
        .flat_map(|v| {
            let frame = base64::engine::general_purpose::STANDARD
                .decode(v["frame"].as_str().unwrap())
                .unwrap();
            let (_, payload) = sunrise_wire_protocol::decode_frame(&frame).unwrap();
            sunrise_wire_protocol::OpBatchPayload::decode(&payload)
                .unwrap()
                .ops
        })
        .collect()
}

fn op_bytes(envelope_b64: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(envelope_b64)
        .unwrap()
}

/// The route ADR-0074 builds: what alice publishes reaches bob's session
/// through the grant he claimed, replayed and then live.
#[tokio::test]
async fn a_viewer_reads_the_owners_stream_through_a_claimed_grant() {
    let client = relay(TestClock::at(T0_MS));
    let writer = session_as(&client, ALICE).await;
    let first = envelope([7; 16], 1);
    assert_eq!(
        publish_as(&client, ALICE, &writer, vec![first.clone()], 1).await,
        StatusCode::OK
    );

    let grant_id = granted(&client, None).await;
    let reader = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &reader, through(&grant_id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let second = envelope([7; 16], 2);
    let (body, published) = tokio::join!(read_as(&client, BOB, &reader, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        publish_as(&client, ALICE, &writer, vec![second.clone()], 2).await
    });
    assert_eq!(published, StatusCode::OK);
    assert_eq!(
        delivered_ops(&body),
        vec![op_bytes(&first), op_bytes(&second)],
        "the replayed op, then the live one: {body}"
    );
    assert!(body.contains("caught_up"), "{body}");
}

/// Without a grant, a second account naming the same Stream id reads its own
/// channel, which holds nothing of the owner's.
#[tokio::test]
async fn without_a_grant_another_account_reads_only_its_own_channel() {
    let client = relay(TestClock::at(T0_MS));
    let writer = session_as(&client, ALICE).await;
    assert_eq!(
        publish_as(&client, ALICE, &writer, vec![envelope([7; 16], 1)], 1).await,
        StatusCode::OK
    );
    let reader = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &reader, own())
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = read_as(&client, BOB, &reader, &[]).await;
    assert!(delivered_ops(&body).is_empty(), "{body}");
    assert!(body.contains("caught_up"), "{body}");
}

/// A viewer cannot write: what bob publishes under the shared Stream's id
/// lands in his own channel, and alice never sees it.
#[tokio::test]
async fn a_viewer_cannot_write_the_owners_stream() {
    let client = relay(TestClock::at(T0_MS));
    let grant_id = granted(&client, None).await;
    let viewer = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &viewer, through(&grant_id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let forged = envelope([9; 16], 1);
    assert_eq!(
        publish_as(&client, BOB, &viewer, vec![forged.clone()], 1).await,
        StatusCode::OK,
        "acked, into bob's own channel"
    );

    let owner = session_as(&client, ALICE).await;
    subscribe_to(&client, ALICE, &owner, own())
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = read_as(&client, ALICE, &owner, &[]).await;
    assert!(delivered_ops(&body).is_empty(), "alice read: {body}");

    let mine = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &mine, own())
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = read_as(&client, BOB, &mine, &[]).await;
    assert_eq!(delivered_ops(&body), vec![op_bytes(&forged)], "{body}");
}

/// A grant reads only once claimed, only for the account that claimed it,
/// and only for the Stream it names.
#[tokio::test]
async fn a_grant_serves_only_its_claimant_and_its_stream() {
    let client = relay(TestClock::at(T0_MS));
    let made = create(&client, ALICE, &token(1), None).await;
    made.assert_status(StatusCode::CREATED);
    let grant_id = made.json()["grant_id"].as_str().unwrap().to_owned();

    let bob = session_as(&client, BOB).await;
    let unclaimed = subscribe_to(&client, BOB, &bob, through(&grant_id)).await;
    unclaimed.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&unclaimed), codes::RELAY_GRANT_REVOKED);

    claim(&client, BOB, &token(1))
        .await
        .assert_status(StatusCode::OK);
    let carol = session_as(&client, CAROL).await;
    let other = subscribe_to(&client, CAROL, &carol, through(&grant_id)).await;
    other.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&other), codes::RELAY_GRANT_REVOKED);

    let elsewhere = subscribe_to(
        &client,
        BOB,
        &bob,
        serde_json::json!([{ "stream_id": "22".repeat(16), "grant": grant_id }]),
    )
    .await;
    elsewhere.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&elsewhere), codes::RELAY_GRANT_REVOKED);

    let unknown = subscribe_to(&client, BOB, &bob, through(&"ee".repeat(16))).await;
    unknown.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&unknown), codes::RELAY_GRANT_REVOKED);
}

/// The first other account to present the token is bound; the owner, a later
/// account and an unknown token all get the same `404`, and the holder is
/// answered again.
#[tokio::test]
async fn a_claim_binds_one_account() {
    let client = relay(TestClock::at(T0_MS));
    create(&client, ALICE, &token(1), None)
        .await
        .assert_status(StatusCode::CREATED);
    for (bearer, tag) in [(ALICE, 1), (BOB, 2)] {
        let refused = claim(&client, bearer, &token(tag)).await;
        refused.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(code_of(&refused), codes::RELAY_GRANT_REVOKED);
    }
    let first = claim(&client, BOB, &token(1)).await;
    first.assert_status(StatusCode::OK);
    let again = claim(&client, BOB, &token(1)).await;
    again.assert_status(StatusCode::OK);
    assert_eq!(again.json(), first.json());
    let late = claim(&client, CAROL, &token(1)).await;
    late.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&late), codes::RELAY_GRANT_REVOKED);
}

/// A registration retried with the same token answers the grant it made; a
/// token another grant holds, an expiry already past and a malformed token
/// are refused.
#[tokio::test]
async fn a_registration_is_idempotent_and_validated() {
    let client = relay(TestClock::at(T0_MS));
    let first = create(&client, ALICE, &token(1), Some(T0_MS + 5_000)).await;
    first.assert_status(StatusCode::CREATED);
    let retried = create(&client, ALICE, &token(1), Some(T0_MS + 5_000)).await;
    retried.assert_status(StatusCode::CREATED);
    assert_eq!(retried.json()["grant_id"], first.json()["grant_id"]);

    create(&client, BOB, &token(1), None)
        .await
        .assert_status(StatusCode::CONFLICT);
    create(&client, ALICE, &token(2), Some(T0_MS))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    create(&client, ALICE, &"AB".repeat(32), None)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// Each party lists the grant on its own side, and neither sees the other's
/// account.
#[tokio::test]
async fn each_party_lists_the_grant_on_its_side() {
    let client = relay(TestClock::at(T0_MS));
    let grant_id = granted(&client, Some(T0_MS + 5_000)).await;
    let list = |bearer| {
        let client = &client;
        async move {
            let res = client
                .send_as(Method::GET, "/api/v1/sync/grants", Some(bearer), None)
                .await;
            res.assert_status(StatusCode::OK);
            res.json()
        }
    };
    let alice = list(ALICE).await;
    assert_eq!(alice["owned"][0]["grant_id"], grant_id.as_str());
    assert_eq!(alice["owned"][0]["stream_id"], stream_hex().as_str());
    assert_eq!(alice["owned"][0]["expires_at_ms"], T0_MS + 5_000);
    assert_eq!(alice["owned"][0]["claimed_ms"], T0_MS);
    assert_eq!(alice["held"], serde_json::json!([]));
    let bob = list(BOB).await;
    assert_eq!(bob["held"][0]["grant_id"], grant_id.as_str());
    assert_eq!(bob["owned"], serde_json::json!([]));
    let text = bob.to_string();
    assert!(
        !text.contains("alice") && !text.contains("account"),
        "no account is named: {text}"
    );
    assert_eq!(list(CAROL).await["held"], serde_json::json!([]));
}

/// Withdrawing a grant ends a stream already reading through it, with the
/// code the catalogue reserves for it, and refuses the next subscribe.
#[tokio::test]
async fn withdrawing_a_grant_ends_an_open_stream() {
    let client = relay(TestClock::at(T0_MS));
    let grant_id = granted(&client, None).await;
    let reader = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &reader, through(&grant_id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let (body, withdrawn) = tokio::join!(read_as(&client, BOB, &reader, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        withdraw(&client, ALICE, &grant_id).await.status
    });
    assert_eq!(withdrawn, StatusCode::NO_CONTENT);
    assert!(
        body.contains("\"kind\":\"closed\"") && body.contains("RELAY_GRANT_REVOKED"),
        "the stream must close naming the grant: {body}"
    );

    let again = subscribe_to(&client, BOB, &reader, through(&grant_id)).await;
    again.assert_status(StatusCode::FORBIDDEN);
    withdraw(&client, ALICE, &grant_id)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// The recipient declines by withdrawing; a third account cannot withdraw
/// a grant it is not party to.
#[tokio::test]
async fn the_recipient_may_decline_and_a_third_account_may_not() {
    let client = relay(TestClock::at(T0_MS));
    let grant_id = granted(&client, None).await;
    let refused = withdraw(&client, CAROL, &grant_id).await;
    refused.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&refused), codes::RELAY_GRANT_REVOKED);
    withdraw(&client, BOB, &grant_id)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let reader = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &reader, through(&grant_id))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

/// A grant subscribed to and then withdrawn or expired serves no replay: the
/// stream open checks it again, since the subscribe that checked it is older.
#[tokio::test]
async fn a_stream_opened_after_the_grant_ended_is_refused() {
    let clock = TestClock::at(T0_MS);
    let client = relay(clock.clone());
    let writer = session_as(&client, ALICE).await;
    assert_eq!(
        publish_as(&client, ALICE, &writer, vec![envelope([7; 16], 1)], 1).await,
        StatusCode::OK
    );
    let grant_id = granted(&client, Some(T0_MS + 1_000)).await;
    let reader = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &reader, through(&grant_id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    clock.set(T0_MS + 1_000);
    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.send_with(
            Method::GET,
            "/api/v1/sync/events",
            Some(BOB),
            None,
            &[("x-sunrise-session", reader.as_str())],
        ),
    )
    .await
    .expect("an expired grant's stream must be refused, not served");
    refused.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&refused), codes::RELAY_GRANT_REVOKED);
    let late = claim(&client, CAROL, &token(1)).await;
    late.assert_status(StatusCode::NOT_FOUND);
}

/// One id in the event stream means one channel: a granted Stream's id may
/// not also be named as the caller's own.
#[tokio::test]
async fn a_granted_stream_id_appears_once_in_a_set() {
    let client = relay(TestClock::at(T0_MS));
    let grant_id = granted(&client, None).await;
    let reader = session_as(&client, BOB).await;
    subscribe_to(
        &client,
        BOB,
        &reader,
        serde_json::json!([
            { "stream_id": stream_hex() },
            { "stream_id": stream_hex(), "grant": grant_id },
        ]),
    )
    .await
    .assert_status(StatusCode::BAD_REQUEST);
}

/// A device registered under `bearer`'s account, with the key it signs by.
async fn device_of(client: &Client, bearer: &str, seed: u8) -> (String, ed25519_dalek::SigningKey) {
    use base64::Engine as _;
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let body = serde_json::json!({
        "device_pub_s": base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(key.verifying_key().as_bytes()),
        "nickname": "viewer phone",
        "platform": "linux",
    });
    let res = client
        .send_as(Method::POST, "/api/v1/devices", Some(bearer), Some(&body))
        .await;
    res.assert_status(StatusCode::CREATED);
    (res.json()["device_id"].as_str().unwrap().to_owned(), key)
}

/// `bearer`'s device signs a request, with `session` when it has one.
async fn signed_as(
    client: &Client,
    bearer: &str,
    device: &(String, ed25519_dalek::SigningKey),
    target: &str,
    body: &serde_json::Value,
    session: Option<&str>,
) -> Res {
    let date = crate::api::testing::now_rfc2822(client);
    let signature = sunrise_http_sig::sign(&device.1, "POST", target, &date, Some(body))
        .expect("the client half signs");
    let mut headers = vec![
        ("x-sunrise-device", device.0.as_str()),
        ("x-sunrise-device-sig", signature.as_str()),
        ("date", date.as_str()),
    ];
    if let Some(id) = session {
        headers.push(("x-sunrise-session", id));
    }
    client
        .send_with(Method::POST, target, Some(bearer), Some(body), &headers)
        .await
}

/// The cursors a viewer's device declares for a granted Stream are not its
/// acknowledgement of anything the owner's blob-tombstone quorum counts, so
/// none is recorded; the cursors it declares for its own Stream still are.
#[tokio::test]
async fn a_subscribe_through_a_grant_records_no_cursor_for_the_granted_stream() {
    let (client, state) = relay_with_state(TestClock::at(T0_MS));
    let grant_id = granted(&client, None).await;
    let device = device_of(&client, BOB, 9).await;
    let opened = signed_as(
        &client,
        BOB,
        &device,
        "/api/v1/sync/session",
        &hello(),
        None,
    )
    .await;
    opened.assert_status(StatusCode::CREATED);
    let session = opened.json()["session_id"].as_str().unwrap().to_owned();
    let origin = hex::encode([0x22; 16]);
    let own_stream = hex::encode([0x33; 16]);
    let body = serde_json::json!({ "streams": [
        {
            "stream_id": stream_hex(),
            "grant": grant_id,
            "cursors": [{ "device_id": origin, "last_applied_seq": 4 }],
        },
        {
            "stream_id": own_stream,
            "cursors": [{ "device_id": origin, "last_applied_seq": 6 }],
        },
    ]});
    signed_as(
        &client,
        BOB,
        &device,
        "/api/v1/sync/subscribe",
        &body,
        Some(&session),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    let streams: Vec<Vec<u8>> = {
        let conn = state.store.as_sqlite().expect("SQLite").conn.lock();
        let mut stmt = conn
            .prepare("SELECT stream_id FROM device_cursors WHERE device_id = ?1")
            .unwrap();
        stmt.query_map([&device.0], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        streams,
        vec![vec![0x33u8; 16]],
        "only the device's own stream's cursor may be recorded"
    );
}

/// A relay that cannot read its grants does not know the grant is revoked,
/// so it ends the stream with the retryable storage code rather than
/// `RELAY_GRANT_REVOKED`.
#[tokio::test]
async fn an_unreadable_grant_store_ends_the_stream_retryably() {
    let (client, state) = relay_with_state(TestClock::at(T0_MS));
    let grant_id = granted(&client, None).await;
    let reader = session_as(&client, BOB).await;
    subscribe_to(&client, BOB, &reader, through(&grant_id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let (body, ()) = tokio::join!(read_as(&client, BOB, &reader, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        state
            .store
            .as_sqlite()
            .expect("SQLite")
            .conn
            .lock()
            .execute_batch("DROP TABLE viewer_grants")
            .unwrap();
    });
    assert!(
        body.contains("\"kind\":\"closed\"")
            && body.contains("RELAY_STORAGE_UNAVAILABLE")
            && !body.contains("RELAY_GRANT_REVOKED"),
        "an unreadable grant store must close retryably, not as revoked: {body}"
    );
}

/// A fresh batch the owner publishes wakes the offline device of the account
/// holding a grant in force on the Stream, and nobody else's; once the grant
/// is withdrawn, the next batch wakes it no more. The window is zero, so a
/// wake the withdrawal failed to stop would be a push at once.
#[tokio::test]
async fn a_publish_wakes_the_viewer_while_its_grant_is_in_force() {
    let (tx, mut woken) = tokio::sync::mpsc::unbounded_channel();
    let (_, state) = relay_with_state(TestClock::at(T0_MS));
    let client = Client::from_state(state.with_push(crate::push::Dispatcher::with_tuning(
        Arc::new(super::push_wake::Recorder(tx)),
        crate::push::Tuning {
            window_ms: 0,
            ..crate::push::Tuning::default()
        },
    )));
    let grant_id = granted(&client, None).await;
    let viewer = device_of(&client, BOB, 9).await;
    let bystander = device_of(&client, CAROL, 10).await;
    for (bearer, device, token) in [(BOB, &viewer, "cc03"), (CAROL, &bystander, "dd04")] {
        signed_as(
            &client,
            bearer,
            device,
            "/api/v1/devices/push-tokens",
            &serde_json::json!({ "device_id": device.0, "platform": "apns", "token": token }),
            None,
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    }

    let writer = session_as(&client, ALICE).await;
    assert_eq!(
        publish_as(&client, ALICE, &writer, vec![envelope([7; 16], 1)], 1).await,
        StatusCode::OK
    );
    let push = tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv())
        .await
        .expect("the viewer's offline device is woken")
        .unwrap();
    assert_eq!(push.registration.device_id, viewer.0);
    assert_eq!(push.registration.token, "cc03");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), woken.recv())
            .await
            .is_err(),
        "one batch is one push, and an account with no grant is not woken"
    );

    withdraw(&client, ALICE, &grant_id)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        publish_as(&client, ALICE, &writer, vec![envelope([7; 16], 2)], 2).await,
        StatusCode::OK
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), woken.recv())
            .await
            .is_err(),
        "a withdrawn grant wakes nobody"
    );
}
