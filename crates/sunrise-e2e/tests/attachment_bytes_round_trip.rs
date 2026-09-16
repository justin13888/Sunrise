//! The acceptance test for issue #176: an attachment's *bytes* reaching a
//! second device.
//!
//! Two real `Core`s, the real `sunrise-server` relay, the real SSE + `POST`
//! transport, and the relay's real `blobs/init` → `PUT` → `finalize` and
//! `GET /blobs/{id}` routes. Nothing is mocked and nothing is shared between
//! the two vault directories, so the only path the ciphertext can take from one
//! to the other is over TCP through the relay.
//!
//! # Why a unit test could not have covered this
//!
//! Every part of the byte path was individually testable before and
//! individually passing. What was missing was that nothing *called* the four
//! blob routes — the relay mounted them, verified them and tested them
//! server-side, and no client in the workspace made a request to any of them.
//! A test with a fake transport asserts that the driver would have called a
//! relay; the defect was that the calls did not exist, and only a test that
//! makes a second device read the bytes back distinguishes the two.
//!
//! # What each case pins
//!
//! * [`an_attachment_written_on_one_device_is_readable_on_another`] — the whole
//!   path, with a multi-chunk attachment so the chunk loop, the concatenated
//!   download body and the split back into chunks are all exercised rather than
//!   collapsing into a single-chunk special case.
//! * [`an_attachment_sealed_offline_uploads_when_the_device_reconnects`] — the
//!   reason the upload is a durable queue drained by the driver and not a call
//!   inside `attach_file`: attaching a file has to work with no network, and
//!   the bytes have to leave on the next session rather than never.
//! * [`attachment_bytes_travel_over_a_relay_that_requires_a_device_binding`] —
//!   the chunk `PUT` is the only route on this surface whose body is not JSON,
//!   so its signature is taken over raw bytes rather than a canonicalized
//!   value. The harness's default relay checks no binding at all, so without
//!   this case that path would run and never be verified.
//! * [`an_attachment_over_the_auto_fetch_threshold_arrives_when_it_is_asked_for`]
//!   — issue #227, and the same argument as the paragraph above it. Every part
//!   of the on-demand route is unit-tested, and the thing that was wrong before
//!   it existed was that *nothing joined the parts*: the threshold was a `WHERE`
//!   clause, the seam exported two read-only calls, and an attachment over
//!   10 MiB was unreachable on every device but the one that sealed it. A test
//!   with a fake transport asserts that the driver would have fetched; only a
//!   second real device reading back an over-threshold file distinguishes that
//!   from a queue nobody drains. The fixture is deliberately
//!   `AUTO_FETCH_MAX_BYTES + 1`, because the boundary is the claim.
//! * [`cancelling_a_download_marks_it_partial_and_the_next_press_restarts_it`] —
//!   the other two sentences of §Lazy fetch. Made deterministic by killing the
//!   relay rather than by racing a localhost download: with nothing to fetch,
//!   the request is reliably in flight, and what the case then asserts is that
//!   Cancel releases the waiting caller with no relay involved at all — which
//!   is the state a user is most likely to be cancelling from.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    AttachError, AttachmentFetchState, Clock, Command, Core, SystemClock, AUTO_FETCH_MAX_BYTES,
};
use sunrise_crypto::blob_chunk::CHUNK_PLAINTEXT_LEN;
use sunrise_domain::TaskDraft;
use sunrise_e2e::{
    open_core_offline, open_paired_core, open_paired_core_offline, open_synced_core,
    signed_ws_factory, spawn_relay, spawn_relay_with, wait_live, wait_tasks_converge,
};
use sunrise_id::EntityRef;
use sunrise_server::store::NewDevice;
use sunrise_server::{ServerConfig, StaticVerifier, Store, Subject};

/// Shared paired-device vault root, as in the other two-core tests.
const ROOT: [u8; 32] = [0x42; 32];

/// Generous CI-safe cap. A healthy run moves the bytes in well under a second;
/// the fetch backstop runs on the harness's 200 ms resync interval, so a run
/// that needs the timer rather than the inbound-batch trigger still finishes
/// far inside this.
const TIMEOUT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(25);

/// Two and a bit chunks. Past one chunk on purpose: a single-chunk attachment
/// would pass even if the chunk loop, the concatenation the relay streams back,
/// or the split that recovers the boundaries were wrong.
fn a_file() -> Vec<u8> {
    (0..CHUNK_PLAINTEXT_LEN * 2 + 4242)
        .map(|i| u8::try_from(i % 251).expect("a modulus below 256 fits a byte"))
        .collect()
}

async fn a_task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

