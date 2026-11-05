//! The transport's shape, how a refusal is classified and explained, and
//! the request binding every operation carries -- all without a socket.

use super::*;

/// Compile-time assertion that this still fits the driver's transport slot.
#[test]
fn it_is_a_transport() {
    fn assert_transport<T: Transport>() {}
    assert_transport::<SseTransport>();
}

/// A frame sent before `Hello` has nowhere to go, and saying so beats
/// issuing a request with no session that the relay would refuse anyway.
#[tokio::test]
async fn a_read_before_hello_reports_the_missing_session() {
    let mut t = SseTransport::connect("http://127.0.0.1:1");
    let err = t.recv_frame().await.expect_err("no session yet");
    assert!(
        format!("{err}").contains("session"),
        "expected a session complaint, got {err}"
    );
}

/// The regression: a `401` whose body names a code this build knows keeps
/// that code. Flattening it to the status map is what left a client
/// refreshing a bearer to cure a clock.
#[test]
fn a_typed_code_survives_the_status_map() {
    let t = SseTransport::connect("http://127.0.0.1:1");
    let err = t.refuse(&reply(401, SIG_INVALID, None));
    assert_eq!(code_of(&err), "AUTH_DEVICE_SIG_INVALID");
}

/// A refusal carries the relay's `Retry-After` onto the error the driver
/// branches on, and one that sent none carries none.
#[test]
fn a_refusal_carries_its_retry_after() {
    let t = SseTransport::connect("http://127.0.0.1:1");
    let limited = br#"{"status":429,"code":"RATE_LIMITED"}"#;
    let mut r = reply(429, limited, None);
    r.retry_after = Some(std::time::Duration::from_secs(5));
    match t.refuse(&r) {
        TransportError::Server { retry_after, .. } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(5)));
        }
        other => panic!("expected a server refusal, got {other}"),
    }
    match t.refuse(&reply(429, limited, None)) {
        TransportError::Server { retry_after, .. } => assert_eq!(retry_after, None),
        other => panic!("expected a server refusal, got {other}"),
    }
}

/// And the fallback still holds, so a relay that answers with a code this
/// build has never heard of — or with no body at all — is still classified
/// rather than dropped on the floor.
#[test]
fn an_untyped_401_still_maps_to_the_token_code() {
    let t = SseTransport::connect("http://127.0.0.1:1");
    let err = t.refuse(&reply(401, b"", None));
    assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");

    let unknown = br#"{"status":401,"code":"AUTH_SOMETHING_FROM_THE_FUTURE"}"#;
    let err = t.refuse(&reply(401, unknown, None));
    assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
}

/// A clock outside the replay window is the commonest cause of this
/// refusal and the only one the user can fix, so the measurement the relay
/// handed back on the refusing response is turned into an instruction. The
/// bare code alone sent people to `sunrise login`, which cures nothing
/// here.
#[test]
fn a_skewed_clock_is_named_and_measured() {
    let late = NOW_MS + (sunrise_http_sig::MAX_CLOCK_SKEW_SECS as u64 + 100) * 1000;
    let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(late));
    let err = t.refuse(&reply(401, SIG_INVALID, Some(DATE)));
    let message = message_of(&err);
    assert!(message.contains("400s from the relay's"), "{message}");
    assert!(message.contains("set the system clock"), "{message}");
}

/// And a clock that is *fine* must not be blamed: the same code then means
/// the key is unregistered or the device revoked, and naming the clock
/// would send the user to fix the one thing that is right.
#[test]
fn a_correct_clock_is_not_blamed_for_a_refused_key() {
    let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(NOW_MS));
    let message = message_of(&t.refuse(&reply(401, SIG_INVALID, Some(DATE))));
    assert!(!message.contains("clock is"), "{message}");
    assert!(message.contains("revoked"), "{message}");

    // A relay that sent no `Date` leaves the skew unmeasurable, which is
    // the same "cannot blame the clock" answer rather than a guess.
    let message = message_of(&t.refuse(&reply(401, SIG_INVALID, None)));
    assert!(!message.contains("clock is"), "{message}");
}

/// A transport with no signer against a relay that requires one gets the
/// fourth case named, because "your signature is wrong" is misleading when
/// none was sent.
#[test]
fn an_unbound_transport_is_told_it_sent_no_binding() {
    let t = SseTransport::connect("http://127.0.0.1:1");
    let message = message_of(&t.refuse(&reply(401, SIG_INVALID, Some(DATE))));
    assert!(message.contains("sent no device binding"), "{message}");
}

