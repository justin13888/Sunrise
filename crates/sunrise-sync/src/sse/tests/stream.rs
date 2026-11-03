//! The event stream against the loopback relay: how it is opened and
//! resumed, how a refusal of it is reported, and the frame cap on what it
//! delivers.

use super::*;

/// [`MAX_EVENT_BYTES`] is [`FRAME_CAP`]'s number. Eight mebibytes is the ceiling a relay
/// has to stay under for its events to be delivered at all, so moving it is
/// a protocol-visible act rather than an implementation detail.
#[test]
fn the_frame_cap_is_the_documented_eight_mebibytes() {
    assert_eq!(super::MAX_EVENT_BYTES, FRAME_CAP);
}

/// A stream that breaks mid-body is reported, not read as a graceful end.
///
/// `recv_frame` has two arms for a stream that stops: `None` is the relay
/// closing the stream cleanly, which is not an error because the driver's
/// reconnect decides whether to come back, and `Some(Err(e))` is the
/// connection failing under it, which is. Collapsing the second onto the
/// first would have a client treat a broken relay as a caught-up one and
/// back off instead of retrying.
///
/// Reaching it needs a double whose body can fail: with
/// `Chunks::Error = Infallible` the arm is unreachable by construction, and
/// a mutant that deletes it survives for that reason rather than because
/// nothing depends on it.
#[tokio::test]
async fn a_stream_that_breaks_mid_body_is_reported_rather_than_ended() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream_then_abort(200, vec![half_read_stream()]),
    ])
    .await;
    let mut t = open_session(&relay).await;
    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("an event became a frame");

    let err = next_frame(&mut t)
        .await
        .expect_err("a body that fails is not an end of stream");
    assert!(
        matches!(err, TransportError::Unavailable(_)),
        "a broken connection is retryable, not a clean close: {err}"
    );
    assert!(
        t.events.is_none(),
        "the failed body is released so the next read reopens"
    );
}

/// The stream is a bound route like every other, and its events become the
/// frames the driver already knows how to handle.
///
/// It is the easiest binding to leave out, because it is the one request
/// with no body to sign over.
#[tokio::test]
async fn the_event_stream_is_opened_bound_and_its_events_become_frames() {
    let event = format!(
        "id: 7\ndata: {}\n\n",
        serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
    );
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![hyper::body::Bytes::from(event)]),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let (head, payload) = super::decode_frame(
        &next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame"),
    )
    .expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::StreamUpdate);
    assert_eq!(
        super::CaughtUpPayload::decode(&payload)
            .expect("a CaughtUpPayload")
            .stream_id,
        [0xabu8; 16]
    );
    assert_eq!(
        t.last_event_id.as_deref(),
        Some("7"),
        "the resume point advances with every id the stream carried"
    );

    let seen = relay.seen();
    assert_eq!(seen[1].method, "GET");
    assert_eq!(seen[1].path, "/api/v1/sync/events");
    assert_eq!(seen[1].header("x-sunrise-session"), Some("sess-1"));
    assert_eq!(seen[1].header("accept"), Some("text/event-stream"));
    assert_json_binding(&seen[1]);
}

/// A stream that ends is a graceful end, and the reconnect resumes from the
/// last id rather than replaying what it already applied.
#[tokio::test]
async fn a_reopened_stream_resumes_from_the_last_id_it_saw() {
    let event = |id: u32| {
        hyper::body::Bytes::from(format!(
            "id: {id}\ndata: {}\n\n",
            serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
        ))
    };
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![event(7)]),
        relay::Canned::stream(vec![event(8)]),
    ])
    .await;
    let mut t = open_session(&relay).await;

    next_frame(&mut t)
        .await
        .expect("the stream opens")
        .expect("a frame");
    assert!(
        next_frame(&mut t)
            .await
            .expect("a relay that closes the stream is not an error")
            .is_none(),
        "the driver's backoff decides whether to come back, not this layer"
    );
    next_frame(&mut t)
        .await
        .expect("the stream reopens")
        .expect("a frame");

    let seen = relay.seen();
    assert_eq!(
        seen[1].header("last-event-id"),
        None,
        "nothing to resume from yet"
    );
    assert_eq!(seen[2].header("last-event-id"), Some("7"));
}

