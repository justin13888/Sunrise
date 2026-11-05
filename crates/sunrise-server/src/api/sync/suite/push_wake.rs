//! Push wake-ups: a publish wakes the offline peer, never the sender, and a
//! full push queue never slows a publish.

use super::*;

/// A provider that hands every intent to the test.
#[derive(Debug)]
struct Recorder(tokio::sync::mpsc::UnboundedSender<crate::push::PushIntent>);

#[async_trait::async_trait]
impl crate::push::PushProvider for Recorder {
    fn platform(&self) -> crate::push::PushPlatform {
        crate::push::PushPlatform::Apns
    }
    async fn send(&self, intent: &crate::push::PushIntent) -> Result<(), crate::push::PushError> {
        let _ = self.0.send(intent.clone());
        Ok(())
    }
}

/// A provider that never answers, so every delivery holds its slot.
#[derive(Debug)]
struct Stalled;

#[async_trait::async_trait]
impl crate::push::PushProvider for Stalled {
    fn platform(&self) -> crate::push::PushPlatform {
        crate::push::PushPlatform::Apns
    }
    async fn send(&self, _: &crate::push::PushIntent) -> Result<(), crate::push::PushError> {
        std::future::pending().await
    }
}

/// A phone and a laptop on one account, both holding an APNs token, and a
/// session the phone established.
struct PushPair {
    phone: (String, ed25519_dalek::SigningKey),
    laptop: (String, ed25519_dalek::SigningKey),
    phone_session: String,
}

async fn push_pair(client: &Client) -> PushPair {
    let phone = register_device(client, 50, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
    let laptop = register_device(client, 51, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;
    for ((id, key), token) in [(&phone, "aa01"), (&laptop, "bb02")] {
        send_signed(
            client,
            "POST",
            "/api/v1/devices/push-tokens",
            id,
            key,
            Some(&serde_json::json!({ "device_id": id, "platform": "apns", "token": token })),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    }
    let phone_session = signed_session(client, &phone).await;
    PushPair {
        phone,
        laptop,
        phone_session,
    }
}

async fn signed_session(
    client: &Client,
    (id, key): &(String, ed25519_dalek::SigningKey),
) -> String {
    let opened = send_signed(
        client,
        "POST",
        "/api/v1/sync/session",
        id,
        key,
        Some(&hello()),
    )
    .await;
    opened.assert_status(StatusCode::CREATED);
    opened.json()["session_id"].as_str().unwrap().to_owned()
}

/// The phone publishes one batch to `stream`, signed, on its session.
async fn phone_publishes(
    client: &Client,
    pair: &PushPair,
    stream: [u8; 16],
    seq: u64,
) -> StatusCode {
    let (id, key) = &pair.phone;
    let batch = serde_json::json!({
        "stream_id": hex::encode(stream),
        "batch_id": seq,
        "ops": [envelope([0xaa; 16], seq)],
    });
    send_signed_with(
        client,
        "POST",
        "/api/v1/sync/ops",
        id,
        key,
        Some(&batch),
        &[("x-sunrise-session", pair.phone_session.as_str())],
    )
    .await
    .status
}

/// Through the whole surface: a published batch wakes the peer that has no
/// stream open, once, and never the device that sent it.
#[tokio::test]
async fn a_published_batch_wakes_the_offline_peer_and_not_the_sender() {
    let (tx, mut woken) = tokio::sync::mpsc::unbounded_channel();
    let client = Client::from_state(
        ServerState::new(ServerConfig::default()).with_push_provider(Arc::new(Recorder(tx))),
    );
    let pair = push_pair(&client).await;
    assert_eq!(
        phone_publishes(&client, &pair, STREAM_BYTES, 1).await,
        StatusCode::OK
    );

    let push = tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv())
        .await
        .expect("the offline laptop is woken")
        .unwrap();
    assert_eq!(push.registration.device_id, pair.laptop.0);
    assert_eq!(push.registration.token, "bb02");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), woken.recv())
            .await
            .is_err(),
        "one batch is one push, and the phone is not woken by its own batch"
    );
}

