//! The bounds on a response's head and body: `read_body` and `await_head`
//! driven directly, at deadlines and caps of their own, and the shipped
//! values pinned.

use super::*;

/// A response body this module drives directly, one frame at a time.
///
/// The relay double in `relay` proves each *route* reaches [`read_body`]; this
/// proves what `read_body` itself does, at boundaries a relay would have to
/// move a hundred megabytes or wait whole seconds to reach. Driving the
/// body here rather than over loopback is also what lets these cases name
/// their own deadline: `read_body` takes one as a parameter, so the
/// mechanism can be exercised in milliseconds and the shipped value pinned
/// separately, instead of every case paying for two real seconds.
#[derive(Debug)]
struct Scripted {
    /// Each frame, and how long the body withholds it first.
    rest: std::collections::VecDeque<(std::time::Duration, hyper::body::Bytes)>,
    /// Withhold the *end* of the body too, indefinitely.
    stall: bool,
    /// Fail instead of ending, once every frame has been delivered.
    abort: bool,
    /// A trailer frame to send after the last data frame, if any.
    ///
    /// A body may end with a `Frame::trailers` rather than with data, and
    /// no double in these tests could produce one until this field existed:
    /// both of them built `Frame::data` exclusively, which left
    /// [`read_body`]'s trailer arm unreachable by the fixtures rather than
    /// by the code.
    trailer: Option<hyper::HeaderMap>,
    /// The gap currently being waited out, if any.
    sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

/// A body that yields `frames` — each a `(gap, data)` pair — and then ends,
/// stalls or breaks.
fn scripted(frames: &[(std::time::Duration, &str)], stall: bool, abort: bool) -> Scripted {
    Scripted {
        rest: frames
            .iter()
            .map(|(gap, data)| (*gap, hyper::body::Bytes::copy_from_slice(data.as_bytes())))
            .collect(),
        stall,
        abort,
        trailer: None,
        sleep: None,
    }
}

/// The same, ending with one trailer frame after the last data frame.
fn scripted_with_trailer(
    frames: &[(std::time::Duration, &str)],
    name: &'static str,
    value: &'static str,
) -> Scripted {
    let mut trailer = hyper::HeaderMap::new();
    trailer.insert(
        hyper::header::HeaderName::from_static(name),
        hyper::header::HeaderValue::from_static(value),
    );
    Scripted {
        trailer: Some(trailer),
        ..scripted(frames, false, false)
    }
}

impl hyper::body::Body for Scripted {
    type Data = hyper::body::Bytes;
    /// Not `Infallible`, for the same reason the relay's body is not: a
    /// body that cannot fail forecloses the error arm by construction.
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<hyper::body::Bytes>, std::io::Error>>>
    {
        use std::future::Future as _;
        use std::task::Poll;

        let this = self.get_mut();
        loop {
            if let Some(sleep) = this.sleep.as_mut() {
                match sleep.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(()) => this.sleep = None,
                }
            }
            match this.rest.front_mut() {
                // A gap still owed. Arm it, clear it, and go round, so the
                // timer registers its waker before this returns `Pending`.
                Some(owed) if !owed.0.is_zero() => {
                    let gap = std::mem::replace(&mut owed.0, std::time::Duration::ZERO);
                    this.sleep = Some(Box::pin(tokio::time::sleep(gap)));
                }
                Some(_) => {
                    let (_, data) = this.rest.pop_front().expect("a frame was just there");
                    return Poll::Ready(Some(Ok(hyper::body::Frame::data(data))));
                }
                // Before the end, and before a stall or a break: a
                // trailer is the last frame of a body that ended well.
                None if this.trailer.is_some() => {
                    let trailer = this.trailer.take().expect("just checked");
                    return Poll::Ready(Some(Ok(hyper::body::Frame::trailers(trailer))));
                }
                None if this.stall => {
                    // A real timer rather than a bare `Pending`: a body that
                    // returns `Pending` without registering a waker is one
                    // the runtime is entitled never to poll again, and the
                    // stall would then be an artefact of this double.
                    let held = this.sleep.get_or_insert_with(|| {
                        Box::pin(tokio::time::sleep(std::time::Duration::from_secs(3_600)))
                    });
                    return match held.as_mut().poll(cx) {
                        Poll::Pending => Poll::Pending,
                        Poll::Ready(()) => Poll::Ready(None),
                    };
                }
                None if this.abort => {
                    this.abort = false;
                    return Poll::Ready(Some(Err(std::io::Error::other(
                        "the peer broke the body",
                    ))));
                }
                None => return Poll::Ready(None),
            }
        }
    }
}

