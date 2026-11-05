//! The attachment routes against the loopback relay: upload, commit and
//! fetch, their caps and deadlines, and their refusals.

use super::*;

/// The blob surface is bounded too — and by a deadline of its own.
///
/// The third of the three body reads. It is not on the handshake's footing
/// — a blob fetch is driven by a caller who can be told it failed — but it
/// had the same missing bound, and the shared read is what makes the three
/// move together instead of each being remembered separately.
///
/// What it does *not* share is the value. This case is the one that says
/// so, from both sides: the fetch is still running well past
/// [`super::BODY_IDLE_TIMEOUT`], so a mutant passing the document deadline
/// here returns far too early and turns it red; and the diagnostic names
/// the fifteen seconds it actually waited, so a mutant moving
/// [`super::BLOB_IDLE_TIMEOUT`] without moving the constant's own case has
/// nowhere to hide either. An upper bound alone would have left both
/// passing.
///
/// This is the most expensive case in the file — it spends
/// [`super::BLOB_IDLE_TIMEOUT`] of real wall clock, by construction, since
/// it reads a real socket, which paused time would not wait for — and it is the only
/// place the blob route's own patience is observable end to end. The
/// mechanism itself is pinned in milliseconds against `Scripted` in `body_bounds`.
///
/// The double's media type says JSON where a blob would say octets;
/// `call_bytes` never reads it, and the stall is the subject.
#[tokio::test]
async fn a_stalled_blob_fetch_is_reported_rather_than_parking_the_caller() {
    let relay = relay::start(vec![relay::Canned::json_then_stall(
        200,
        "the first ciphertext",
    )])
    .await;
    let mut t = dial(&relay);

    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        t.blob_fetch(&[0x22; 16]),
    )
    .await
    .expect("the read is bounded by the client, not by this harness")
    .expect_err("the relay answered and then went quiet");

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
        "a stalled fetch is reported, so the caller can retry it: {err}"
    );
    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("no data for 15s")),
        "and the deadline that ran is the blob route's own, named in the diagnostic \
             rather than inferred from the clock: {err}"
    );
    let elapsed = started.elapsed();
    // A lower bound only: the diagnostic names the fifteen seconds that ran,
    // and the harness timeout bounds the wait.
    assert!(
        elapsed >= std::time::Duration::from_millis(11_000),
        "it waited an attachment's worth of patience, not a document's: two seconds \
             here is a minimum inter-frame arrival rate on a hundred megabytes, and with \
             no `Range` header and ten attempts it is an attachment that never arrives: \
             {elapsed:?}"
    );
}

/// An attachment larger than the *event* cap is still delivered.
///
/// The blob route's cap is the one argument in this module that cannot be
/// reached from its refusing side: [`super::MAX_BLOB_BYTES`] is a hundred
/// megabytes, and scripting that over loopback to watch it be refused would
/// cost more than the boundary is worth. So it is pinned from the other
/// side, which is the side that matters anyway — sizing this route by
/// [`super::MAX_EVENT_BYTES`] would refuse every attachment over eight
/// mebibytes, and the relay commits them up to a hundred megabytes
/// (`crates/sunrise-server/src/api/blobs/mod.rs#MAX_BLOB_BYTES`). A byte past the event cap
/// is enough to tell the two constants apart, and it is the cheapest body
/// that can.
///
/// The ciphertext is opaque by construction here: what is asserted is that
/// every byte the relay sent came back, which is what a content hash
/// downstream depends on.
#[tokio::test]
async fn an_attachment_larger_than_the_event_cap_is_still_fetched_whole() {
    let ciphertext = vec![0xA5u8; FRAME_CAP + 1];
    let relay = relay::start(vec![relay::Canned::bytes(200, &ciphertext)]).await;
    let mut t = dial(&relay);

    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.blob_fetch(&[0x33; 16]),
    )
    .await
    .expect("a bounded read returns")
    .expect("the relay served the blob")
    .expect("and it is there");

    assert_eq!(
        fetched.len(),
        FRAME_CAP + 1,
        "the blob route is not bounded by the event cap"
    );
    assert!(
        fetched.iter().all(|b| *b == 0xA5),
        "and every byte of it came back unaltered"
    );
}

