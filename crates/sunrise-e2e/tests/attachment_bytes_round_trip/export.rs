//! A full export on a device that holds an attachment's metadata and not its
//! bytes (issue #1124, the remainder of #350): `fetch: false` lists it as
//! missing, and `fetch: true` brings the bytes over the relay and writes them.

use std::path::Path;
use std::sync::Arc;

use sunrise_client_core::export::{export_all, ExportOptions};
use sunrise_core::export::{AttachmentState, Manifest, MissingReason, MANIFEST_PATH};
use sunrise_core::{Clock, Core, SystemClock};
use sunrise_e2e::{
    open_paired_core, open_synced_core, spawn_relay, wait_live, wait_tasks_converge,
};

use super::on_demand::{an_over_threshold_file, wait_attachment_known};
use super::{a_task, ROOT, TIMEOUT};

/// The export-side half of §Lazy fetch, over a real relay.
///
/// The attachment is over the auto-fetch threshold, so B never fetches it on
/// its own and the first export sees exactly the state a user exporting from a
/// second device would: the row, and no bytes. The unit test in
/// `sunrise-client-core` reaches the missing half with no sync at all, so
/// `fetch: true` there can only fail; this is the only case in which the fetch
/// the export asks for has a relay to answer it.
#[tokio::test(flavor = "multi_thread")]
async fn an_export_fetches_an_attachment_that_is_only_on_the_relay() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("a vault dir");
    let dir_b = tempfile::tempdir().expect("a vault dir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, Arc::clone(&clock)).await;
    let b = open_paired_core(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let task = a_task(&a, "Send the assessor the site plan").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    let bytes = an_over_threshold_file();
    let att = a
        .attach_file(
            task,
            "site-plan.pdf".into(),
            "application/pdf".into(),
            &bytes,
        )
        .await
        .expect("attach the file");
    assert!(
        att.is_fetchable(),
        "A must have recorded the relay's name for the blob"
    );
    wait_attachment_known(&b, att.id, TIMEOUT).await;
    let id = att.id.to_str();

    // Not asked to fetch: listed as missing, and no file under `attachments/`.
    let first = tempfile::tempdir().expect("an export dir");
    let manifest = export(&b, first.path(), false).await;
    let [entry] = manifest.attachments.as_slice() else {
        panic!("one attachment: {:?}", manifest.attachments);
    };
    assert_eq!(entry.id, id);
    assert_eq!(entry.state, AttachmentState::Missing);
    assert_eq!(entry.missing_reason, Some(MissingReason::NotFetched));
    assert!(entry.path.is_none(), "a missing attachment has no path");
    assert!(
        !first.path().join("attachments").exists(),
        "nothing may be written for an attachment that is not here"
    );
    assert_eq!(
        read_manifest(first.path()).attachments,
        manifest.attachments
    );
    assert!(
        !b.attachment_is_local(&att).expect("locality"),
        "an export that was told not to fetch must not have fetched"
    );

    // Asked to fetch, into a fresh directory: the relay's copy is fetched and
    // written, byte for byte, and the manifest vouches for it.
    let second = tempfile::tempdir().expect("an export dir");
    let manifest = export(&b, second.path(), true).await;
    let [entry] = manifest.attachments.as_slice() else {
        panic!("one attachment: {:?}", manifest.attachments);
    };
    assert_eq!(entry.id, id);
    assert_eq!(entry.state, AttachmentState::Present);
    assert_eq!(entry.missing_reason, None);
    let path = format!("attachments/{id}/site-plan.pdf");
    assert_eq!(entry.path.as_deref(), Some(path.as_str()));
    let written = std::fs::read(
        second
            .path()
            .join("attachments")
            .join(&id)
            .join("site-plan.pdf"),
    )
    .expect("the fetched attachment was written");
    assert_eq!(
        written, bytes,
        "the export must hold exactly what A attached"
    );
    let listed = manifest
        .files
        .iter()
        .find(|f| f.path == path)
        .expect("the attachment is listed among the manifest's files");
    assert_eq!(listed.size, u64::try_from(bytes.len()).expect("fits a u64"));
    let on_disk = read_manifest(second.path());
    assert_eq!(on_disk.attachments, manifest.attachments);
    assert!(
        on_disk.files.contains(listed),
        "manifest.json must vouch for the attachment's file too"
    );

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}

/// `export_all` under a deadline: its fetch waits on the relay with none of
/// its own, and a fetch that never drains must fail the suite, not hang it.
async fn export(core: &Core, dir: &Path, fetch: bool) -> Manifest {
    tokio::time::timeout(TIMEOUT, export_all(core, dir, ExportOptions { fetch }))
        .await
        .expect("the export finished within the deadline")
        .expect("the export succeeded")
}

/// The `manifest.json` the export wrote, which is what a reader of the export
/// sees rather than the value the call returned. Its `files` are sorted on
/// the way out, so the tests compare entries rather than the whole value.
fn read_manifest(dir: &Path) -> Manifest {
    let bytes = std::fs::read(dir.join(MANIFEST_PATH)).expect("manifest.json was written");
    Manifest::from_bytes(&bytes).expect("manifest.json parses")
}