/// The deadline these cases drive `read_body` with.
///
/// Not [`BODY_IDLE_TIMEOUT`]: the mechanism and the shipped value are
/// separate claims, and tying them together would say nothing the value's
/// own case does not.
///
/// The cases that time a read run on paused time (`start_paused`), so a
/// timer fires on its own deadline and `elapsed()` is exactly the sum of
/// the waits the read made, however loaded the machine is.
const TEST_IDLE: std::time::Duration = std::time::Duration::from_millis(300);

/// [`read_body`]'s deadline bounds a *stall*, not a download: a body that
/// keeps arriving keeps resetting it, however long the whole read takes.
///
/// The load-bearing property of the whole helper. Four frames, each
/// withheld for a third of the deadline, so every gap is comfortably inside
/// it while the total is a third longer than it.
///
/// A deadline wrapped around the whole read instead of around each frame
/// passes every other SSE case and refuses this one — and with it
/// any attachment large enough to take longer than [`BODY_IDLE_TIMEOUT`] to
/// arrive, which on the blob route is most of them: [`MAX_BLOB_BYTES`] is a
/// hundred megabytes and the relay serves it one chunk per poll. That is
/// why the bound is idle rather than total, and this is the case that says
/// so.
#[tokio::test(start_paused = true)]
async fn a_body_that_keeps_arriving_is_not_cut_off_by_the_idle_deadline() {
    let gap = TEST_IDLE / 3;
    let started = tokio::time::Instant::now();

    let read = super::read_body(
        scripted(
            &[(gap, "one "), (gap, "two "), (gap, "three "), (gap, "four")],
            false,
            false,
        ),
        1024,
        TEST_IDLE,
    )
    .await
    .expect("every gap is inside the deadline");

    assert_eq!(
        String::from_utf8(read).expect("the frames were UTF-8"),
        "one two three four",
        "and every frame is there, in order"
    );
    assert!(
        gap * 4 > TEST_IDLE,
        "the script is longer than the deadline"
    );
    assert_eq!(
        started.elapsed(),
        gap * 4,
        "the read outlasted the deadline, which is the whole point of it, and took \
             exactly the four gaps it was given"
    );
}