/// A refused blob fetch is a document, and is capped like one.
///
/// `call_bytes` has the status in hand before it reads a byte, and the
/// bounds its caller passed describe the reply that caller asked for. A
/// `404` is not that reply. Reading it under the fetch's own bounds let a
/// relay answer "no such blob" with a hundred megabytes and have this
/// client hold all of it — [`super::SseTransport::blob_fetch`] would then
/// return `Ok(None)`, "not here, try later", having allocated a hundred
/// megabytes to be told so.
///
/// A body one byte past [`super::MAX_EVENT_BYTES`] is what tells the two
/// caps apart here, the same byte and the same reasoning as the upload
/// route's ack: under the blob cap it is read whole and the fetch answers
/// `Ok(None)`; under the event cap the read is abandoned and the fetch
/// reports it. This is the upload route's defect of the round before, one
/// status along on the download route rather than one caller along.
#[tokio::test]
async fn a_refused_blob_fetch_is_capped_as_a_document_not_as_an_attachment() {
    let oversized = format!(
        r#"{{"code":"SYNC_BLOB_NOT_FOUND","detail":"{}"}}"#,
        "x".repeat(FRAME_CAP)
    );
    assert!(
        oversized.len() > FRAME_CAP,
        "the refusal is past the event cap"
    );
    assert!(
        oversized.len() < super::MAX_BLOB_BYTES,
        "and far under the blob cap, so a fetch reading it as content reads it whole"
    );
    let relay = relay::start(vec![relay::Canned::json(404, &oversized)]).await;
    let mut t = dial(&relay);

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.blob_fetch(&[0x44; 16]),
    )
    .await
    .expect("a capped read returns")
    .expect_err("a refusal past the document cap is not read");

    assert!(
        matches!(&err, TransportError::Unavailable(_)),
        "the relay sent more than a problem document is allowed to be, which is the \
             relay's failure rather than a missing blob: {err}"
    );
}

/// And a refused blob fetch is waited for on the document deadline.
///
/// The deadline half of the same split, and the cheaper half to watch: the
/// document bound expires in two seconds where the attachment bound would
/// hold this caller for fifteen. That difference is not one wait.
/// `MAX_FETCHES_PER_DRAIN` is four and a drain's fetches run one after
/// another, and the drain is awaited inline in the sync driver's session
/// pump — so four stalled refusals are the difference between eight seconds
/// and a minute of a session that is reading nothing.
///
/// The relay answers `503` rather than `404` so the refusal reaches
/// `refuse` rather than the deliberate-`404` arm above it; what is under
/// test is the read, and both arms are downstream of it.
#[tokio::test]
async fn a_stalled_blob_refusal_is_cut_off_on_the_document_deadline() {
    let relay = relay::start(vec![relay::Canned::json_then_stall(
        503,
        r#"{"code":"RELAY_STORAGE"#,
    )])
    .await;
    let mut t = dial(&relay);

    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.blob_fetch(&[0x55; 16]),
    )
    .await
    .expect("the read is bounded by the client, not by this harness")
    .expect_err("the relay refused and then went quiet");

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("no data for 2s")),
        "a problem document is a document on this route as on every other, so it is \
             the two seconds that ran and not the attachment's fifteen: {err}"
    );
    let elapsed = started.elapsed();
    // A lower bound only: the diagnostic names the two seconds that ran,
    // and the harness timeout bounds the wait.
    assert!(
        elapsed >= std::time::Duration::from_millis(1_500),
        "and it came back on that deadline and not before it, which is what decides \
             what a drain of four of these costs the session pump: {elapsed:?}"
    );
}

/// The whole attachment upload: an id from the relay, a signed chunk, and a
/// commit the caller can address the blob by.
#[tokio::test]
async fn a_blob_upload_signs_its_chunks_and_reads_back_the_commit() {
    let relay = relay::start(vec![
        relay::Canned::json(200, r#"{"upload_id":"up-1"}"#),
        relay::Canned::empty(204),
        relay::Canned::json(
            200,
            &format!(
                r#"{{"blob_id":"blb_{}","size_bytes":9,"chunk_count":1}}"#,
                "0f".repeat(16)
            ),
        ),
    ])
    .await;
    let mut t = dial(&relay);

    let upload = t
        .blob_init(&[0xabu8; 16], 1, 9)
        .await
        .expect("an upload id");
    assert_eq!(upload, "up-1");
    t.blob_put_chunk(&upload, 0, b"sealed-chunk-bytes")
        .await
        .expect("the chunk lands");
    let commit = t
        .blob_finalize(&upload, &[0x22u8; 32], &[[0x33u8; 32]])
        .await
        .expect("a commit");
    assert_eq!(commit.blob_id, [0x0fu8; 16]);
    assert_eq!(commit.size_bytes, 9);
    assert_eq!(commit.chunk_count, 1);

    let seen = relay.seen();
    assert_eq!(seen[0].path, "/api/v1/blobs/init");
    assert_json_binding(&seen[0]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen[0].body).expect("JSON")["chunk_count"],
        1
    );
    assert_eq!(seen[1].method, "PUT");
    assert_eq!(seen[1].path, "/api/v1/blobs/up-1/0");
    assert_eq!(seen[1].body, b"sealed-chunk-bytes");
    assert_byte_binding(&seen[1]);
    assert_eq!(
        seen[1].header("content-type"),
        Some("application/octet-stream"),
        "a chunk is opaque ciphertext, and says so"
    );
    assert_eq!(seen[2].path, "/api/v1/blobs/finalize");
    assert_json_binding(&seen[2]);
    assert_eq!(
        seen[2].header("content-type"),
        Some("application/json"),
        "a request with a JSON body advertises one"
    );
}

