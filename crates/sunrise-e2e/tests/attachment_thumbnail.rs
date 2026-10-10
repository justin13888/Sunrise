//! The acceptance test for issue #346's thumbnails: device A attaches a file
//! with a thumbnail it rendered, and device B shows the thumbnail without
//! fetching the original.
//!
//! Two real `Core`s and the real relay, as in `attachment_bytes_round_trip`.
//! The original is one byte over `AUTO_FETCH_MAX_BYTES`, so B never fetches
//! it unasked; the thumbnail is fetched anyway, because ADR-0053 §3 says a
//! thumbnail always auto-fetches, whatever its attachment's size. A second
//! case puts B on cellular, where an original under the threshold waits and
//! the thumbnail still comes.
//!
//! A attaches before B pairs. A vault requires `attachment.thumbnail` only
//! once every paired device has said it supports it, and a device that has
//! just paired may not have said so yet; attaching first leaves A the only
//! device, so the thumbnail is kept rather than dropped.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    AttachError, AttachPreview, Clock, Command, Core, NetworkClass, SystemClock, ThumbnailImage,
    AUTO_FETCH_MAX_BYTES,
};
use sunrise_domain::TaskDraft;
use sunrise_e2e::{
    open_paired_core, open_synced_core, spawn_relay, wait_live, wait_tasks_converge,
};
use sunrise_id::EntityRef;

const ROOT: [u8; 32] = [0x43; 32];
const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(25);

fn a_jpeg() -> ThumbnailImage {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend((0..4000u32).map(|i| u8::try_from(i % 241).unwrap_or(0)));
    ThumbnailImage {
        mime_type: "image/jpeg".into(),
        bytes,
    }
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

/// Poll until `core` can read `id`'s thumbnail.
async fn wait_thumbnail(core: &Core, id: EntityRef) -> (String, Vec<u8>) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut last = String::from("never polled");
    while tokio::time::Instant::now() < deadline {
        match core.thumbnail_bytes(id).await {
            Ok(Some(t)) => return t,
            Ok(None) => last = "the attachment has no thumbnail here".into(),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("the thumbnail never arrived on this device: {last}");
}

async fn attach_pictured(a: &Core, task: EntityRef, size: usize) -> sunrise_domain::Attachment {
    let bytes: Vec<u8> = (0..size)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    a.attach_file_with(
        task,
        "site-photo.jpg".into(),
        "image/jpeg".into(),
        &bytes,
        AttachPreview {
            width: Some(4032),
            height: Some(3024),
            thumbnail: Some(a_jpeg()),
        },
    )
    .await
    .expect("attach")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_device_shows_the_thumbnail_without_fetching_the_original() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    let task = a_task(&a, "Survey the site").await;
    let size = usize::try_from(AUTO_FETCH_MAX_BYTES).expect("fits") + 1;
    let att = attach_pictured(&a, task, size).await;
    assert!(
        att.thumbnail().is_some(),
        "A is the only device; it keeps it"
    );

    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    wait_live(&b, TIMEOUT).await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    let (mime, bytes) = wait_thumbnail(&b, att.id).await;
    assert_eq!(mime, "image/jpeg");
    assert_eq!(bytes, a_jpeg().bytes);

    // Two resync periods of the harness's backstop: B had every chance to
    // fetch the original and did not.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let on_b = b
        .query(sunrise_core::Query::EntityById(att.id))
        .await
        .expect("query");
    let sunrise_core::QueryResult::Attachments(rows) = on_b else {
        panic!("an attachment query answers with attachments");
    };
    assert_eq!(rows[0].width, Some(4032));
    assert!(!b.attachment_is_local(&rows[0]).expect("locality"));
    assert!(matches!(
        b.attachment_bytes(att.id).await,
        Err(AttachError::BytesNotHere { .. })
    ));

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn on_cellular_the_thumbnail_comes_and_a_small_original_waits() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    let task = a_task(&a, "Survey the site").await;

    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    b.set_network_class(NetworkClass::Cellular);
    wait_live(&b, TIMEOUT).await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // A now has a paired device; wait until it knows B supports the feature,
    // or the thumbnail is dropped by design.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let att = loop {
        let att = attach_pictured(&a, task, 2048).await;
        if att.thumbnail().is_some() {
            break att;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "A never learned that B supports attachment.thumbnail"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let (_, bytes) = wait_thumbnail(&b, att.id).await;
    assert_eq!(bytes, a_jpeg().bytes);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        matches!(
            b.attachment_bytes(att.id).await,
            Err(AttachError::BytesNotHere { .. })
        ),
        "an original under the threshold waits on cellular while the preference is off"
    );

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}