/// A re-sent batch the relay already holds wakes nobody: only a fresh
/// append queues a wake. The window is zero, so a second wake would be a
/// second push at once rather than coalescing into the first.
#[tokio::test]
async fn a_resent_duplicate_batch_wakes_nobody() {
    let (tx, mut woken) = tokio::sync::mpsc::unbounded_channel();
    let client = Client::from_state(ServerState::new(ServerConfig::default()).with_push(
        crate::push::Dispatcher::with_tuning(
            Arc::new(Recorder(tx)),
            crate::push::Tuning {
                window_ms: 0,
                ..crate::push::Tuning::default()
            },
        ),
    ));
    let pair = push_pair(&client).await;
    for _ in 0..2 {
        assert_eq!(
            phone_publishes(&client, &pair, STREAM_BYTES, 1).await,
            StatusCode::OK
        );
    }
    assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 1);

    let push = tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv())
        .await
        .expect("the fresh batch wakes the laptop")
        .unwrap();
    assert_eq!(push.registration.device_id, pair.laptop.0);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), woken.recv())
            .await
            .is_err(),
        "the duplicate is one push in total, not two"
    );
    // The control: a fresh batch on the same stream is pushed at once
    // under this window, so the silence above is the duplicate's.
    assert_eq!(
        phone_publishes(&client, &pair, STREAM_BYTES, 2).await,
        StatusCode::OK
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv())
            .await
            .is_ok(),
        "a fresh batch after it is pushed"
    );
}

/// A device reading its event stream is sent its ops there, not a push;
/// once the stream ends it is wakeable again.
#[tokio::test]
async fn a_peer_holding_an_event_stream_is_not_woken() {
    let (tx, mut woken) = tokio::sync::mpsc::unbounded_channel();
    let client = Client::from_state(
        ServerState::new(ServerConfig::default()).with_push_provider(Arc::new(Recorder(tx))),
    );
    let pair = push_pair(&client).await;
    let laptop_session = signed_session(&client, &pair.laptop).await;
    subscribe(&client, &laptop_session, None).await;

    let (body, published) = tokio::join!(read(&client, &laptop_session, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        phone_publishes(&client, &pair, STREAM_BYTES, 1).await
    });
    assert_eq!(published, StatusCode::OK);
    assert!(
        body.contains("\"kind\":\"ops\""),
        "delivered on the stream: {body}"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), woken.recv())
            .await
            .is_err(),
        "a device with a stream open is not pushed"
    );

    assert_eq!(
        phone_publishes(&client, &pair, STREAM_BYTES, 2).await,
        StatusCode::OK
    );
    let push = tokio::time::timeout(std::time::Duration::from_secs(5), woken.recv())
        .await
        .expect("the stream ended, so the laptop is offline again")
        .unwrap();
    assert_eq!(push.registration.device_id, pair.laptop.0);
}

/// A dispatcher that cannot keep up drops wakes; the publish path neither
/// fails nor waits on it.
#[tokio::test]
async fn a_full_push_queue_never_fails_or_slows_a_publish() {
    let state =
        ServerState::new(ServerConfig::default()).with_push(crate::push::Dispatcher::with_tuning(
            Arc::new(Stalled),
            crate::push::Tuning {
                queue: 1,
                in_flight: 1,
                per_device_per_min: 1_000,
                ..crate::push::Tuning::default()
            },
        ));
    let client = Client::from_state(state);
    let pair = push_pair(&client).await;
    // Distinct streams, so no wake coalesces into another and each one
    // needs a delivery slot the stalled provider never gives back.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for s in 0..20u8 {
            assert_eq!(
                phone_publishes(&client, &pair, [s; 16], u64::from(s) + 1).await,
                StatusCode::OK
            );
        }
    })
    .await
    .expect("every publish returned while the push queue was full");
    let dropped = client.metrics.get_with(
        "sunrise_push_dispatch_total",
        &[("provider", "apns"), ("result", "dropped")],
    );
    assert!(dropped >= 1, "the full queue dropped wakes: {dropped}");
}