/// A refused stream is reported as a refusal rather than opened, since
/// treating one as a successful open would leave the driver reading an
/// error document as events — and it is reported with the relay's *own*
/// code, like every other route.
///
/// This is the flattening [`SseTransport::refuse`] exists to prevent: a
/// client told "your signature is stale, fix your clock" heard "your bearer
/// is bad" and refreshed a token that was never the problem. The stream was
/// the one route still doing it, because it dropped the problem document
/// before `refuse` could read a code out of it.
///
/// The read goes through [`next_frame`] like the other relay reads: a
/// refusal is answered by a relay whose script is exhausted by then, and an
/// unbounded read there would hang the whole test binary rather than fail
/// one test.
#[tokio::test]
async fn a_refused_stream_is_reported_with_the_relay_s_own_code() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(401, r#"{"code":"AUTH_DEVICE_SIG_INVALID"}"#),
    ])
    .await;
    let mut t = open_session(&relay).await;
    let err = next_frame(&mut t)
        .await
        .expect_err("the relay refused the stream");
    assert_eq!(code_of(&err), "AUTH_DEVICE_SIG_INVALID");

    // And the advice rides with it. A refused *binding* is the one refusal
    // whose code leaves the user nothing to do, so `refuse` appends what to
    // do about it — and until `open_events` read the body this route could
    // not reach that arm at all, because a refusal with no document never
    // resolves to `AUTH_DEVICE_SIG_INVALID`. The skew branch is the
    // deterministic one here: [`signer`] is fixed at [`NOW_MS`], a date in
    // the past, and the relay's own `Date` is the wall clock, so the
    // measured skew is always outside the tolerance.
    let message = message_of(&err);
    assert!(
        message.contains("AUTH_DEVICE_SIG_INVALID"),
        "the relay's own words come through: {message}"
    );
    assert!(
        message.contains("set the system clock and retry"),
        "and the user is told what to do about it: {message}"
    );
}

/// The stream route carries the *other two* binding-advice arms as well.
///
/// The case above is the skew arm, which is deterministic there only
/// because [`signer`] is fixed at [`NOW_MS`], a date in the past. The arm a
/// registered user meets more often is the one that cannot blame a clock —
/// a clock that agrees with the relay's, and a key the relay still will not
/// take — and the fourth is a client that sent no binding at all. Both
/// reach a user through this route, and neither was executed on it.
///
/// The clock that is *fine* is the load-bearing half: naming a skew there
/// would send someone to fix the one thing that is already right.
#[tokio::test]
async fn a_refused_stream_names_the_binding_cause_it_can_actually_measure() {
    let refusal = || {
        vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(401, r#"{"code":"AUTH_DEVICE_SIG_INVALID"}"#),
        ]
    };
    let open = |mut t: SseTransport| async move {
        t.send_frame(hello_frame()).await.expect("a session opens");
        next_frame(&mut t)
            .await
            .expect("the ack is waiting")
            .expect("a HelloAck");
        next_frame(&mut t)
            .await
            .expect_err("the relay refused the stream")
    };

    // A clock that agrees with the relay's own `Date`, so the skew is
    // measurable and inside the window.
    //
    // The one place this file reads the real wall clock, and the arm under
    // test is why: the relay's `Date` is hyper's own wall clock, so a
    // device clock fixed at [`NOW_MS`] — which is what every other case
    // here signs with — can only ever produce the *skew* arm. That is
    // exactly how this route came to execute one arm of three.
    #[allow(clippy::disallowed_methods)]
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_millis(),
    )
    .expect("a millisecond count that fits a u64");
    let relay = relay::start(refusal()).await;
    let message = message_of(
        &open(SseTransport::connect(&relay.base).with_device_signer(signer(now_ms))).await,
    );
    assert!(
        message.contains("may not be registered"),
        "the remaining causes are listed when none can be measured: {message}"
    );
    assert!(
        !message.contains("set the system clock"),
        "and a clock that is fine is not blamed for it: {message}"
    );

    // And a transport carrying no signer at all is told so, rather than
    // being sent to look at a binding it never sent.
    let relay = relay::start(refusal()).await;
    let message = message_of(&open(SseTransport::connect(&relay.base)).await);
    assert!(
        message.contains("sent no device binding"),
        "an unbound client is told what it is missing: {message}"
    );
}

