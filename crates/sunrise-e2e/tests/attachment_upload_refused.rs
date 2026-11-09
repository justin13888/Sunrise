//! A relay that refuses an attachment's upload id, and the client that
//! recovers from it (issue #756).
//!
//! `sunrise_core::blob_sync` keeps a reserved upload id across every failure
//! but one: a `SYNC_OP_INVALID` refusal, which says the id itself is no good.
//! Then it drops the id and reserves a fresh one on its next attempt. The
//! core's unit tests cover that arm against a fake transport; this covers it
//! through the real relay and the real SSE + `POST` transport, ending where a
//! user would look: the bytes readable on a second device.
//!
//! The seam is [`refusing_upload_ws_factory`]: the first upload id the relay
//! reserves reaches A one character too long, so the relay's own route
//! refuses its chunk, and the client reads that refusal as `SYNC_OP_INVALID`.

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, SystemClock};
use sunrise_crypto::blob_chunk::CHUNK_PLAINTEXT_LEN;
use sunrise_domain::TaskDraft;
use sunrise_e2e::{
    open_core_with_factory, open_paired_core, refusing_upload_ws_factory, spawn_relay, wait_live,
    wait_tasks_converge,
};
use sunrise_error::ErrorCode;

/// Shared paired-device vault root, as in the other two-core tests.
const ROOT: [u8; 32] = [0x42; 32];

/// Generous CI-safe cap, as in `attachment_bytes_round_trip.rs`: the retry
/// rides the harness's 200 ms resync interval.
const TIMEOUT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(25);

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_whose_id_the_relay_refuses_is_reserved_again_and_arrives() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Two chunks, so the upload that follows the refusal re-sends more than
    // the one chunk the refused attempt got as far as, and so the seam picks
    // this upload out from the core's one-chunk snapshot records.
    let bytes: Vec<u8> = (0..CHUNK_PLAINTEXT_LEN + 77)
        .map(|i| u8::try_from(i % 251).expect("a modulus below 256 fits a byte"))
        .collect();
    let (factory, refusal) = refusing_upload_ws_factory(addr, 2);
    let a = open_core_with_factory(dir_a.path(), ROOT, addr, Arc::clone(&clock), factory).await;
    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let task = a
        .submit(Command::CreateTask(TaskDraft {
            title: "Send the signed lease".into(),
            ..Default::default()
        }))
        .await
        .expect("create task")
        .entity;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    let att = a
        .attach_file(task, "lease.pdf".into(), "application/pdf".into(), &bytes)
        .await
        .expect("attach the file");
    assert_eq!(att.chunk_count, 2, "the chunk count the seam was built for");

    // What a user observes: B opens the attachment.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let got = loop {
        match b.attachment_bytes(att.id).await {
            Ok(got) => break got,
            Err(e) => assert!(
                tokio::time::Instant::now() < deadline,
                "the bytes never reached B ({e}); refusals {:?}, inits {}",
                refusal.refusals(),
                refusal.inits()
            ),
        }
        tokio::time::sleep(POLL).await;
    };
    assert_eq!(got, bytes, "B reassembles exactly what A attached");

    // And they arrived the way this test is about: the relay refused the
    // first id this attachment was reserved, the client read that as the
    // id's own refusal, and the core reserved a second one rather than
    // retrying the first.
    assert_eq!(
        refusal.refusals(),
        vec![ErrorCode::SyncOpInvalid.as_str().to_owned()],
        "one refusal, read as the upload id's"
    );
    assert_eq!(refusal.inits(), 2, "a fresh id after the refused one");

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}
