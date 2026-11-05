//! The reply readers.

use super::*;

/// The relay's own `Date` is read back when it is readable and never
/// invented when it is not: it is the only server clock a *refused* request
/// carries, and a fabricated one would make the skew advice a guess.
#[test]
fn the_server_s_date_is_read_back_and_never_invented() {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::DATE, DATE.parse().expect("a header value"));
    assert_eq!(super::server_date(&headers).as_deref(), Some(DATE));

    assert!(super::server_date(&hyper::HeaderMap::new()).is_none());

    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::DATE,
        hyper::header::HeaderValue::from_bytes(&[0xff, 0xfe]).expect("a header value"),
    );
    assert!(
        super::server_date(&headers).is_none(),
        "an unreadable Date degrades the advice rather than the diagnosis"
    );
}

/// `Retry-After` is read in its delta-seconds form, the one the relay sends,
/// and every other shape reads as absent so the driver falls back to its own
/// backoff rather than to a guess.
#[test]
fn retry_after_reads_delta_seconds_and_nothing_else() {
    let with = |value: &str| {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::RETRY_AFTER,
            value.parse().expect("a header value"),
        );
        super::retry_after(&headers)
    };
    assert_eq!(with("5"), Some(std::time::Duration::from_secs(5)));
    assert_eq!(with(" 120 "), Some(std::time::Duration::from_secs(120)));
    assert_eq!(with("0"), Some(std::time::Duration::ZERO));
    assert_eq!(with("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    assert_eq!(with("-1"), None);
    assert_eq!(with("1.5"), None);
    assert!(super::retry_after(&hyper::HeaderMap::new()).is_none());
}

/// The advisory reply fields are total: absent or wrongly typed reads as
/// the type's zero, because a relay that omits a capability bitfield should
/// not sink a session that does not use it.
#[test]
fn the_reply_field_readers_return_the_value_or_a_zero() {
    let reply = serde_json::json!({
        "server_app_v": "1.2.3",
        "capabilities": 9,
        "wire_proto": 7,
        "text": "not a number",
    });
    assert_eq!(super::field_str(&reply, "server_app_v"), "1.2.3");
    assert_eq!(super::field_str(&reply, "absent"), "");
    assert_eq!(
        super::field_str(&reply, "capabilities"),
        "",
        "a number is not a string"
    );
    assert_eq!(super::field_u64(&reply, "capabilities"), 9);
    assert_eq!(super::field_u64(&reply, "absent"), 0);
    assert_eq!(super::field_u64(&reply, "text"), 0);
    assert_eq!(super::field_u32(&reply, "wire_proto").expect("a u32"), 7);
}

/// A negotiated version that does not fit a `u32` is refused rather than
/// truncated. The whole point of the handshake is that both sides agree on
/// the value, and a wrapped one is a number neither of them said.
#[test]
fn a_negotiated_version_too_large_for_a_u32_is_refused() {
    let reply = serde_json::json!({"wire_proto": u64::from(u32::MAX) + 1});
    let err = super::field_u32(&reply, "wire_proto").expect_err("it does not fit");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("wire_proto")),
        "{err}"
    );
}

/// A cursor goes up as the hex device id and the sequence already applied.
#[test]
fn a_cursor_is_sent_as_a_hex_device_id_and_its_applied_sequence() {
    let entry = super::CursorEntry {
        device_id: [0x7bu8; 16],
        last_applied_seq: 42,
    };
    assert_eq!(
        super::cursor_json(&entry),
        serde_json::json!({"device_id": "7b".repeat(16), "last_applied_seq": 42})
    );
}

/// A stream id is sixteen bytes of hex; anything else is a malformed event
/// rather than an id of zeroes that would then address the wrong stream.
#[test]
fn a_stream_id_is_parsed_from_hex_and_anything_else_is_refused() {
    let id = serde_json::json!("0f".repeat(16));
    assert_eq!(
        super::parse_id(Some(&id)).expect("a stream id"),
        [0x0fu8; 16]
    );
    assert!(super::parse_id(None).is_err(), "an event with no stream id");
    assert!(
        super::parse_id(Some(&serde_json::json!("0f0f"))).is_err(),
        "a stream id of the wrong length"
    );
    assert!(
        super::parse_id(Some(&serde_json::json!(16))).is_err(),
        "a stream id that is not a string"
    );
}

/// A status carrying no typed code still lands in the right family, so a
/// storage outage and a rejected operation do not read alike to a driver
/// deciding whether to retry.
#[test]
fn a_status_with_no_typed_code_still_maps_to_the_right_family() {
    let t = SseTransport::connect("http://127.0.0.1:1");
    assert_eq!(
        code_of(&t.refuse(&reply(503, b"", None))),
        "RELAY_STORAGE_UNAVAILABLE"
    );
    assert_eq!(
        code_of(&t.refuse(&reply(403, b"", None))),
        "AUTH_TOKEN_INVALID"
    );
    assert_eq!(
        code_of(&t.refuse(&reply(400, b"", None))),
        "SYNC_OP_INVALID"
    );
}