/// And a refusal carrying no code at all still lands on the status map,
/// which is what keeps this client readable against a relay whose codes it
/// does not know.
#[tokio::test]
async fn a_refused_stream_with_no_document_falls_back_to_the_status_map() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::empty(401),
    ])
    .await;
    let mut t = open_session(&relay).await;
    let err = next_frame(&mut t)
        .await
        .expect_err("the relay refused the stream");
    assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
}

/// A refusal whose body *fails* is still a refusal.
///
/// The third way the document can fail to arrive, beside empty and
/// oversized, and the one no script could reach until the double could
/// pair a non-2xx status with a failing body. The relay answers `401`,
/// writes a **whole, valid** problem document naming
/// `AUTH_DEVICE_SIG_INVALID` and then breaks the connection instead of
/// ending the message: a failed read yields nothing, so what arrived is
/// discarded rather than parsed, the read does not swallow the refusal,
/// and the status map answers — the same degrade the empty and oversized
/// bodies take.
///
/// The document is valid on purpose. A truncated one would land on
/// `AUTH_TOKEN_INVALID` whether the failed-read arm ran or the bytes were
/// simply unparseable, and the test would pass without reaching the arm it
/// names.
#[tokio::test]
async fn a_refusal_whose_body_fails_falls_back_to_the_status_map() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream_then_abort(
            401,
            vec![hyper::body::Bytes::from_static(
                br#"{"code":"AUTH_DEVICE_SIG_INVALID"}"#,
            )],
        ),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let err = next_frame(&mut t)
        .await
        .expect_err("the relay refused the stream and then dropped the body");
    assert_eq!(
        code_of(&err),
        "AUTH_TOKEN_INVALID",
        "a refusal whose document failed is reported as a refusal, not as a broken read"
    );
}

/// A refusal whose body never finishes arriving is still a refusal.
///
/// The relay answers `401`, writes the first few bytes of a problem
/// document, and then holds the connection. Before the read was deadlined
/// this await did not return: the driver's only bound is `tokio::select!`
/// **cancelling** the whole future, and cancellation is not a timeout — the
/// partial body and the refusal are both dropped, `events` is still `None`,
/// and the next iteration opens a brand-new stream. No error reaches the
/// driver, the session never ends and no backoff engages, so a stalling
/// relay becomes a silent retry loop driven by whatever else wakes the
/// driver.
///
/// What comes back is the status map, which is where this route was before
/// it read the body at all — and the truncated `"code":"AUTH_DEV` that did
/// arrive is *not* read, because half a document is not a code.
#[tokio::test]
async fn a_refusal_whose_body_stalls_is_still_reported_as_a_refusal() {
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json_then_stall(401, r#"{"code":"AUTH_DEV"#),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let started = tokio::time::Instant::now();
    let err = next_frame(&mut t)
        .await
        .expect_err("the relay refused the stream and then went quiet");
    assert_eq!(
        code_of(&err),
        "AUTH_TOKEN_INVALID",
        "a body that never arrived falls back to the status map"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(1_500)
            && elapsed < std::time::Duration::from_secs(14),
        "the read is bounded by its own deadline, and that deadline is the two seconds \
             `the_body_bounds_are_the_numbers_they_are_documented_as` pins — an upper bound \
             alone leaves every shorter value passing too. The upper bound is the one thing \
             here that tells two seconds from the blob route's fifteen, since nothing names \
             the deadline that ran, so it sits just under fifteen and leaves real wall \
             clock the rest: {elapsed:?}"
    );
}

/// A refusal document is read up to the frame cap and no further.
///
/// Both sides of the boundary, because a cap that refused everything would
/// pass a test written only against the oversized half. At exactly
/// [`super::MAX_EVENT_BYTES`] the relay's own code still wins; one byte
/// past it the read is abandoned and the status map answers instead — which
/// is the same degrade every other unreadable refusal takes, so a relay
/// cannot make this client hold an unbounded body by refusing it.
#[tokio::test]
async fn a_refusal_document_past_the_frame_cap_is_abandoned_for_the_status_map() {
    let document = |len: usize| {
        let head = r#"{"code":"AUTH_DEVICE_SIG_INVALID","detail":""#;
        let tail = r#""}"#;
        format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
    };

    let at_cap = document(FRAME_CAP);
    assert_eq!(at_cap.len(), FRAME_CAP);
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(401, &at_cap),
    ])
    .await;
    let mut t = open_session(&relay).await;
    let err = next_frame(&mut t).await.expect_err("the relay refused");
    assert_eq!(
        code_of(&err),
        "AUTH_DEVICE_SIG_INVALID",
        "exactly the cap is under the cap, so the relay's own code wins"
    );

    let past_cap = document(FRAME_CAP + 1);
    assert_eq!(past_cap.len(), FRAME_CAP + 1);
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::json(401, &past_cap),
    ])
    .await;
    let mut t = open_session(&relay).await;
    let err = next_frame(&mut t).await.expect_err("the relay refused");
    assert_eq!(
        code_of(&err),
        "AUTH_TOKEN_INVALID",
        "one byte past the cap is not read, so the status map answers"
    );
}

