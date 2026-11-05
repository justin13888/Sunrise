//! The on-demand half of the byte path (issue #227): an attachment over the
//! auto-fetch threshold, which reaches a second device only when somebody asks
//! for it, and the Cancel that abandons that request.

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    AttachError, AttachmentFetchState, Clock, Core, SystemClock, AUTO_FETCH_MAX_BYTES,
};
use sunrise_e2e::chaos::{ToxicConfig, DEFAULT_FUZZ_SEED};
use sunrise_e2e::{
    open_paired_core, open_paired_core_with_factory, open_synced_core, spawn_relay,
    toxic_ws_factory, wait_live, wait_tasks_converge,
};
use sunrise_id::EntityRef;

use super::{a_task, POLL, ROOT, TIMEOUT};

/// The property issue #227 is about: an attachment past the auto-fetch
/// threshold reaches a second device when, and only when, somebody asks.
///
/// Both halves are asserted, and the negative half is the one that was true
/// before this existed. B holds the metadata, is live, has had every drain the
/// session offers, and does **not** have the bytes — that is the threshold
/// working. Then `fetch_attachment` returns and the same file reads back byte
/// for byte, which is the part that did not exist.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_over_the_auto_fetch_threshold_arrives_when_it_is_asked_for() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    // A fault-free toxic link, for its count of B's resync rounds.
    let (fb, b_link) = toxic_ws_factory(addr, ToxicConfig::passthrough(), DEFAULT_FUZZ_SEED);
    let b = open_paired_core_with_factory(dir_b.path(), &a, addr, Arc::clone(&clock), fb).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let task = a_task(&a, "Send the surveyor the floor plan").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // One byte over. A comfortable margin would pass against an off-by-one in
    // either direction, and the threshold is the whole subject here.
    let bytes = an_over_threshold_file();
    let att = a
        .attach_file(
            task,
            "floor-plan.pdf".into(),
            "application/pdf".into(),
            &bytes,
        )
        .await
        .expect("attach the file");
    assert!(
        att.size_bytes > AUTO_FETCH_MAX_BYTES,
        "the fixture must be past the threshold it is testing"
    );

    // B learns the attachment exists. Polled on the metadata rather than the
    // bytes, because the bytes are what must *not* arrive.
    wait_attachment_known(&b, att.id, TIMEOUT).await;

    // The threshold, working. Two complete resync rounds of B's, each with
    // its blob drain, observed rather than slept for, so this is "B had every
    // chance and declined" rather than "B has not got round to it".
    let rounds = b_link.subscribes_sent();
    b_link
        .wait_until("two resync rounds on B", TIMEOUT, |h| {
            h.subscribes_sent() >= rounds + 2
        })
        .await;
    assert!(
        !b.attachment_is_local(&att).expect("locality"),
        "an attachment over the threshold must never be fetched unasked"
    );
    assert_eq!(
        b.attachment_fetch_state(att.id).expect("state"),
        AttachmentFetchState::Idle,
        "and no request should have been invented on B's behalf"
    );

    // The route that did not exist.
    download(&b, att.id, TIMEOUT)
        .await
        .expect("the download a client's Download button asks for");
    assert_eq!(
        b.attachment_bytes(att.id).await.expect("read it back"),
        bytes,
        "B must reassemble exactly what A attached"
    );
    assert_eq!(
        b.attachment_fetch_state(att.id).expect("state"),
        AttachmentFetchState::Idle,
        "a finished download leaves no request behind to advertise"
    );

    // Asking again for something already here is a no-op, not a second
    // download: a client redraws a row without tracking what it holds.
    download(&b, att.id, TIMEOUT).await.expect("already here");

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}

