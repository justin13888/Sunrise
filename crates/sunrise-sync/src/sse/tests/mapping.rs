//! The mapping half: `frame_for` needs no socket either.

use super::*;

/// An `ops` event hands the driver the relay's bytes, byte for byte.
///
/// The module documentation claims exactly this — what the driver applies
/// is what the relay stored — and it is why this direction is a base64
/// decode rather than a re-encoding. A re-encoding that happened to
/// round-trip today would break the first time either side reordered a map
/// key, and every signature over those bytes with it.
#[test]
fn an_ops_event_carries_the_relay_s_frame_verbatim() {
    use base64::Engine as _;
    let stored: &[u8] = b"\x01\x02\x03 whatever the publisher sent";
    let event = serde_json::json!({
        "kind": "ops",
        "frame": base64::engine::general_purpose::STANDARD.encode(stored),
    });
    let frame = SseTransport::frame_for(&event)
        .expect("a well-formed ops event")
        .expect("an ops event is a frame");
    assert_eq!(frame, stored);
}

/// An `ops` event with nothing to decode is a malformed exchange, not an
/// empty frame the driver would then try to apply.
#[test]
fn an_ops_event_with_no_frame_is_a_protocol_error() {
    let err = SseTransport::frame_for(&serde_json::json!({"kind": "ops"}))
        .expect_err("there is nothing to decode");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("no frame")),
        "{err}"
    );
}

/// `caught_up` becomes the `StreamUpdate` the driver waits on, naming the
/// stream that finished replaying.
#[test]
fn a_caught_up_event_becomes_a_stream_update_naming_its_stream() {
    let event = serde_json::json!({
        "kind": "caught_up",
        "stream_id": "ab".repeat(16),
    });
    let frame = SseTransport::frame_for(&event)
        .expect("a well-formed caught_up event")
        .expect("caught_up is a frame");
    let (head, payload) = super::decode_frame(&frame).expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::StreamUpdate);
    let parsed = super::CaughtUpPayload::decode(&payload).expect("a CaughtUpPayload");
    assert_eq!(parsed.stream_id, [0xabu8; 16]);
}

/// A `gap` becomes a coded error frame rather than a disconnection: the
/// driver answers a cursor gap by resubscribing, which it can only do if it
/// is told which condition it hit.
/// #459: `backlog` becomes a `StreamUpdate` carrying the count, which the
/// driver tells apart from a `caught_up` by its shape.
#[test]
fn a_backlog_event_becomes_a_stream_update_carrying_its_count() {
    let frame = SseTransport::frame_for(&serde_json::json!({
        "kind": "backlog",
        "stream_id": "ab".repeat(16),
        "ops": 42,
    }))
    .expect("a well-formed backlog event")
    .expect("backlog is a frame");
    let (head, payload) = super::decode_frame(&frame).expect("a frame");
    assert_eq!(head.msg_kind, super::MsgKind::StreamUpdate);
    assert_eq!(
        super::BacklogPayload::decode(&payload).expect("a BacklogPayload"),
        super::BacklogPayload {
            ops: 42,
            stream_id: [0xab; 16],
        }
    );
    let err = SseTransport::frame_for(&serde_json::json!({
        "kind": "backlog",
        "stream_id": "ab".repeat(16),
    }))
    .expect_err("a count is the event's whole content");
    assert!(matches!(err, TransportError::Protocol(_)), "{err:?}");
}

#[test]
fn a_gap_event_becomes_a_coded_error_frame_carrying_its_reason() {
    for (event, reason) in [
        (
            serde_json::json!({"kind": "gap", "reason": "cursor behind retention"}),
            "cursor behind retention",
        ),
        (serde_json::json!({"kind": "gap"}), "cursor gap"),
    ] {
        let frame = SseTransport::frame_for(&event)
            .expect("a well-formed gap event")
            .expect("a gap is a frame");
        let (head, payload) = super::decode_frame(&frame).expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::Error);
        let parsed = super::ErrorPayload::decode(&payload).expect("an ErrorPayload");
        assert_eq!(parsed.code, sunrise_error::ErrorCode::SyncCursorGap);
        assert_eq!(parsed.reason, reason);
    }
}

/// A `closed` event carries the relay's own close code through, so only the
/// expiry is read as recoverable.
///
/// One case per code `crates/sunrise-server/src/api/sync/stream.rs` emits —
/// `AUTH_TOKEN_EXPIRED` (:338), `AUTH_DEVICE_REVOKED` (:362) and
/// `RELAY_STORAGE_UNAVAILABLE` (:232) — because
/// `ClosePayload::is_recoverable` is `matches!(self.code,
/// AuthTokenExpired)` and collapsing the three onto the first is what
/// `docs/05-sync/wire-protocol.md:203-206` calls "retries forever against a
/// revoked device". `SyncEvent::Closed.code` is not an `Option`, so every
/// one of these is an event the relay really sends.
#[test]
fn a_closed_event_carries_the_relay_s_own_close_code() {
    for (code, recoverable) in [
        ("AUTH_TOKEN_EXPIRED", true),
        ("AUTH_DEVICE_REVOKED", false),
        ("RELAY_STORAGE_UNAVAILABLE", false),
    ] {
        let event = serde_json::json!({
            "kind": "closed",
            "code": code,
            "reason": "the relay said so",
        });
        let frame = SseTransport::frame_for(&event)
            .expect("a well-formed closed event")
            .expect("a close is a frame");
        let (head, payload) = super::decode_frame(&frame).expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::Close);
        let parsed = super::ClosePayload::decode(&payload).expect("a ClosePayload");
        assert_eq!(
            parsed.code.as_str(),
            code,
            "the driver branches on the relay's code, not on a constant"
        );
        assert_eq!(parsed.reason, "the relay said so");
        assert_eq!(
            parsed.is_recoverable(),
            recoverable,
            "{code} decides whether the client refreshes or asks the user"
        );
    }
}