/// An event that arrives in two chunks is one event, and the frame cap does
/// not fire on the way.
///
/// Both halves are deliberately larger than the square root of the cap, so
/// a cap check that multiplied the buffered length by the arriving one
/// instead of adding them would refuse this stream. That is the arithmetic
/// the check is made of, and it is invisible to any test whose chunks are
/// small.
#[tokio::test]
async fn an_event_split_across_chunks_is_reassembled_under_the_cap() {
    let event = format!(
        "id: 3\ndata: {}\n\n",
        serde_json::json!({"kind": "gap", "reason": "x".repeat(8_000)})
    );
    let (head, tail) = event.as_bytes().split_at(event.len() / 2);
    assert!(
        head.len() * tail.len() > FRAME_CAP,
        "the halves must be large enough to separate a product from a sum"
    );
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![
            hyper::body::Bytes::copy_from_slice(head),
            hyper::body::Bytes::copy_from_slice(tail),
        ]),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let (head, payload) = super::decode_frame(
        &next_frame(&mut t)
            .await
            .expect("the halves make one event")
            .expect("a frame"),
    )
    .expect("a decodable frame");
    assert_eq!(head.msg_kind, super::MsgKind::Error);
    let parsed = super::ErrorPayload::decode(&payload).expect("an ErrorPayload");
    assert_eq!(parsed.code, sunrise_error::ErrorCode::SyncCursorGap);
    assert_eq!(parsed.reason.len(), 8_000);
}

/// A relay that never sends a blank line cannot grow this buffer without
/// bound: past the frame cap the stream is dropped and the condition named.
#[tokio::test]
async fn an_event_that_never_terminates_is_refused_at_the_frame_cap() {
    let cap = FRAME_CAP;
    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![
            hyper::body::Bytes::from(vec![b'x'; cap - 4]),
            hyper::body::Bytes::from(vec![b'x'; 8]),
        ]),
    ])
    .await;
    let mut t = open_session(&relay).await;

    let err = next_frame(&mut t).await.expect_err("the cap fires");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("frame cap")),
        "{err}"
    );
    assert!(t.events.is_none(), "and the stream is dropped with it");
}

/// The cap is a ceiling, not a limit one byte lower: a stream that arrives
/// at exactly the cap is delivered.
///
/// Written against a keep-alive so the whole eight mebibytes are a single
/// terminated event that yields nothing to the driver — the cap is the
/// subject here, not the parsing.
#[tokio::test]
async fn a_stream_arriving_at_exactly_the_cap_is_not_refused() {
    let cap = FRAME_CAP;
    let mut comment = Vec::with_capacity(cap);
    comment.push(b':');
    comment.extend(std::iter::repeat_n(b'x', cap - 3));
    comment.extend_from_slice(b"\n\n");
    assert_eq!(comment.len(), cap);
    let (head, tail) = comment.split_at(cap - 4);

    let relay = relay::start(vec![
        relay::Canned::json(200, SESSION_OK),
        relay::Canned::stream(vec![
            hyper::body::Bytes::copy_from_slice(head),
            hyper::body::Bytes::copy_from_slice(tail),
        ]),
    ])
    .await;
    let mut t = open_session(&relay).await;

    assert!(
        next_frame(&mut t)
            .await
            .expect("exactly the cap is under the cap")
            .is_none(),
        "a keep-alive carries no frame, and the stream then ends"
    );
}