/// A chunk upload's *ack* is read under the event cap, not the blob cap.
///
/// `call_bytes` has two callers and only one of them receives content. The
/// cap used to be chosen inside `call_bytes`, and the comment that chose it
/// named `blob_fetch` alone — so the reply to every chunk `PUT`, which is
/// an empty `204` or a short JSON document, was read under a hundred-
/// megabyte ceiling. A relay answering an upload with a hundred megabytes
/// made this client hold it, on the more frequently driven of the two
/// routes.
///
/// A body one byte past [`super::MAX_EVENT_BYTES`] is the cheapest thing
/// that tells the two constants apart on this route: under the blob cap it
/// is read and the upload succeeds, under the event cap it is refused. The
/// same byte, and the same reasoning, as the fetch route's case — pointed
/// the other way, because here the cap's *refusing* side is the one that
/// matters.
#[tokio::test]
async fn a_chunk_upload_ack_past_the_event_cap_is_refused_rather_than_held() {
    let oversized = format!(r#"{{"ok":"{}"}}"#, "x".repeat(FRAME_CAP));
    assert!(oversized.len() > FRAME_CAP, "the ack is past the event cap");
    let relay = relay::start(vec![relay::Canned::json(200, &oversized)]).await;
    let mut t = dial(&relay);

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.blob_put_chunk("up-1", 0, b"sealed-chunk-bytes"),
    )
    .await
    .expect("a capped read returns")
    .expect_err("the ack is past the cap this route reads under");

    assert!(
        matches!(&err, TransportError::Unavailable(_)),
        "the relay sent more than an ack is allowed to be, which is the relay's \
             failure: {err}"
    );

    let seen = relay.seen();
    assert_eq!(
        seen[0].method, "PUT",
        "and the route reached was the upload"
    );
    assert_eq!(seen[0].path, "/api/v1/blobs/up-1/0");
}

