//! The ported WebSocket suite, a unit test of the whole sync surface.
//!
//! It drives all five operations over HTTP through `crate::api::testing`, which
//! is `#[cfg(test)] pub(crate)` and therefore reachable only from inside the
//! crate — so this is a unit test in its own file rather than anything under
//! `tests/`. It covers every half of this module at once, which is why it sits
//! beside them instead of inside one of them.

#[cfg(test)]
mod tests {
    //! The WebSocket suite, carried over.
    //!
    //! Every scenario the five `ws_*` integration files covered lives here,
    //! against the operations that replaced the frames. They are unit tests now
    //! rather than integration ones because `Service::call` needs no port: what
    //! used to require booting a listener and dialling it is an in-process call,
    //! so the same coverage costs no sockets and no timing.
    //!
    //! Three scenarios changed shape rather than moving, and each says so where
    //! it sits: `Ping` is a keep-alive comment now, a malformed batch is refused
    //! by the schema rather than nacked, and the handshake round trip is an HTTP
    //! response rather than a frame exchange.

    use crate::api::error::codes;
    use crate::api::testing::{register_device, send_signed, send_signed_with, Client, BEARER};
    use crate::relay::RingCaps;
    use crate::relay_log::DurableCaps;
    use crate::state::{Clock, ServerState};
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use sunrise_cbor::magic::{write_prefix, MagicKind, MAGIC_LEN};
    use sunrise_cbor::version::{ENVELOPE_FORMAT_V, WIRE_PROTO_V};
    use sunrise_wire_protocol::{Capability, CapabilityBits};

    const STREAM_BYTES: [u8; 16] = [0x11; 16];
    const ISSUER: &str = "https://idp.example";
    const T0_MS: u64 = 1_704_067_200_000;

    fn stream_hex() -> String {
        hex::encode(STREAM_BYTES)
    }

    /// A clock a test moves by hand, so expiry is a value rather than a wait.
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