/// "The 'Cancel' button during transfer aborts and marks the attachment
/// `partial: true` in cache", and "re-tapping a `partial: true` attachment
/// retries from byte 0" — over a real relay, or rather over the absence of one.
///
/// The relay is killed once B holds the metadata, which makes the timing
/// deterministic: the request cannot complete, so it is reliably outstanding
/// when Cancel arrives. It also tests the thing that matters most about Cancel
/// — that it works when the network does not. A cancel that needed the relay to
/// acknowledge it would hang exactly here, in the state a user actually presses
/// the button in.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_download_marks_it_partial_and_the_next_press_restarts_it() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let task = a_task(&a, "Countersign the survey").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    let bytes = an_over_threshold_file();
    let att = a
        .attach_file(task, "survey.pdf".into(), "application/pdf".into(), &bytes)
        .await
        .expect("attach the file");
    wait_attachment_known(&b, att.id, TIMEOUT).await;

    // From here nothing can be fetched, by construction.
    relay.abort();

    let waiting = tokio::spawn({
        let b = Arc::clone(&b);
        async move { b.fetch_attachment(att.id).await }
    });
    wait_fetch_state(&b, att.id, AttachmentFetchState::Requested, TIMEOUT).await;

    b.cancel_attachment_fetch(att.id).expect("press Cancel");
    let outcome = tokio::time::timeout(TIMEOUT, waiting)
        .await
        .expect("Cancel must release the waiting call, not leave it hanging")
        .expect("the waiting task did not panic");
    assert!(
        matches!(outcome, Err(AttachError::FetchCancelled { .. })),
        "Cancel must release the caller with its own answer, not a timeout: {outcome:?}"
    );
    assert_eq!(
        b.attachment_fetch_state(att.id).expect("state"),
        AttachmentFetchState::Partial,
        "the document's `partial: true`"
    );
    assert!(
        !b.attachment_is_local(&att).expect("locality"),
        "an abandoned transfer leaves no half-written blob claiming to be the file"
    );

    // The second press. A `partial` row that could not be moved back would make
    // Cancel a one-way door, which is indistinguishable from the feature never
    // having existed.
    let again = tokio::spawn({
        let b = Arc::clone(&b);
        async move { b.fetch_attachment(att.id).await }
    });
    wait_fetch_state(&b, att.id, AttachmentFetchState::Requested, TIMEOUT).await;
    b.cancel_attachment_fetch(att.id).expect("release the test");
    let _ = tokio::time::timeout(TIMEOUT, again)
        .await
        .expect("the second call resolves too")
        .expect("the second task did not panic");

    a.shutdown().await;
    b.shutdown().await;
}

/// `fetch_attachment` under a deadline.
///
/// The call itself has none by design — the Cancel button is the timeout, and
/// a client holds one — but a test does not, and a request that is never
/// drained would otherwise hang the suite instead of failing it. That is not
/// hypothetical: it is exactly what re-applying the threshold to
/// `requested_blob_fetches` does, which is the mutation this case exists to
/// catch.
async fn download(core: &Core, id: EntityRef, timeout: Duration) -> Result<(), AttachError> {
    tokio::time::timeout(timeout, core.fetch_attachment(id))
        .await
        .expect("the download neither finished nor failed within the deadline")
}

/// One byte past [`AUTO_FETCH_MAX_BYTES`], so the automatic drain declines it
/// and nothing else about the fixture is doing the work.
fn an_over_threshold_file() -> Vec<u8> {
    (0..=usize::try_from(AUTO_FETCH_MAX_BYTES).expect("the threshold fits a usize"))
        .map(|i| u8::try_from(i % 251).expect("a modulus below 256 fits a byte"))
        .collect()
}

/// Poll until `core` holds `id`'s *metadata* — which it reports by saying the
/// bytes are not here, rather than that the attachment is not.
async fn wait_attachment_known(core: &Core, id: EntityRef, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if matches!(
            core.attachment_bytes(id).await,
            Err(AttachError::BytesNotHere { .. })
        ) {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("the attachment's metadata never reached this device");
}

/// Poll until `core`'s cache state for `id` is `want`.
async fn wait_fetch_state(
    core: &Core,
    id: EntityRef,
    want: AttachmentFetchState,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = AttachmentFetchState::Idle;
    while tokio::time::Instant::now() < deadline {
        last = core
            .attachment_fetch_state(id)
            .expect("read the cache state");
        if last == want {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("the fetch state never reached {want:?}; it is {last:?}");
}
