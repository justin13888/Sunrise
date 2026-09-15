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

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, SystemClock};
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