    fn hello() -> serde_json::Value {
        serde_json::json!({
            "client_app_v": "0.1.0",
            "client_platform": "test",
            "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
            "doc_schema_min": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
            "doc_schema_max": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
            "crypto_suite_supported": [u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)],
            // `SrvTokenRefresh` is agreed by AND, so a client that wants a
            // refresh has to offer it too. A real one does; this mirrors that.
            "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0
                | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
            "trace": "01J000000000000000000000000",
        })
    }

    async fn session_as(client: &Client, bearer: &str) -> String {
        let res = client
            .send_as(
                Method::POST,
                "/api/v1/sync/session",
                Some(bearer),
                Some(&hello()),
            )
            .await;
        res.assert_status(StatusCode::CREATED);
        res.json()["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned()
    }

    async fn establish(client: &Client) -> String {
        session_as(client, BEARER).await
    }

    async fn subscribe_as(
        client: &Client,
        bearer: &str,
        id: &str,
        cursor: Option<(String, u64)>,
    ) -> StatusCode {
        let cursors = cursor.map_or_else(Vec::new, |(device_id, last_applied_seq)| {
            vec![serde_json::json!({
                "device_id": device_id,
                "last_applied_seq": last_applied_seq,
            })]
        });
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(bearer),
                Some(&serde_json::json!({
                    "streams": [{ "stream_id": stream_hex(), "cursors": cursors }]
                })),
                &[("x-sunrise-session", id)],
            )
            .await
            .status
    }

    async fn subscribe(client: &Client, id: &str, cursor: Option<(String, u64)>) {
        assert_eq!(
            subscribe_as(client, BEARER, id, cursor).await,
            StatusCode::NO_CONTENT
        );
    }

    /// One op envelope from `device` at `seq`.
    ///
    /// Header-shaped rather than a real sealed envelope: that is exactly the
    /// surface the relay reads — `stream_id`, `device_id`, `seq`, all cleartext
    /// — and building real ones would put `sunrise-crypto`, the crate holding
    /// the keys a relay must not have, into the server's test graph.
    fn envelope(device: [u8; 16], seq: u64) -> String {
        use base64::Engine as _;
        use ciborium::value::Value;
        let entries: Vec<(Value, Value)> = vec![
            (Value::Integer(1.into()), Value::Integer(3.into())),
            (
                Value::Integer(2.into()),
                Value::Bytes(STREAM_BYTES.to_vec()),
            ),
            (Value::Integer(3.into()), Value::Bytes(device.to_vec())),
            (Value::Integer(4.into()), Value::Integer(seq.into())),
            (
                Value::Integer(10.into()),
                Value::Bytes(vec![u8::try_from(seq & 0xff).unwrap(); 32]),
            ),
        ];
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_V);
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    async fn publish_as(
        client: &Client,
        bearer: &str,
        id: &str,
        ops: Vec<String>,
        batch_id: u64,
    ) -> StatusCode {
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(bearer),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(),
                    "batch_id": batch_id,
                    "ops": ops,
                })),
                &[("x-sunrise-session", id)],
            )
            .await
            .status
    }

    async fn publish(client: &Client, id: &str, ops: Vec<String>, batch_id: u64) -> StatusCode {
        publish_as(client, BEARER, id, ops, batch_id).await
    }

    /// [`publish`], keeping the `Ack` body — the dedup tests assert on
    /// `server_first_seen_ms`, which a status code cannot carry.
    async fn publish_acked(
        client: &Client,
        id: &str,
        ops: Vec<String>,
        batch_id: u64,
    ) -> serde_json::Value {
        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(),
                    "batch_id": batch_id,
                    "ops": ops,
                })),
                &[("x-sunrise-session", id)],
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()
    }

    async fn read_as(client: &Client, bearer: &str, id: &str, extra: &[(&str, &str)]) -> String {
        let mut headers = vec![("x-sunrise-session", id), ("authorization", bearer)];
        headers.extend_from_slice(extra);
        client
            .read_stream(
                "/api/v1/sync/events",
                &headers,
                std::time::Duration::from_millis(250),
            )
            .await
    }

    async fn read(client: &Client, id: &str, extra: &[(&str, &str)]) -> String {
        read_as(client, BEARER, id, extra).await
    }

    /// `GET /sync/events`, expecting a refusal rather than a stream.
    ///
    /// [`Client::send_with`] collects the whole body, and a stream that is
    /// *not* refused never ends — so the call is bounded. A timeout here is
    /// the assertion failing: the server served a request it was meant to
    /// refuse.
    async fn open_events(
        client: &Client,
        id: &str,
        extra: &[(&str, &str)],
    ) -> crate::api::testing::Res {
        let mut headers = vec![("x-sunrise-session", id)];
        headers.extend_from_slice(extra);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.send_with(
                Method::GET,
                "/api/v1/sync/events",
                Some(BEARER),
                None,
                &headers,
            ),
        )
        .await
        .expect("the stream had to be refused; a served stream never ends")
    }

    // -- ws_auth ------------------------------------------------------------

    /// Was `unauthenticated_upgrade_is_refused`.
    ///
    /// Against a verifier that actually checks, as the socket test's own relay
    /// did. `NullVerifier` accepts an absent header by design — that is what
    /// makes self-host work and what makes enabling authentication purely a
    /// matter of configuring a verifier.
    #[tokio::test]
    async fn an_unauthenticated_session_is_refused() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
        ));
        let client = Client::from_state(state);
        client
            .send_as(Method::POST, "/api/v1/sync/session", None, Some(&hello()))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Was `unknown_bearer_is_refused`.
    #[tokio::test]
    async fn an_unknown_bearer_is_refused() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
        ));
        let client = Client::from_state(state);

        client
            .send_as(
                Method::POST,
                "/api/v1/sync/session",
                Some("Bearer nope"),
                Some(&hello()),
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Was `valid_bearer_is_accepted`.
    #[tokio::test]
    async fn a_valid_bearer_establishes_a_session() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
        ));
        let client = Client::from_state(state);
        let _ = session_as(&client, "Bearer good").await;
    }

    /// Was `one_tenant_never_receives_another_tenants_frames`.
    ///
    /// The channel key is `(account_hash, stream_id)` and the account half comes
    /// from the verified token, never from anything the client sends — so naming
    /// another tenant's stream id reaches a different channel entirely.
    #[tokio::test]
    async fn one_tenant_never_receives_another_tenants_frames() {
        let verifier = StaticVerifier::default()
            .with("alice", Subject::new(ISSUER, "alice"))
            .with("bob", Subject::new(ISSUER, "bob"));
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);

        let alice = session_as(&client, "Bearer alice").await;
        let bob = session_as(&client, "Bearer bob").await;

        assert_eq!(
            subscribe_as(&client, "Bearer bob", &bob, None).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            publish_as(&client, "Bearer alice", &alice, vec![], 1).await,
            StatusCode::OK
        );

        let body = read_as(&client, "Bearer bob", &bob, &[]).await;
        assert!(
            !body.contains("\"kind\":\"ops\""),
            "another tenant's frame reached this stream: {body}"
        );
        assert!(
            body.contains("\"kind\":\"caught_up\""),
            "the stream must still open and report its own emptiness: {body}"
        );
    }

    /// Was `two_sessions_of_one_tenant_still_fan_out`: the isolation above is
    /// per account, not per session.
    #[tokio::test]
    async fn two_sessions_of_one_tenant_both_receive() {
        let client = Client::new(ServerConfig::default());
        let publisher = establish(&client).await;
        let reader = establish(&client).await;

        subscribe(&client, &reader, None).await;
        assert_eq!(
            publish(&client, &publisher, vec![], 1).await,
            StatusCode::OK
        );

        let body = read(&client, &reader, &[]).await;
        assert!(
            body.contains("\"kind\":\"ops\""),
            "a sibling session's batch must fan out: {body}"
        );
    }

    // -- ws_handshake -------------------------------------------------------

    /// Was `ws_handshake_round_trip`.
    #[tokio::test]
    async fn a_session_negotiates_the_same_versions_the_handshake_did() {
        let client = Client::new(ServerConfig::default());
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
            .await;
        res.assert_status(StatusCode::CREATED);
        let body = res.json();

        assert_eq!(body["wire_proto"], u32::from(WIRE_PROTO_V));
        assert_eq!(
            body["crypto_suite"],
            u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)
        );
        assert!(
            body["session_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("ses_")),
            "a session id must come back: {body}"
        );
    }

    /// A client the server cannot agree with is refused for the same reason it
    /// was refused at the socket.
    #[tokio::test]
    async fn an_unnegotiable_client_is_refused() {
        let client = Client::new(ServerConfig::default());
        let mut body = hello();
        body["wire_proto_supported"] = serde_json::json!([9999]);

        client
            .send(Method::POST, "/api/v1/sync/session", Some(&body))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// The four negotiation failures are four different user-facing outcomes,
    /// and every one of them used to arrive as `VALIDATION_INVALID` with the
    /// distinction only in a message string. `as_error_code` had always
    /// computed it; nothing called it. Telling a self-hoster to update their
    /// app when their *relay* is the stale half is the concrete failure.
    #[tokio::test]
    async fn every_negotiation_refusal_carries_its_own_code() {
        let client = Client::new(ServerConfig::default());
        let cases: [(&str, serde_json::Value, &str); 4] = [
            (
                "wire_proto_supported",
                serde_json::json!([9999]),
                "SYNC_PROTOCOL_VERSION_MISMATCH",
            ),
            (
                "crypto_suite_supported",
                serde_json::json!([9999]),
                "CRYPTO_SUITE_MISMATCH",
            ),
            // The server's floor is 1, so a client that can read nothing at
            // all is the device whose data predates what this relay accepts.
            ("doc_schema_max", serde_json::json!(0), "DOC_SCHEMA_TOO_OLD"),
            (
                "capabilities",
                serde_json::json!(0),
                "CAPABILITY_REQUIRED_MISSING",
            ),
        ];

        for (field, value, expected) in cases {
            let mut body = hello();
            body[field] = value;
            let res = client
                .send(Method::POST, "/api/v1/sync/session", Some(&body))
                .await;
            res.assert_status(StatusCode::BAD_REQUEST);
            assert_eq!(
                res.json()["code"],
                serde_json::json!(expected),
                "a refusal on {field} must carry {expected}, not a shared code"
            );
        }
    }

    /// Was `ws_subscribe_receives_op_batch_fanout`.
    #[tokio::test]
    async fn a_subscribed_stream_receives_a_published_batch() {
        let client = Client::new(ServerConfig::default());
        let reader = establish(&client).await;
        let writer = establish(&client).await;
        subscribe(&client, &reader, None).await;

        assert_eq!(publish(&client, &writer, vec![], 1).await, StatusCode::OK);

        let body = read(&client, &reader, &[]).await;
        assert!(body.contains("\"kind\":\"ops\""), "no fan-out in: {body}");
    }

    /// Was `ws_malformed_op_batch_nacked_no_fanout`.
    ///
    /// Shape changed: a `Nack` frame becomes a refusal status, and the body is
    /// caught before the handler rather than after a decode.
    ///
    /// The exact status and code rather than `is_client_error`: the loose form
    /// was satisfied by any 4xx, including the 401 an unauthenticated request
    /// produces and the 422 a schema rejection produces — neither of which is
    /// this refusal, and both of which would have hidden the handler no longer
    /// decoding the ops at all.
    #[tokio::test]
    async fn a_malformed_batch_is_refused_and_never_fans_out() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(),
                    "batch_id": 1,
                    "ops": ["not base64!!"],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            res.json()["code"],
            serde_json::json!(codes::VALIDATION_INVALID)
        );

        let body = read(&client, &id, &[]).await;
        assert!(
            !body.contains("\"kind\":\"ops\""),
            "a refused batch must not fan out: {body}"
        );
    }

    // -- ws_cursors ---------------------------------------------------------

    /// Was `no_cursor_still_replays_the_whole_ring`.
    #[tokio::test]
    async fn no_cursor_replays_everything_retained() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &id, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            3,
            "every retained frame must replay: {body}"
        );
    }

    /// Was `a_cursor_narrows_the_replay_to_what_the_subscriber_missed`.
    #[tokio::test]
    async fn a_cursor_narrows_the_replay_to_what_was_missed() {
        let client = Client::new(ServerConfig::default());
        let writer = establish(&client).await;
        let device = [7u8; 16];

        for seq in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![envelope(device, seq)], seq).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 2))).await;

        let body = read(&client, &reader, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            1,
            "only what the cursor does not cover may replay: {body}"
        );
    }

    /// Was `eviction_of_already_applied_ops_is_not_a_gap`.
    #[tokio::test]
    async fn a_cursor_at_the_head_replays_nothing_and_reports_no_gap() {
        let client = Client::new(ServerConfig::default());
        let writer = establish(&client).await;
        let device = [8u8; 16];
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, 1)], 1).await,
            StatusCode::OK
        );

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 1))).await;

        let body = read(&client, &reader, &[]).await;
        assert!(
            !body.contains("\"kind\":\"ops\""),
            "nothing was missed: {body}"
        );
        assert!(
            !body.contains("\"kind\":\"gap\""),
            "an applied op falling out of retention is not a loss: {body}"
        );
        assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
    }

    /// Was `a_cursor_past_durable_retention_gets_a_typed_gap_not_silence`.
    ///
    /// The load-bearing half is the ordering: the gap precedes the partial
    /// replay, so a client cannot read `caught_up` as `complete`.
    #[tokio::test]
    async fn a_cursor_past_retention_gets_a_typed_gap_before_the_replay() {
        let state = ServerState::new(ServerConfig::default())
            .with_ring_caps(RingCaps {
                max_frames: 1,
                max_bytes: 1 << 20,
            })
            .with_durable_caps(DurableCaps {
                max_bytes: 1,
                max_age_ms: u64::MAX,
            });
        let client = Client::from_state(state);
        let writer = establish(&client).await;
        let device = [9u8; 16];

        for seq in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![envelope(device, seq)], seq).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 0))).await;

        let body = read(&client, &reader, &[]).await;
        assert!(
            body.contains("\"kind\":\"gap\""),
            "eviction past a cursor must be reported, not passed over: {body}"
        );
        let gap_at = body.find("\"kind\":\"gap\"").expect("a gap");
        let caught_at = body.find("\"kind\":\"caught_up\"").expect("a marker");
        assert!(
            gap_at < caught_at,
            "a gap must precede the caught-up it qualifies: {body}"
        );
    }

    /// Was `resubscribing_replaces_the_receiver_rather_than_duplicating_it`.
    ///
    /// The fan-out assertion first, because it is the fact a client can
    /// observe. `spawn_stream` walks `session.streams` and replays each entry,
    /// so a set that grew on the second `Subscribe` delivers the same batch
    /// once per entry — a subscriber re-applying every op it is sent twice.
    /// Only the stored-set length was ever asserted, read out of the session
    /// store through the harness, so the behaviour itself was unpinned and the
    /// test would fail for a reason that is not a regression the moment that
    /// field moves.
    #[tokio::test]
    async fn resubscribing_replaces_the_stream_set() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        subscribe(&client, &id, None).await;
        subscribe(&client, &id, None).await;

        assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);
        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            1,
            "one publish must reach a twice-subscribed session once: {body}"
        );

        let session = client
            .sessions
            .get(&id, client.clock_now_ms())
            .expect("a live session");
        assert_eq!(
            session.streams.len(),
            1,
            "a second subscribe replaces rather than duplicating"
        );
    }

    /// Was `a_cursor_past_the_ring_bound_is_served_from_the_durable_log` and
    /// `ops_survive_a_relay_restart`.
    ///
    /// The in-memory ring is bounded to one frame, so anything replayed past
    /// that came off the disk — which is the property a restart relies on.
    #[tokio::test]
    async fn a_replay_past_the_ring_bound_is_served_from_the_durable_log() {
        let state = ServerState::new(ServerConfig::default()).with_ring_caps(RingCaps {
            max_frames: 1,
            max_bytes: 1 << 20,
        });
        let client = Client::from_state(state);
        let writer = establish(&client).await;

        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, None).await;

        let body = read(&client, &reader, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            3,
            "the durable log, not the ring, is the authority for replay: {body}"
        );
    }

    // -- ws_device_binding --------------------------------------------------

    /// Was `an_unregistered_device_cannot_open_a_sync_session`.
    ///
    /// It used to claim it also covered `a_revoked_device_cannot_open_a_sync_session`
    /// — a test that has never existed in this tree (`#81`). The claim was
    /// wrong on its own terms as well as dangling: this device id was never
    /// registered, so the lookup misses for a reason a revoked device's does
    /// not share. The revoked case is
    /// [`a_revoked_device_cannot_open_a_session`] below, and it revokes a
    /// device that really did register.
    #[tokio::test]
    async fn an_unregistered_device_cannot_open_a_session() {
        let client = Client::new(ServerConfig::default());
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session",
                Some(BEARER),
                Some(&hello()),
                &[
                    ("x-sunrise-device", "01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
                    ("x-sunrise-device-sig", "AAAA"),
                    ("date", "Mon, 01 Jan 2024 00:00:00 GMT"),
                ],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// A vault id for the device under test, and one for the device revoking
    /// it. Crockford base-32 of 16 bytes, the way a `device_revoke` names one.
    const SESSION_PHONE_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
    /// The revoking device's.
    const SESSION_LAPTOP_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";

    /// The test the comment above claimed for four months and nobody wrote.
    ///
    /// A device that registered, signed, and was then revoked cannot open a
    /// sync session: `verify_bytes` resolves it through `active_device`, whose
    /// SQL ends `AND revoked = 0`, so it resolves to no device and the request
    /// is refused as a credential failure rather than as a forbidden action.
    #[tokio::test]
    async fn a_revoked_device_cannot_open_a_session() {
        let client = Client::new(ServerConfig::default());
        let (phone, phone_key) =
            register_device(&client, 40, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
        let (laptop, laptop_key) =
            register_device(&client, 41, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;

        // While it is active, the phone opens a session.
        send_signed(
            &client,
            "POST",
            "/api/v1/sync/session",
            &phone,
            &phone_key,
            Some(&hello()),
        )
        .await
        .assert_status(StatusCode::CREATED);

        send_signed(
            &client,
            "DELETE",
            &format!("/api/v1/devices/by-vault-id/{SESSION_PHONE_VAULT_ID}"),
            &laptop,
            &laptop_key,
            None,
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);

        send_signed(
            &client,
            "POST",
            "/api/v1/sync/session",
            &phone,
            &phone_key,
            Some(&hello()),
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// A revoked device cannot publish, on a session it already holds.
    ///
    /// This is the half that matters for `#80`: the session survives the
    /// revocation as a row, and the upload is still refused, because every
    /// `Signed<…>` route re-resolves the device on every request rather than
    /// trusting what establishment recorded.
    #[tokio::test]
    async fn a_revoked_devices_signed_upload_is_refused() {
        let client = Client::new(ServerConfig::default());
        let (phone, phone_key) =
            register_device(&client, 42, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
        let (laptop, laptop_key) =
            register_device(&client, 43, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;

        let opened = send_signed(
            &client,
            "POST",
            "/api/v1/sync/session",
            &phone,
            &phone_key,
            Some(&hello()),
        )
        .await;
        opened.assert_status(StatusCode::CREATED);
        let session = opened.json()["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned();

        let batch = serde_json::json!({
            "stream_id": stream_hex(),
            "batch_id": 1,
            "ops": [envelope([0xaa; 16], 1)],
        });
        send_signed_with(
            &client,
            "POST",
            "/api/v1/sync/ops",
            &phone,
            &phone_key,
            Some(&batch),
            &[("x-sunrise-session", session.as_str())],
        )
        .await
        .assert_status(StatusCode::OK);

        send_signed(
            &client,
            "DELETE",
            &format!("/api/v1/devices/by-vault-id/{SESSION_PHONE_VAULT_ID}"),
            &laptop,
            &laptop_key,
            None,
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);

        let batch = serde_json::json!({
            "stream_id": stream_hex(),
            "batch_id": 2,
            "ops": [envelope([0xaa; 16], 2)],
        });
        send_signed_with(
            &client,
            "POST",
            "/api/v1/sync/ops",
            &phone,
            &phone_key,
            Some(&batch),
            &[("x-sunrise-session", session.as_str())],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Revoking a device ends the stream it is already holding.
    ///
    /// `device_recheck_ms` is described in `config.rs` as "the bound on how
    /// long a revoked device keeps receiving fan-out on a socket it already
    /// holds", which is a security bound that had no test behind it (`#81`).
    /// It is driven down here rather than waited out: the production default
    /// is 30 s.
    #[tokio::test]
    async fn an_open_stream_is_torn_down_when_its_device_is_revoked() {
        let client = Client::new(ServerConfig {
            device_recheck_ms: 20,
            ..ServerConfig::default()
        });
        let (phone, phone_key) =
            register_device(&client, 44, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
        let (laptop, laptop_key) =
            register_device(&client, 45, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;

        let opened = send_signed(
            &client,
            "POST",
            "/api/v1/sync/session",
            &phone,
            &phone_key,
            Some(&hello()),
        )
        .await;
        opened.assert_status(StatusCode::CREATED);
        let session = opened.json()["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned();
        subscribe(&client, &session, None).await;

        // The revocation lands while the stream is open, which is the case the
        // recheck exists for — refusing the *next* connection would not end
        // this one.
        let (body, revoked) = tokio::join!(read_as(&client, BEARER, &session, &[]), async {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            send_signed(
                &client,
                "DELETE",
                &format!("/api/v1/devices/by-vault-id/{SESSION_PHONE_VAULT_ID}"),
                &laptop,
                &laptop_key,
                None,
            )
            .await
            .status
        });
        assert_eq!(revoked, StatusCode::NO_CONTENT);
        assert!(
            body.contains("AUTH_DEVICE_REVOKED"),
            "the stream must close naming the revocation, not merely stop: {body}"
        );
    }

    // -- ws_token_expiry ----------------------------------------------------

    /// Was `the_relay_advertises_the_token_refresh_capability`.
    #[tokio::test]
    async fn the_relay_advertises_the_token_refresh_capability() {
        let client = Client::new(ServerConfig::default());
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
            .await;
        res.assert_status(StatusCode::CREATED);

        let agreed = res.json()["capabilities"].as_u64().expect("a bitfield");
        assert!(
            CapabilityBits(agreed).has(Capability::SrvTokenRefresh),
            "a client only asks for a refresh if it sees this bit come back"
        );
    }

    /// Was `an_idle_session_is_closed_when_its_token_expires` and
    /// `an_inbound_frame_on_an_expired_token_ends_the_session`.
    ///
    /// One test now, because the difference between them is gone: every
    /// operation resolves the session, and an expired one resolves to nothing
    /// whether or not anything arrived.
    #[tokio::test]
    async fn an_expired_token_ends_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default().with_expiring(
            "good",
            Subject::new(ISSUER, "alice"),
            T0_MS + 60_000,
        );
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer good").await;

        clock.set(T0_MS + 60_001);
        assert_eq!(
            subscribe_as(&client, "Bearer good", &id, None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Was `a_session_with_no_deadline_is_never_closed`.
    #[tokio::test]
    async fn a_session_with_no_deadline_is_never_closed() {
        let clock = TestClock::at(T0_MS);
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
        let client = Client::from_state(state);
        let id = establish(&client).await;

        clock.set(T0_MS + 10_000_000);
        assert_eq!(
            subscribe_as(&client, BEARER, &id, None).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Was `a_refresh_extends_the_session_without_a_reconnect`.
    #[tokio::test]
    async fn a_refresh_extends_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default()
            .with_expiring("first", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
            .with_expiring("second", Subject::new(ISSUER, "alice"), T0_MS + 600_000);
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer first").await;

        let refreshed = client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer first"),
                Some(&serde_json::json!({ "token": "second" })),
                &[("x-sunrise-session", &id)],
            )
            .await;
        refreshed.assert_status(StatusCode::OK);
        assert_eq!(refreshed.json()["expires_at_ms"], T0_MS + 600_000);

        // Past the first deadline, the session is still live on the second.
        clock.set(T0_MS + 60_001);
        assert_eq!(
            subscribe_as(&client, "Bearer second", &id, None).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Was `a_refresh_with_an_unverifiable_token_is_refused_but_keeps_the_session`.
    #[tokio::test]
    async fn an_unverifiable_refresh_is_refused_but_keeps_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default().with_expiring(
            "good",
            Subject::new(ISSUER, "alice"),
            T0_MS + 60_000,
        );
        let state = ServerState::with_clock(ServerConfig::default(), clock)
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer good").await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer good"),
                Some(&serde_json::json!({ "token": "garbage" })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);

        // Still running on the credential it already had.
        assert_eq!(
            subscribe_as(&client, "Bearer good", &id, None).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Was `a_refresh_naming_another_principal_ends_the_session`.
    ///
    /// The channel namespace was fixed at establishment and is never
    /// re-derived, so a token naming someone else must not be allowed to keep
    /// reading this account's stream.
    #[tokio::test]
    async fn a_refresh_naming_another_principal_ends_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default()
            .with_expiring("alice", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
            .with_expiring("bob", Subject::new(ISSUER, "bob"), T0_MS + 600_000);
        let state = ServerState::with_clock(ServerConfig::default(), clock)
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer alice").await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer alice"),
                Some(&serde_json::json!({ "token": "bob" })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);

        assert!(
            client.sessions.get(&id, T0_MS).is_none(),
            "the session must be gone, not merely refused"
        );
    }

    /// The other half of the refresh identity check, and the half nothing
    /// reached.
    ///
    /// [`a_refresh_naming_another_principal_ends_the_session`] uses two
    /// different `sub`s, so `principal_key()` alone decides it and the
    /// `device_id` clause beside it never has to be the reason. Here the
    /// principal is identical and only the token's own
    /// `https://sunrise.app/device_id` claim differs: one device must not be
    /// able to hand its token to a session another device opened and keep that
    /// session's stream alive.
    #[tokio::test]
    async fn a_refresh_naming_another_device_ends_the_session() {
        let clock = TestClock::at(T0_MS);
        let alice = Subject::new(ISSUER, "alice");
        let verifier = StaticVerifier::default()
            .with_device_id("phone", alice.clone(), "dev_phone")
            .with_device_id("phone-renewed", alice.clone(), "dev_phone")
            .with_device_id("laptop", alice, "dev_laptop");
        let state = ServerState::with_clock(ServerConfig::default(), clock)
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer phone").await;

        // The control: the same principal and the same device claim renews.
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer phone"),
                Some(&serde_json::json!({ "token": "phone-renewed" })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::OK);

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer phone"),
                Some(&serde_json::json!({ "token": "laptop" })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);

        assert!(
            client.sessions.get(&id, T0_MS).is_none(),
            "the session must be gone, not merely refused"
        );
    }

    /// Was `an_ack_for_a_token_with_no_expiry_reports_zero`.
    #[tokio::test]
    async fn a_refresh_for_a_token_with_no_expiry_reports_zero() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some(BEARER),
                Some(&serde_json::json!({ "token": "anything" })),
                &[("x-sunrise-session", &id)],
            )
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(
            res.json()["expires_at_ms"],
            0,
            "the self-host verifier issues no deadline"
        );
    }

    /// Was `a_refresh_with_an_undecodable_payload_is_refused`.
    #[tokio::test]
    async fn a_refresh_with_an_undecodable_body_is_refused() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some(BEARER),
                Some(&serde_json::json!({ "not_a_token": 1 })),
                &[("x-sunrise-session", &id)],
            )
            .await;
        // kynos's, not this surface's: a body that is JSON but does not match
        // the declared schema never reaches the handler, so `RefreshRequest`'s
        // `deny_unknown_fields` and its missing `token` are refused at the
        // extractor. `is_client_error` could not tell that from the handler
        // accepting the body and failing later.
        res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // -- the surface's own invariants ---------------------------------------

    /// Every operation after establishment needs the session id.
    #[tokio::test]
    async fn an_operation_without_a_session_header_is_refused() {
        let client = Client::new(ServerConfig::default());
        let _ = establish(&client).await;

        client
            .send(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(&serde_json::json!({ "streams": [] })),
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// A session id is a bearer-equivalent, so it is checked against the caller
    /// rather than merely looked up.
    #[tokio::test]
    async fn an_unknown_session_is_refused() {
        let client = Client::new(ServerConfig::default());
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(BEARER),
                Some(&serde_json::json!({ "streams": [] })),
                &[("x-sunrise-session", "ses_00000000000000000000000000000000")],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Durable first, then the ack.
    #[tokio::test]
    async fn a_batch_is_acked_after_it_is_durable() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(), "batch_id": 7, "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await;

        res.assert_status(StatusCode::OK);
        let body = res.json();
        assert_eq!(body["batch_id"], 7);
        assert!(body["server_first_seen_ms"].is_u64(), "{body}");
    }

    /// A stream id that is not 32 hex characters never reaches the relay.
    #[tokio::test]
    async fn a_malformed_stream_id_is_refused() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": "not-a-stream", "batch_id": 1, "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// The replay, then the marker that completes it, each frame carrying the
    /// durable id `Last-Event-ID` resumes from.
    #[tokio::test]
    async fn the_event_stream_replays_then_reports_caught_up() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

        let body = read(&client, &id, &[]).await;
        assert!(body.contains("\"kind\":\"ops\""), "no replay in: {body}");
        assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
        let ops_at = body.find("\"kind\":\"ops\"").expect("a replay");
        let caught_at = body.find("\"kind\":\"caught_up\"").expect("a marker");
        assert!(ops_at < caught_at, "{body}");
        assert!(body.contains("id: 1"), "{body}");
    }

    /// `Last-Event-ID` resumes rather than replaying from the start.
    ///
    /// The stream that serves the subscription comes first, because that is
    /// where the ids come from: a client cannot hold a `Last-Event-ID` it was
    /// never sent, and a resume on a stream that follows another stream is
    /// exactly what `SseTransport` does. The test used to resume straight off
    /// the `Subscribe`, which is the shape the server now refuses.
    #[tokio::test]
    async fn a_resumed_stream_does_not_replay_what_it_already_had() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &id, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        let first = read(&client, &id, &[]).await;
        assert!(first.contains("id: 3"), "the ids come from here: {first}");

        let body = read(&client, &id, &[("last-event-id", "2")]).await;
        assert!(!body.contains("id: 1"), "already had: {body}");
        assert!(!body.contains("id: 2"), "already had: {body}");
        assert!(body.contains("id: 3"), "not yet had: {body}");
    }

    /// The catalogue's `srv.sync.stream_open`, and the field that is the
    /// reason for emitting it.
    ///
    /// A plain `#[test]` with its own runtime rather than `#[tokio::test]`,
    /// because installing a dispatcher is synchronous and has to wrap the whole
    /// exchange rather than sit inside it.
    ///
    /// Both opens are asserted, not just the resumed one: `resumed` is only
    /// readable as a ratio, and a field that is always true measures nothing.
    #[test]
    fn a_stream_open_records_whether_it_resumed() {
        use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};

        let cap = Capture::new();
        let dispatch = build_subscriber(LogConfig {
            target: LogTarget::Capture(cap.clone()),
            filter: "info".to_owned(),
            format: LogFormat::Ndjson,
        })
        .expect("subscriber builds");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        tracing::dispatcher::with_default(&dispatch, || {
            rt.block_on(async {
                let client = Client::new(ServerConfig::default());
                let id = establish(&client).await;
                subscribe(&client, &id, None).await;
                assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

                // A cold open first: the ids a resume names come from it.
                let first = read(&client, &id, &[]).await;
                assert!(first.contains("id: 1"), "no id to resume from: {first}");
                let _ = read(&client, &id, &[("last-event-id", "1")]).await;
            });
        });

        let out = cap.contents();
        assert!(!out.is_empty(), "the capture received no records at all");
        let opens: Vec<serde_json::Value> = out
            .lines()
            .filter(|l| l.contains(r#""ev":"srv.sync.stream_open""#))
            .map(|l| serde_json::from_str(l).expect("record is JSON"))
            .collect();
        assert_eq!(opens.len(), 2, "one record per opened stream: {out}");
        assert_eq!(opens[0]["resumed"], serde_json::json!(false), "{out}");
        assert_eq!(opens[1]["resumed"], serde_json::json!(true), "{out}");
        for open in &opens {
            assert!(
                open["account_h"].is_string(),
                "the record is scoped to an account by hash: {open}"
            );
        }
    }

    /// The defect #101 names, at the boundary.
    ///
    /// A client crashes between receiving a frame and committing it, restarts,
    /// re-sends `Subscribe` with the cursors it can actually vouch for, and
    /// keeps the id of the last frame it *received*. Id-only selection skips
    /// every frame at or below that id, so the ops the cursors asked for are
    /// never sent and `caught_up` is reported over the hole. The pair is
    /// refused instead, with a code the client can branch on.
    #[tokio::test]
    async fn a_resume_id_presented_with_a_fresh_subscribe_is_refused() {
        let client = Client::new(ServerConfig::default());
        let writer = establish(&client).await;
        let device = [9u8; 16];
        for seq in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![envelope(device, seq)], seq).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        // Received through frame 3, applied only through seq 1.
        subscribe(&client, &reader, Some((hex::encode(device), 1))).await;

        let res = open_events(&client, &reader, &[("last-event-id", "3")]).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            res.json()["code"],
            serde_json::json!("SYNC_RESUME_CONFLICT"),
            "the client has to learn this from a code, not from missing frames"
        );
        assert_eq!(
            client.metrics.get("sunrise_sync_resume_conflict_total"),
            1,
            "an operator watching a third-party client needs to see this"
        );
    }

    /// And the frames the refusal protected are actually there.
    ///
    /// Without the refusal this is the silent half: the same session, the same
    /// cursors, and the two ops it never applied simply never arrive.
    #[tokio::test]
    async fn dropping_the_id_replays_what_the_cursors_asked_for() {
        let client = Client::new(ServerConfig::default());
        let writer = establish(&client).await;
        let device = [10u8; 16];
        for seq in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![envelope(device, seq)], seq).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 1))).await;
        open_events(&client, &reader, &[("last-event-id", "3")])
            .await
            .assert_status(StatusCode::BAD_REQUEST);

        let body = read(&client, &reader, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            2,
            "seqs 2 and 3 were received but never applied: {body}"
        );
    }

    /// A refusal is not a state change: the same request stays refused, so a
    /// client that retries without reading the code makes no progress rather
    /// than getting the truncated replay on its second attempt.
    #[tokio::test]
    async fn a_refused_resume_stays_refused_until_the_id_is_dropped() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, Some((hex::encode([11u8; 16]), 1))).await;

        for _ in 0..2 {
            open_events(&client, &id, &[("last-event-id", "1")])
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        assert_eq!(client.metrics.get("sunrise_sync_resume_conflict_total"), 2);
    }

    /// Every `Subscribe` voids the resume point, not just the first: a client
    /// that restates its cursors mid-session is in the same position as one
    /// that restated them at the start.
    #[tokio::test]
    async fn a_second_subscribe_voids_the_resume_point_again() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

        // The stream that serves the first set clears the bar.
        let _ = read(&client, &id, &[]).await;
        subscribe(&client, &id, Some((hex::encode([12u8; 16]), 1))).await;

        open_events(&client, &id, &[("last-event-id", "1")])
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// A zero carries no claim — `relay_replay_after` already reads it as
    /// "first connection" — so it is not the ambiguous pair and is served.
    #[tokio::test]
    async fn a_zero_last_event_id_is_not_a_resume_point() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, Some((hex::encode([13u8; 16]), 1))).await;
        assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

        let body = read(&client, &id, &[("last-event-id", "0")]).await;
        assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
        assert_eq!(client.metrics.get("sunrise_sync_resume_conflict_total"), 0);
    }

    // -- ws_batch_dedup -----------------------------------------------------

    /// The defect: a client that re-sends a batch it never saw acked — every
    /// reconnect re-drains the outbox — had the relay store and fan out a
    /// second copy of ops it already held.
    #[tokio::test]
    async fn a_resent_batch_is_appended_once_and_acked_once() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        let ops = vec![envelope([3u8; 16], 1)];

        let first = publish_acked(&client, &id, ops.clone(), 1).await;
        let second = publish_acked(&client, &id, ops, 1).await;

        assert_eq!(
            first["server_first_seen_ms"], second["server_first_seen_ms"],
            "a re-send is acked with the timestamp the first copy got"
        );
        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 1);

        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            1,
            "the batch must be retained once, not twice: {body}"
        );
    }

    /// Why the key cannot be the `batch_id`.
    ///
    /// `sync_driver.rs` initialises its counter inside `session()`, so a
    /// reconnect re-sends the very same ops under a *different* number — and,
    /// worse, later ops under a number an earlier session already used. Only a
    /// content key catches the first and spares the second.
    #[tokio::test]
    async fn a_resent_batch_reacked_after_a_reconnect_carries_the_first_timestamp() {
        let clock = TestClock::at(T0_MS);
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
        let client = Client::from_state(state);
        let ops = vec![envelope([4u8; 16], 9)];

        let first = publish_acked(&client, &establish(&client).await, ops.clone(), 7).await;
        assert_eq!(first["server_first_seen_ms"], T0_MS);

        // A new session, a counter that restarted, and a later clock.
        clock.set(T0_MS + 60_000);
        let second = publish_acked(&client, &establish(&client).await, ops, 1).await;

        assert_eq!(
            second["server_first_seen_ms"], T0_MS,
            "'first seen' is when the relay first saw it, not when the copy arrived"
        );
    }

    /// The other half of the same rule: the same `batch_id` over different ops
    /// is two batches, and reading them as one would be the data loss the
    /// `batch_id` key was rejected for.
    #[tokio::test]
    async fn a_batch_with_different_ops_is_never_deduped() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        publish_acked(&client, &id, vec![envelope([5u8; 16], 1)], 1).await;
        publish_acked(&client, &id, vec![envelope([5u8; 16], 2)], 1).await;

        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 0);
        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            2,
            "two distinct batches must both be retained: {body}"
        );
    }

    /// The shape ADR-0033 accepted and could not see, now counted.
    ///
    /// A client loses an ack, authors between the loss and the reconnect, and
    /// re-drains its outbox in a different partition. The whole-batch content
    /// key reads that as new work — correctly, by its own rule — so the batch
    /// is stored and fanned out, and `sunrise_relay_batch_duplicate_total`
    /// stays at zero while ops are being re-sent. The two counters together are
    /// what makes the trade in that ADR falsifiable.
    #[tokio::test]
    async fn a_repartitioned_resend_is_counted_without_being_refused() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        let device = [6u8; 16];

        publish_acked(
            &client,
            &id,
            vec![envelope(device, 1), envelope(device, 2)],
            1,
        )
        .await;
        // Seq 2 again, this time carried with a newer op: a different hash and
        // a different partition of the same work.
        publish_acked(
            &client,
            &id,
            vec![envelope(device, 2), envelope(device, 3)],
            2,
        )
        .await;

        assert_eq!(
            client.metrics.get("sunrise_relay_batch_duplicate_total"),
            0,
            "the batch key is not supposed to catch this; that is the point"
        );
        assert_eq!(
            client.metrics.get("sunrise_relay_batch_overlap_total"),
            1,
            "an op the channel already held arrived again"
        );

        // Counted, never refused.
        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            2,
            "both batches must still be stored and served: {body}"
        );
    }

    /// The counter measures the near miss and nothing else.
    ///
    /// Ordinary new work and an exact re-send are the two cases dedup already
    /// handles, and a counter that fired on either would be unreadable against
    /// the append rate.
    #[tokio::test]
    async fn new_work_and_an_exact_resend_are_not_overlaps() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        let device = [7u8; 16];

        let ops = vec![envelope(device, 1)];
        publish_acked(&client, &id, ops.clone(), 1).await;
        // The same batch again: caught by the content key.
        publish_acked(&client, &id, ops, 1).await;
        // And strictly later ops: new work.
        publish_acked(&client, &id, vec![envelope(device, 2)], 2).await;

        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 1);
        assert_eq!(
            client.metrics.get("sunrise_relay_batch_overlap_total"),
            0,
            "neither shape is a near miss"
        );
    }

    /// Retention must not erase the evidence.
    ///
    /// A re-send of an op the channel evicted is still a re-send, and
    /// `relay_frame_heads` cannot say so once the frame that carried it is
    /// gone. `relay_evicted` outlives it, which is why the head is read from
    /// both.
    #[tokio::test]
    async fn an_op_the_channel_evicted_is_still_an_overlap() {
        let device = [8u8; 16];
        let first = envelope(device, 1);
        // A budget that holds one frame of this size, so the first append is
        // evicted by the second.
        let caps = DurableCaps {
            max_bytes: u64::try_from(first.len()).unwrap_or(u64::MAX),
            ..DurableCaps::default()
        };
        let state = ServerState::new(ServerConfig::default()).with_durable_caps(caps);
        let client = Client::from_state(state);
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        publish_acked(&client, &id, vec![first], 1).await;
        publish_acked(&client, &id, vec![envelope(device, 2)], 2).await;
        assert_eq!(
            client.metrics.get("sunrise_relay_batch_overlap_total"),
            0,
            "seq 2 is new work, whatever retention did to seq 1"
        );

        // Seq 1 again, long after the frame that carried it was evicted.
        publish_acked(&client, &id, vec![envelope(device, 1)], 3).await;
        assert_eq!(
            client.metrics.get("sunrise_relay_batch_overlap_total"),
            1,
            "an evicted op is one the channel held, not one it never saw"
        );
    }

    /// An empty batch has no content to be the same as. Collapsing them would
    /// silently drop the three-event expectation the cursor tests hold.
    #[tokio::test]
    async fn an_empty_batch_is_never_deduped() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &id, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 0);
        let body = read(&client, &id, &[]).await;
        assert_eq!(body.matches("\"kind\":\"ops\"").count(), 3, "{body}");
    }

    /// A duplicate must not reach the ring either. Storing it once and fanning
    /// it out twice would leave every live subscriber re-applying ops in
    /// proportion to how flaky the *sender's* link is.
    #[tokio::test]
    async fn a_duplicate_is_not_fanned_out_to_a_live_subscriber() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        let ops = vec![envelope([6u8; 16], 1)];

        publish_acked(&client, &id, ops.clone(), 1).await;
        let body = read(&client, &id, &[]).await;
        assert!(body.contains("id: 1"), "the first copy is frame 1: {body}");

        publish_acked(&client, &id, ops, 2).await;

        // Resuming past frame 1 is how a live subscriber that already had the
        // first copy sees the stream: a second frame would show up here.
        let after = read(&client, &id, &[("last-event-id", "1")]).await;
        assert!(
            !after.contains("\"kind\":\"ops\""),
            "a duplicate produced a second frame: {after}"
        );
    }
}