/// And a body that *stops* arriving is cut off.
#[tokio::test(start_paused = true)]
async fn a_body_that_stops_arriving_is_cut_off_at_the_idle_deadline() {
    let started = tokio::time::Instant::now();

    let err = super::read_body(
        scripted(
            &[(std::time::Duration::ZERO, r#"{"code":"AUTH_DEV"#)],
            true,
            false,
        ),
        1024,
        TEST_IDLE,
    )
    .await
    .expect_err("the rest of the body never came");

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
        "a stall is named as one rather than reported as a broken read: {err}"
    );
    assert_eq!(
        started.elapsed(),
        TEST_IDLE,
        "and it waited the deadline out rather than giving up on the first `Pending`, \
             and no longer than the deadline"
    );
}

/// The shipped deadlines and the blob cap are these numbers.
///
/// Spelled out rather than read from the constants they pin, for the reason
/// [`FRAME_CAP`] is: a case sized from the constant moves with it and
/// asserts the comparison instead of the value. The cases above exercise
/// the mechanism at a deadline of their own, so without this one a mutant
/// could move either deadline anywhere and nothing would notice — which
/// matters, because "anywhere" includes long enough to park the handshake
/// route for the rest of the session.
///
/// The two deadlines are asserted together, and their *inequality* with
/// them, because what this pair pins is not either value alone: it is that
/// one value does not serve both kinds of body. [`BODY_IDLE_TIMEOUT`]
/// applied to a fetched attachment is a minimum inter-frame arrival rate on
/// a hundred megabytes, and [`BLOB_IDLE_TIMEOUT`] applied to the handshake
/// would slacken the one route with nothing above it. Both bodies reach the
/// blob route, so the split there is by status rather than by route.
///
/// [`MAX_BLOB_BYTES`] is the relay's own ceiling
/// (`crates/sunrise-server/src/api/blobs/mod.rs:81#MAX_BLOB_BYTES`) restated on the side that
/// allocates it. Lowering it would refuse attachments the relay accepted,
/// which is a failure no caller could diagnose from here. It bounds a
/// fetched attachment and nothing else; a refused fetch is capped by
/// [`MAX_EVENT_BYTES`], which `the_frame_cap_is_the_documented_eight_mebibytes`
/// pins.
#[test]
fn the_body_bounds_are_the_numbers_they_are_documented_as() {
    assert_eq!(super::BODY_IDLE_TIMEOUT, std::time::Duration::from_secs(2));
    assert_eq!(super::BLOB_IDLE_TIMEOUT, std::time::Duration::from_secs(15));
    assert!(
        super::BLOB_IDLE_TIMEOUT > super::BODY_IDLE_TIMEOUT,
        "the route that carries content waits longer than the routes that carry documents"
    );
    assert_eq!(super::MAX_BLOB_BYTES, 100 * 1000 * 1000);
}

/// The head bounds are pinned for the reason the body bounds are: every
/// relay-backed case that spends one does so in real wall clock, so
/// the values themselves are asserted here rather than inferred from a
/// timing window.
///
/// The ordering is the half that states a decision. A chunk upload's head
/// cannot arrive before the chunk has been sent, so the route whose
/// request carries a quarter of a mebibyte waits longer than the routes
/// whose requests carry a document — and neither head bound is the body's
/// two seconds, which would refuse an ordinary slow connect.
#[test]
fn the_head_bounds_are_the_numbers_they_are_documented_as() {
    assert_eq!(super::HEAD_TIMEOUT, std::time::Duration::from_secs(15));
    assert_eq!(
        super::CHUNK_HEAD_TIMEOUT,
        std::time::Duration::from_secs(60)
    );
    assert!(
        super::CHUNK_HEAD_TIMEOUT > super::HEAD_TIMEOUT,
        "the route whose request is a chunk waits longer for its head"
    );
    assert!(
        super::HEAD_TIMEOUT > super::BODY_IDLE_TIMEOUT,
        "a head bound contains the connect, which the body's idle bound never does"
    );
}

/// A head that never comes is reported on the bound, and names it.
///
/// The mechanism, in milliseconds, against a future that never resolves —
/// which is exactly what `Client::request` is while a peer that finished
/// TLS says nothing. The relay-backed case further down proves the wiring
/// on the handshake route at the shipped value; this one proves the bound
/// is the one it was given rather than some other, from both sides.
#[tokio::test]
async fn a_head_that_never_comes_is_reported_on_its_own_bound() {
    let bound = std::time::Duration::from_millis(200);
    let started = tokio::time::Instant::now();
    let err = super::await_head(
        std::future::pending::<Result<hyper::Response<()>, std::convert::Infallible>>(),
        bound,
    )
    .await
    .expect_err("a head that never arrived is not a response");
    let elapsed = started.elapsed();

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m == "no response head within 200ms"),
        "unavailable, which the driver turns into a disconnect and a backoff, and naming \
             the bound that ran: {err}"
    );
    assert!(
        elapsed >= bound && elapsed < std::time::Duration::from_secs(5),
        "it came back on the bound it was given: {elapsed:?}"
    );
}

/// A head that does come is passed through untouched, and a failed dial
/// is still reported as the failure it was.
///
/// The two arms the deadline must not disturb: wrapping the request must
/// not turn a prompt answer into a refusal, nor a refused connection into
/// a timeout the caller would misreport.
#[tokio::test]
async fn a_head_that_comes_or_a_dial_that_fails_is_reported_as_itself() {
    let bound = std::time::Duration::from_secs(5);
    let response = super::await_head(
        std::future::ready(Ok::<_, std::convert::Infallible>(
            hyper::Response::builder()
                .status(204)
                .body(())
                .expect("a response"),
        )),
        bound,
    )
    .await
    .expect("a head that arrived in time is the response");
    assert_eq!(response.status(), hyper::StatusCode::NO_CONTENT);

    let err = super::await_head(
        std::future::ready(Err::<hyper::Response<()>, _>("connection refused")),
        bound,
    )
    .await
    .expect_err("a dial that failed is not a response");
    assert!(
        matches!(&err, TransportError::Unavailable(m) if m == "connection refused"),
        "the dial's own error, not a deadline it never reached: {err}"
    );
}

/// The cap is a ceiling, not a limit one byte lower — and what it refuses
/// is discarded rather than short-read.
///
/// Both sides of the boundary, because a cap that refused everything would
/// pass a case written only against the oversized half. Ten bytes against a
/// ten-byte cap are delivered; the same ten against nine are not delivered
/// truncated, which is the answer that matters: a caller handed nine bytes
/// of a ten-byte document cannot tell them from a document.
#[tokio::test]
async fn a_body_at_the_cap_is_read_and_one_byte_past_it_is_refused_whole() {
    let at_cap = super::read_body(
        scripted(&[(std::time::Duration::ZERO, "0123456789")], false, false),
        10,
        TEST_IDLE,
    )
    .await
    .expect("exactly the cap is under the cap");
    assert_eq!(at_cap, b"0123456789", "and arrives whole");

    let err = super::read_body(
        scripted(&[(std::time::Duration::ZERO, "0123456789")], false, false),
        9,
        TEST_IDLE,
    )
    .await
    .expect_err("one byte past the cap is not read");
    assert!(
        matches!(&err, TransportError::Unavailable(_)),
        "the peer sent more than was allowed, which is the peer's failure: {err}"
    );
}