/// A `closed` event whose code this build has never heard of is **not**
/// recoverable, and its spelling survives whatever shape it arrived in.
///
/// `docs/05-sync/wire-protocol.md:214-216` records adding a close code as
/// non-breaking, so a relay one version ahead closing for a reason this
/// build has never heard of is sanctioned rather than malformed — and a
/// client that read it as recoverable would refresh, reconnect, be closed
/// again for the same unread reason and loop, which is the failure
/// `docs/05-sync/wire-protocol.md:203-206` exists to name. No unreadable
/// code may land on `AUTH_TOKEN_EXPIRED`, the single code `is_recoverable`
/// admits.
///
/// The code it does land on is the one
/// `docs/10-cross-cutting/error-handling.md:36` reserves for an older
/// client meeting a newer code, and the unparsed spelling survives in the
/// diagnostic the same paragraph asks for, so support tooling can still
/// say which close this was.
///
/// The last two shapes are the ones a JSON reader loses. `code` is a
/// `String` in the relay's own type, so a number or an object is as
/// unreadable as an unknown name — but reading it *as a string* yields
/// `None` and throws the spelling away, which leaves a terminal close with
/// nothing to say about itself. Rendering the value as it arrived is what
/// keeps the preservation half of `error-handling.md:36` true on every
/// shape rather than only on the convenient one.
#[test]
fn a_closed_event_this_build_cannot_read_is_terminal_rather_than_recoverable() {
    for (event, reason) in [
        (
            serde_json::json!({"kind": "closed", "code": "FROM_THE_FUTURE", "reason": "who knows"}),
            "who knows (unrecognised close code FROM_THE_FUTURE)",
        ),
        (
            serde_json::json!({"kind": "closed", "code": 42, "reason": "who knows"}),
            "who knows (unrecognised close code 42)",
        ),
        (
            serde_json::json!({"kind": "closed", "code": {"was": "an object"}}),
            r#"session closed (unrecognised close code {"was":"an object"})"#,
        ),
    ] {
        let frame = SseTransport::frame_for(&event)
            .expect("a well-formed closed event")
            .expect("a close is a frame");
        let (_, payload) = super::decode_frame(&frame).expect("a decodable frame");
        let parsed = super::ClosePayload::decode(&payload).expect("a ClosePayload");
        assert_eq!(
            parsed.code,
            sunrise_error::ErrorCode::InternalUnknownCode,
            "a close this build cannot read is named as such, not guessed at"
        );
        assert!(
            !parsed.is_recoverable(),
            "{event} must not read as a close the client may retry through"
        );
        assert_eq!(
            parsed.reason, reason,
            "the relay's own words survive, and so does the code nobody could parse"
        );
    }
}

/// A `closed` event carrying **no** code is a malformed event rather than a
/// close this build is merely too old to read.
///
/// The two conditions are not the same one, and
/// `docs/10-cross-cutting/error-handling.md:36` reserves `INTERNAL_UNKNOWN_CODE` for
/// the first of them — "an older client receiving an **unknown** code". A relay one
/// version ahead is doing something the protocol permits; a relay sending no code at
/// all is not, because `crates/sunrise-server/src/api/sync/stream.rs:107-109#SyncEvent`
/// declares `SyncEvent::Closed.code` as a `String` rather than an `Option<String>`.
/// That is the same argument the `Hello` arm makes from
/// `crates/sunrise-server/src/api/sync/credential.rs:59#SessionResponse` about a
/// missing `session_id`, and this is the other end of it.
///
/// `null` counts as absent: a non-`Option` `String` does not accept one
/// either, so the two reach this client for the same reason and get the
/// same answer.
///
/// Folding this into the forward-compatibility code would file a broken
/// relay under "newer than us", which is the one reading that guarantees
/// nobody looks at it.
#[test]
fn a_closed_event_with_no_code_at_all_is_a_protocol_error() {
    for event in [
        serde_json::json!({"kind": "closed"}),
        serde_json::json!({"kind": "closed", "code": null, "reason": "who knows"}),
    ] {
        let err = SseTransport::frame_for(&event)
            .expect_err("a closed event with no code is malformed, not merely unreadable");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("no code")),
            "{event} names the condition rather than guessing at a code: {err}"
        );
    }
}

/// An event kind this build has never heard of is dropped, not fatal.
///
/// The relay is allowed to grow event kinds without every client being
/// upgraded first; a client that tore the stream down over one would make
/// the forward compatibility the stream is versioned for unusable.
#[test]
fn an_event_kind_this_build_does_not_know_is_dropped_rather_than_fatal() {
    for event in [
        serde_json::json!({"kind": "from_the_future"}),
        serde_json::json!({}),
    ] {
        assert!(
            SseTransport::frame_for(&event)
                .expect("an unknown kind is not an error")
                .is_none(),
            "nothing reaches the driver for {event}"
        );
    }
}