/// The advice is a pure function of the two facts it has, so the branch
/// boundary is pinned rather than approached only through a transport.
#[test]
fn the_skew_advice_turns_on_the_scheme_s_own_tolerance() {
    let max = sunrise_http_sig::MAX_CLOCK_SKEW_SECS as u64;
    let inside = binding_advice(Some(DATE), Some(NOW_MS + max * 1000));
    assert!(!inside.contains("clock is"), "{inside}");
    let outside = binding_advice(Some(DATE), Some(NOW_MS + (max + 1) * 1000));
    assert!(outside.contains("clock is 301s"), "{outside}");
}

/// Every route this transport reaches is one `signed.rs` binds, so a
/// binding is produced for all of them — including the bodyless `GET` that
/// opens the stream and the revocation `DELETE`, which are the two easiest
/// to leave out because neither has a body to sign over.
#[test]
fn every_operation_this_transport_makes_carries_a_binding() {
    let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(NOW_MS));
    for (method, path, body) in [
        (
            "POST",
            "/api/v1/sync/session",
            Some(serde_json::json!({"a": 1})),
        ),
        (
            "POST",
            "/api/v1/sync/subscribe",
            Some(serde_json::json!({"streams": []})),
        ),
        (
            "POST",
            "/api/v1/sync/ops",
            Some(serde_json::json!({"ops": []})),
        ),
        (
            "POST",
            "/api/v1/sync/session/refresh",
            Some(serde_json::json!({"token": "t"})),
        ),
        ("GET", "/api/v1/sync/events", None),
        (
            "DELETE",
            "/api/v1/devices/by-vault-id/01ARZ3NDEKTSV4RRFFQ69G5FAV",
            None,
        ),
    ] {
        let headers = t.binding(method, path, body.as_ref()).expect("a binding");
        let names: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec!["x-sunrise-device", "x-sunrise-device-sig", "date"],
            "{method} {path} must carry the whole binding"
        );

        // And the signature verifies against the key, over this exact
        // target — which is what makes the header more than three
        // well-named strings.
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        sunrise_http_sig::verify(
            &sunrise_http_sig::device_pub_b64(&key.verifying_key().to_bytes()),
            &headers[1].1,
            method,
            path,
            &headers[2].1,
            body.as_ref(),
            NOW_MS,
        )
        .unwrap_or_else(|e| panic!("{method} {path} must verify: {e}"));
    }
}

/// The byte-body binding is the blob path's, and it was the untested half.
///
/// [`Self::binding`] signs a JSON *value* and the test above covers every
/// route that uses it. `binding_bytes` signs bytes that already are the
/// canonical form — the chunk `PUT` and the blob `GET`, which is the whole
/// attachment upload and download surface — and nothing reached it. Five
/// mutants rewriting it to return an empty or junk header list all
/// survived, so the signing on that surface was asserted by nothing.
///
/// Verified with `verify_canonical` rather than `verify`: these bytes are
/// the message, and routing them through the value verifier would
/// canonicalize them a second time and assert the wrong thing.
#[test]
fn the_byte_body_operations_carry_a_verifying_binding() {
    let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(NOW_MS));
    let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
    for (method, path, body) in [
        (
            "PUT",
            "/api/v1/blobs/01ARZ3NDEKTSV4RRFFQ69G5FAV/0",
            b"sealed-chunk-bytes".as_slice(),
        ),
        (
            "GET",
            "/api/v1/blobs/01ARZ3NDEKTSV4RRFFQ69G5FAV",
            b"".as_slice(),
        ),
    ] {
        let headers = t.binding_bytes(method, path, body);
        let names: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec!["x-sunrise-device", "x-sunrise-device-sig", "date"],
            "{method} {path} must carry the whole binding"
        );
        assert_eq!(
            headers[0].1, "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
            "{method} {path} must name the device that signed it"
        );
        sunrise_http_sig::verify_canonical(
            &sunrise_http_sig::device_pub_b64(&key.verifying_key().to_bytes()),
            &headers[1].1,
            method,
            path,
            &headers[2].1,
            body,
            NOW_MS,
        )
        .unwrap_or_else(|e| panic!("{method} {path} must verify: {e}"));
    }
}

/// A transport with no signer sends no binding at all, which is the
/// self-host `NullVerifier` deployment rather than an omission: sending an
/// empty or partial one would be refused where an absent one is accepted.
///
/// Both signing paths, because they carry the decision separately. The
/// byte-body one was reached by no test at all: it is a bare `return` with
/// no `Result` around it, so nothing above would have noticed it returning
/// a partial binding on the unauthenticated path.
#[test]
fn an_unsigned_transport_sends_no_binding_headers() {
    let t = SseTransport::connect("http://127.0.0.1:1");
    assert!(t
        .binding("GET", "/api/v1/sync/events", None)
        .expect("no binding is not an error")
        .is_empty());
    assert!(t
        .binding_bytes("PUT", "/api/v1/blobs/up-1/0", b"sealed-chunk-bytes")
        .is_empty());
}