/// A chunk upload's ack is waited for on the *document* deadline.
///
/// The other half of the bounds `blob_put_chunk` chooses, and the half
/// nothing reached: the cap it passes is pinned by the case above, and its
/// deadline was asserted nowhere at all, so a mutant passing
/// [`super::BLOB_IDLE_TIMEOUT`] here survived the whole suite. It is the
/// same omission one site along as the two the previous round carried a
/// lower bound to, on the more frequently driven of `call_bytes`' two
/// callers — an upload writes one of these per chunk.
///
/// Pinned by the diagnostic rather than by a timing window alone, because
/// the window that separates two seconds from fifteen is exactly the window
/// a loaded machine eats. The window is asserted too, from below: an upper
/// bound on its own leaves every shorter value passing.
#[tokio::test]
async fn a_stalled_chunk_upload_ack_is_cut_off_on_the_document_deadline() {
    let relay = relay::start(vec![relay::Canned::json_then_stall(200, r#"{"ok"#)]).await;
    let mut t = dial(&relay);

    let started = tokio::time::Instant::now();
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        t.blob_put_chunk("up-1", 0, b"sealed-chunk-bytes"),
    )
    .await
    .expect("the read is bounded by the client, not by this harness")
    .expect_err("the relay acked and then went quiet");

    assert!(
        matches!(&err, TransportError::Unavailable(m) if m.contains("no data for 2s")),
        "an ack is a document, so the deadline that ran is the document one — the \
             attachment's fifteen seconds belongs to the body an attachment is: {err}"
    );
    let elapsed = started.elapsed();
    // A lower bound only: the diagnostic names the two seconds that ran,
    // and the harness timeout bounds the wait.
    assert!(
        elapsed >= std::time::Duration::from_millis(1_500),
        "and it came back on that deadline and not before it: {elapsed:?}"
    );
}

/// A commit the relay under-described still names its blob, and reads as
/// zero rather than as a size nobody sent.
///
/// `size_bytes` and `chunk_count` are read with `unwrap_or(0)`, and
/// `chunk_count` narrows to `u32` first. All three fallbacks are reachable
/// from a relay that answers the route and omits or overflows a field, and
/// none of them was executed: the blob id is the only part of the commit
/// this build refuses to guess at, which is the distinction the fallbacks
/// encode.
#[tokio::test]
async fn a_commit_with_fields_missing_or_too_large_reads_as_zero() {
    let relay = relay::start(vec![
        relay::Canned::json(200, &format!(r#"{{"blob_id":"blb_{}"}}"#, "0f".repeat(16))),
        relay::Canned::json(
            200,
            &format!(
                r#"{{"blob_id":"blb_{}","size_bytes":9,"chunk_count":4294967297}}"#,
                "0f".repeat(16)
            ),
        ),
        relay::Canned::json(200, r#"{"size_bytes":9,"chunk_count":1}"#),
    ])
    .await;
    let mut t = dial(&relay);

    let bare = t
        .blob_finalize("up-1", &[0x22u8; 32], &[[0x33u8; 32]])
        .await
        .expect("a commit naming its blob");
    assert_eq!(bare.blob_id, [0x0fu8; 16]);
    assert_eq!(bare.size_bytes, 0, "a size nobody sent is not invented");
    assert_eq!(bare.chunk_count, 0);

    let wide = t
        .blob_finalize("up-1", &[0x22u8; 32], &[[0x33u8; 32]])
        .await
        .expect("a commit naming its blob");
    assert_eq!(wide.size_bytes, 9);
    assert_eq!(
        wide.chunk_count, 0,
        "a count past u32 is refused, not truncated into a plausible one: \
             the fixture is 2^32 + 1, so a narrowing cast would read 1"
    );

    let err = t
        .blob_finalize("up-1", &[0x22u8; 32], &[[0x33u8; 32]])
        .await
        .expect_err("a commit with no blob id names nothing");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("blob id")),
        "{err}"
    );
}

/// A fetch returns the ciphertext, and the relay's single 404 — "no such
/// blob", "not yours" and "not finished yet" answered alike, so the route
/// is not an oracle for whether another account holds a ciphertext — is
/// "not here, try later" rather than a failure.
#[tokio::test]
async fn a_blob_fetch_separates_the_bytes_from_the_deliberate_404() {
    let relay = relay::start(vec![
        relay::Canned::bytes(200, b"sealed"),
        relay::Canned::empty(404),
        relay::Canned::json(503, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#),
    ])
    .await;
    let mut t = dial(&relay);
    let blob = [0x0fu8; 16];

    assert_eq!(
        t.blob_fetch(&blob).await.expect("the bytes"),
        Some(b"sealed".to_vec())
    );
    assert_eq!(
        t.blob_fetch(&blob).await.expect("not an error"),
        None,
        "the deliberate 404 is not here, try later"
    );
    let err = t
        .blob_fetch(&blob)
        .await
        .expect_err("an outage is not a 404");
    assert_eq!(code_of(&err), "RELAY_STORAGE_UNAVAILABLE");

    let seen = relay.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].path,
        format!("/api/v1/blobs/blb_{}", "0f".repeat(16))
    );
    assert!(seen[0].body.is_empty(), "a bodiless GET sends no body");
    assert_eq!(
        seen[0].header("content-type"),
        None,
        "and advertises no media type it is not sending, which is what \
             `content_type: None` on `call_bytes` is for"
    );
    assert_byte_binding(&seen[0]);
}

/// Every blob operation reports a refusal rather than a quiet success.
///
/// The chunk `PUT` is the one that matters most: it returns `Ok(())` on
/// success, so a refusal read as success would leave `finalize` asking the
/// relay to commit chunks it never received.
#[tokio::test]
async fn every_blob_operation_reports_a_refusal_rather_than_a_quiet_success() {
    let refused = relay::Canned::json(503, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#);
    let relay = relay::start(vec![refused.clone(), refused.clone(), refused]).await;
    let mut t = dial(&relay);

    assert_eq!(
        code_of(&t.blob_init(&[1u8; 16], 1, 9).await.expect_err("refused")),
        "RELAY_STORAGE_UNAVAILABLE"
    );
    assert_eq!(
        code_of(
            &t.blob_put_chunk("up-1", 0, b"x")
                .await
                .expect_err("refused")
        ),
        "RELAY_STORAGE_UNAVAILABLE"
    );
    assert_eq!(
        code_of(
            &t.blob_finalize("up-1", &[2u8; 32], &[[3u8; 32]])
                .await
                .expect_err("refused")
        ),
        "RELAY_STORAGE_UNAVAILABLE"
    );
}

/// A relay that answers an upload with no id at all is a malformed
/// exchange, not an upload under the empty string.
#[tokio::test]
async fn an_upload_the_relay_named_nothing_is_a_protocol_error() {
    let relay = relay::start(vec![
        relay::Canned::json(200, "{}"),
        relay::Canned::json(200, r#"{"size_bytes":9}"#),
    ])
    .await;
    let mut t = dial(&relay);

    let err = t
        .blob_init(&[1u8; 16], 1, 9)
        .await
        .expect_err("no upload id");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("upload_id")),
        "{err}"
    );
    let err = t
        .blob_finalize("up-1", &[2u8; 32], &[[3u8; 32]])
        .await
        .expect_err("no blob id");
    assert!(
        matches!(&err, TransportError::Protocol(m) if m.contains("blob id")),
        "{err}"
    );
}