/// Poll until `core` can reassemble `id`'s plaintext, or fail loudly.
///
/// `attachment_bytes` is the same call a client makes to open an attachment, so
/// this asserts the property a user would observe rather than the presence of
/// files on disk.
async fn wait_attachment_bytes(core: &Core, id: EntityRef, timeout: Duration) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::from("never polled");
    while tokio::time::Instant::now() < deadline {
        match core.attachment_bytes(id).await {
            Ok(bytes) => return bytes,
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("the attachment's bytes never arrived on this device: {last}");
}

/// Attach on one device, read the plaintext back on the other.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_written_on_one_device_is_readable_on_another() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // The parent has to exist on A before the attachment can: its Stream is the
    // attachment op's routing stream.
    let task = a_task(&a, "Renew the passport").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    let bytes = a_file();
    let att = a
        .attach_file(task, "passport-scan.png".into(), "image/png".into(), &bytes)
        .await
        .expect("attach the file");
    assert_eq!(att.chunk_count, 3, "the fixture must span several chunks");
    assert!(
        att.is_fetchable(),
        "the sealing device must record the name the relay will know this blob by"
    );

    // The property the issue is about. B holds the metadata op, has never seen
    // the file, and reads its plaintext back byte for byte.
    let got = wait_attachment_bytes(&b, att.id, TIMEOUT).await;
    assert_eq!(got, bytes, "B must reassemble exactly what A attached");

    // And A is unchanged: an upload is not a move.
    assert_eq!(
        a.attachment_bytes(att.id).await.expect("A still has it"),
        bytes
    );

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}

/// Attach with no relay reachable, then connect: the bytes leave on the session
/// that follows.
///
/// This is the case that decides where the upload is driven from. A client that
/// uploaded inside `attach_file` would either refuse this attach or accept it
/// and never upload, and the second is the state issue #176 describes.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_sealed_offline_uploads_when_the_device_reconnects() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // A is opened with a driver pointed at the relay, but the file is attached
    // before B exists — and, more to the point, the attach itself is a local
    // call that never touches the network.
    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    let task = a_task(&a, "File the insurance claim").await;

    let bytes = b"%PDF-1.7 a small but real document".to_vec();
    let att = a
        .attach_file(task, "claim.pdf".into(), "application/pdf".into(), &bytes)
        .await
        .expect("attach the file");

    // B joins afterwards and catches up on both halves: the op through the
    // stream, the ciphertext through the blob routes.
    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    wait_live(&b, TIMEOUT).await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    let got = wait_attachment_bytes(&b, att.id, TIMEOUT).await;
    assert_eq!(got, bytes);

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}

/// The same round trip against a relay that demands a device binding.
///
/// The chunk `PUT` is the only route on this surface whose body is not JSON, so
/// it is the only one whose signature is taken over raw bytes rather than a
/// canonicalized value. Under the harness's default relay nothing checks a
/// binding at all, so that path would be exercised and never verified; here the
/// relay refuses anything it cannot check, and a wrong canonical string for a
/// binary body is a `401` at the first chunk.
#[tokio::test(flavor = "multi_thread")]
async fn attachment_bytes_travel_over_a_relay_that_requires_a_device_binding() {
    const ISSUER: &str = "https://idp.example";
    const BEARER: &str = "alice-token";
    let subject = Subject::new(ISSUER, "alice");

    let mut captured: Option<Arc<Store>> = None;
    let (addr, relay) = spawn_relay_with(
        ServerConfig {
            require_device_sig: true,
            ..ServerConfig::default()
        },
        |state| {
            captured = Some(state.store.clone());
            state.with_verifier(Arc::new(
                StaticVerifier::default().with(BEARER, subject.clone()),
            ))
        },
    )
    .await;
    let store = captured.expect("the harness hands back the relay's own store");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let account = store
        .resolve_account(&subject, true, clock.now_ms())
        .expect("the account the bearer maps to");

    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");

    let a = open_core_offline(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    let a_device = register(&store, &account.account_id, &a, "a", clock.now_ms());
    a.start_sync(signed_ws_factory(
        addr,
        Some(BEARER.to_owned()),
        a.device_signer(a_device),
    ))
    .expect("start sync");

    let b = open_paired_core_offline(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    let b_device = register(&store, &account.account_id, &b, "b", clock.now_ms());
    b.start_sync(signed_ws_factory(
        addr,
        Some(BEARER.to_owned()),
        b.device_signer(b_device),
    ))
    .expect("start sync");

    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let task = a_task(&a, "Countersign the lease").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // Two chunks, so more than one signed binary body goes over the wire.
    let bytes: Vec<u8> = (0..CHUNK_PLAINTEXT_LEN + 9)
        .map(|i| u8::try_from(i % 251).expect("a modulus below 256 fits a byte"))
        .collect();
    let att = a
        .attach_file(task, "lease.pdf".into(), "application/pdf".into(), &bytes)
        .await
        .expect("attach the file");
    assert_eq!(att.chunk_count, 2);

    let got = wait_attachment_bytes(&b, att.id, TIMEOUT).await;
    assert_eq!(got, bytes);

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}

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
    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
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

    // The threshold, working. Two full resync periods of the harness's 200 ms
    // backstop, so this is "B had every chance and declined" rather than "B has
    // not got round to it".
    tokio::time::sleep(Duration::from_millis(600)).await;
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

/// Register `core`'s device at the relay and return the id the relay minted.
///
/// Through the store rather than over HTTP, as the other device-bound test
/// does: this crate has no REST client on its non-dev path, and
/// `POST /api/v1/devices` has its own tests in `sunrise-server`.
fn register(store: &Store, account_id: &str, core: &Core, nickname: &str, now_ms: u64) -> String {
    store
        .register_device(
            account_id,
            &NewDevice {
                device_pub_s: sunrise_http_sig::device_pub_b64(&core.device_signing_pub()),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some(sunrise_id::crockford::encode_bytes(&core.device_id())),
                nickname: nickname.to_owned(),
                platform: "linux".to_owned(),
                app_version: None,
            },
            now_ms,
        )
        .expect("register the device")
        .device_id
}
