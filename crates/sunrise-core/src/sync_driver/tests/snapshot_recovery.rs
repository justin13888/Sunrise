//! A snapshot record is the blob its pin names (issue #604).

use super::*;

/// This vault's own inbox snapshot record, written by compacting
/// everything it holds.
async fn own_inbox_record(core: &Core) -> Vec<u8> {
    core.submit(Command::CreateTask(TaskDraft {
        title: "In the record".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    core.set_compaction_policy(crate::engine::CompactionPolicy {
        retention_ms: 0,
        known_device_window_ms: 1,
        snapshot_every_ms: 0,
    });
    core.compact_op_log().unwrap();
    core.stream_snapshot(sunrise_domain::INBOX_STREAM_BYTES)
        .unwrap()
        .expect("compaction wrote an inbox record")
}

fn serving(record: Vec<u8>, pinned_blob_id: Option<[u8; 16]>) -> ChannelTransport {
    let (mut t, _) = duplex();
    t.serves_snapshots = true;
    t.pinned = Some(record);
    t.pinned_blob_id = pinned_blob_id;
    t
}

/// A record whose bytes are not the blob the pin names is refused before
/// the engine sees it. The control serves the same record under its own
/// address, which reaches the engine and comes back `covered`.
#[tokio::test]
async fn a_snapshot_whose_bytes_are_not_the_pinned_blob_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let core = open_arc(dir.path()).await;
    let record = own_inbox_record(&core).await;
    let stream = sunrise_domain::INBOX_STREAM_BYTES;

    let mut honest = serving(record.clone(), None);
    assert_eq!(
        crate::snapshot_sync::recover(&core, &mut honest, stream).await,
        crate::snapshot_sync::Recovery::Failed("covered")
    );

    let mut wrong = crate::snapshot_sync::blob_id_of(&record);
    wrong[0] ^= 0x01;
    let mut mismatched = serving(record, Some(wrong));
    assert_eq!(
        crate::snapshot_sync::recover(&core, &mut mismatched, stream).await,
        crate::snapshot_sync::Recovery::Failed("rejected")
    );
}