/// A body that breaks part-way through is reported, not short-read.
#[tokio::test]
async fn a_body_that_breaks_part_way_through_is_reported_rather_than_short_read() {
    let err = super::read_body(
        scripted(
            &[(std::time::Duration::ZERO, "half a document")],
            false,
            true,
        ),
        1024,
        TEST_IDLE,
    )
    .await
    .expect_err("the body broke before it ended");
    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("broke")),
        "the peer's own failure comes through: {err}"
    );
}

/// A body that is *still arriving* is cut off at the first gap that
/// crosses the deadline.
///
/// The case that tells "bounded a stall" apart from "aborted a slow
/// download", and neither of the two cases above reaches it: one has every
/// gap inside the deadline and ends, the other stops after one frame and
/// never resumes. This one makes progress, keeps making it, and then takes
/// one gap too long — which is what a real link does when a radio hands
/// over or a retransmit backs off, and is exactly the shape that makes the
/// deadline's *value* a decision rather than a formality. A number below
/// what a healthy link goes quiet for turns this case into every download,
/// which is why the blob route has [`BLOB_IDLE_TIMEOUT`] rather than
/// [`BODY_IDLE_TIMEOUT`].
///
/// Three assertions, because three different things could be true. It
/// errors, so the gap was not simply waited out. It errors as a stall
/// rather than as a broken body. And it errors *late* — after the first,
/// inside-deadline gap was absorbed — so the deadline was re-armed per
/// frame rather than fired on the first pause.
#[tokio::test(start_paused = true)]
async fn a_body_still_arriving_is_cut_off_at_the_first_gap_that_crosses_the_deadline() {
    let inside = TEST_IDLE / 3;
    let across = TEST_IDLE * 2;
    let started = tokio::time::Instant::now();

    let err = super::read_body(
        scripted(
            &[
                (std::time::Duration::ZERO, "one "),
                (inside, "two "),
                (across, "three"),
            ],
            false,
            false,
        ),
        1024,
        TEST_IDLE,
    )
    .await
    .expect_err("the third frame was one gap too late");

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
        "a body that stopped arriving is named as one, though it had been arriving: {err}"
    );
    // On paused time both claims are one equality: the inside gap was
    // absorbed rather than counted against the deadline (a read cut at the
    // first pause would stop at `TEST_IDLE`), and the deadline then ran
    // from the second frame rather than the long gap being waited out
    // (which would stop at `inside + across`).
    assert!(
        TEST_IDLE + inside < inside + across,
        "the two outcomes differ"
    );
    assert_eq!(
        started.elapsed(),
        inside + TEST_IDLE,
        "the gap inside the deadline was absorbed rather than counted against it, and \
             the read was cut a deadline after the last frame rather than waiting the long \
             gap out, which is what an unbounded read would have done"
    );
}

/// A trailer frame ends the body without being appended to it.
///
/// [`read_body`] drops a frame that is not data, and the comment at that
/// arm says why: a trailer's encoding appended to a document is a document
/// nobody can parse. Nothing exercised the arm until this case — both
/// doubles in these tests built `Frame::data` exclusively — so a mutant
/// treating a trailer as data survived the whole suite. That is the same
/// shape as the hard-coded status which once made a failed refusal read
/// unscriptable: unreachable by the fixture, not by the code.
///
/// The body is a problem document, because that is the read whose bytes are
/// parsed straight afterwards, and therefore the one where an extra frame
/// would do damage rather than merely be present.
#[tokio::test]
async fn a_trailer_frame_ends_the_body_without_being_appended_to_it() {
    let document = r#"{"code":"AUTH_DEVICE_SIG_INVALID"}"#;

    let read = super::read_body(
        scripted_with_trailer(
            &[(std::time::Duration::ZERO, document)],
            "x-sunrise-checksum",
            "d0a1",
        ),
        1024,
        TEST_IDLE,
    )
    .await
    .expect("a trailer ends a body; it does not break one");

    assert_eq!(
        String::from_utf8(read).expect("the frame was UTF-8"),
        document,
        "the document is exactly what the data frames carried, with none of the \
             trailer in it"
    );
}
