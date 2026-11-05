//! `Core::close` promises that every later call answers `CoreError::Closed`.
//! Each public method that reaches storage is called here after
//! `Core::shutdown`, which is what `close` runs and the only way to keep a
//! handle to call through, and must refuse without touching the vault: in
//! particular, attaching a file must not write a chunk to the blob store
//! before it is refused.

use super::*;
use crate::{AttachError, AttachPreview, ThumbnailImage};
use sunrise_domain::Attachment;
use sunrise_id::EntityRef;

/// Every file under the blob store, chunks and directories alike.
fn blob_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join("blobs")];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

#[track_caller]
fn refused<T: std::fmt::Debug>(what: &str, r: Result<T, CoreError>) {
    match r {
        Err(CoreError::Closed) => {}
        other => panic!("{what}: {other:?}"),
    }
}

#[track_caller]
fn refused_attach<T: std::fmt::Debug>(what: &str, r: Result<T, AttachError>) {
    match r {
        Err(AttachError::Core(e)) if matches!(*e, CoreError::Closed) => {}
        other => panic!("{what}: {other:?}"),
    }
}

#[tokio::test]
async fn every_storage_method_refuses_after_close_and_no_chunk_is_written() {
    use sunrise_domain::TaskDraft;

    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Tax return".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .entity;
    // One attachment while open, so the reads below have a real row and real
    // chunks to refuse, and so the fixture is known to attach at all.
    let att = core
        .attach_file(task, "a.txt".into(), "text/plain".into(), b"hello")
        .await
        .unwrap();
    let joiner = sunrise_pairing::PairingJoiner::new(
        core.export_pairing_offer().unwrap(),
        "joiner".into(),
        "test".into(),
        [0x73; 32],
        [0x74; 32],
    );
    let before = blob_files(dir.path());
    assert!(!before.is_empty(), "the open attach wrote its chunk");

    core.shutdown().await;

    attachments_refuse(&core, task, &att).await;
    assert_eq!(
        blob_files(dir.path()),
        before,
        "no chunk was written and nothing was evicted after close"
    );
    keys_and_capture_refuse(&core, &joiner).await;
    the_old_checks_refuse(&core, task).await;
}

/// The attachment path, the cache and the fetch queue.
async fn attachments_refuse(core: &Core, task: EntityRef, att: &Attachment) {
    // Refused before a chunk is sealed or stored.
    refused_attach(
        "attach_file",
        core.attach_file(task, "b.txt".into(), "text/plain".into(), b"more")
            .await,
    );
    refused_attach(
        "attach_file_with",
        core.attach_file_with(
            task,
            "c.png".into(),
            "image/png".into(),
            b"pixels",
            AttachPreview {
                width: Some(1),
                height: Some(1),
                thumbnail: Some(ThumbnailImage {
                    mime_type: "image/jpeg".into(),
                    bytes: vec![0xff, 0xd8, 0xff, 0xd9],
                }),
            },
        )
        .await,
    );
    refused_attach("thumbnail_bytes", core.thumbnail_bytes(att.id).await);
    refused_attach("attachment_bytes", core.attachment_bytes(att.id).await);
    refused_attach("attachment_is_local", core.attachment_is_local(att));
    refused_attach("pin_attachment", core.pin_attachment(att.id).await);
    refused_attach("attachment_cache_usage", core.attachment_cache_usage());
    refused_attach("clear_attachment_cache", core.clear_attachment_cache());
    refused_attach("enforce_attachment_cache", core.enforce_attachment_cache());
    refused_attach("fetch_attachment", core.fetch_attachment(att.id).await);
    refused_attach(
        "cancel_attachment_fetch",
        core.cancel_attachment_fetch(att.id),
    );
    refused_attach(
        "attachment_fetch_state",
        core.attachment_fetch_state(att.id),
    );
}

/// Pairing, recovery, the relay intents, and capture, which reads the
/// vault's streams and contexts.
async fn keys_and_capture_refuse(core: &Core, joiner: &sunrise_pairing::PairingJoiner) {
    refused("export_pairing_offer", core.export_pairing_offer());
    refused(
        "issue_pairing_grant",
        core.issue_pairing_grant(joiner.request()),
    );
    refused(
        "pair_device_in_process",
        core.pair_device_in_process("c".into(), "test".into(), [1; 32], [2; 32]),
    );
    refused("seal_recovery_blob", core.seal_recovery_blob(&[7; 32]));
    refused(
        "relay_revocation_pending",
        core.relay_revocation_pending(&[9; 16]),
    );
    refused(
        "capture",
        core.capture("Call mum #home", &jiff::tz::TimeZone::UTC)
            .await,
    );
    refused(
        "capture_aside",
        core.capture_aside("Call mum", &jiff::tz::TimeZone::UTC)
            .await,
    );
}

/// The methods that already refused, now through the same guard.
async fn the_old_checks_refuse(core: &Core, task: EntityRef) {
    refused("submit", core.submit(Command::DeleteTask(task)).await);
    refused("query", core.query(Query::Inbox).await);
    refused(
        "apply_remote",
        core.apply_remote(b"not an envelope").await.map(|_| ()),
    );
    refused("missing_features", core.missing_features());
    refused("enable_field_ops", core.enable_field_ops(false));
    refused(
        "devices_lacking_feature",
        core.devices_lacking_feature("field_ops"),
    );
    refused("export_records", core.export_records().map(|_| ()));
    refused("chain_integrity", core.chain_integrity());
    refused("compact_op_log", core.compact_op_log());
    refused("rebuild_projection", core.rebuild_projection());
    refused("reindex_search", core.reindex_search().await);
    refused("stream_snapshot", core.stream_snapshot([3; 16]));
    refused("apply_stream_snapshot", core.apply_stream_snapshot(b"x"));
    refused(
        "reminder_plan",
        core.reminder_plan(0, 1, &sunrise_domain::ReminderSettings::default()),
    );
    refused(
        "on_time_zone_changed",
        core.on_time_zone_changed("Europe/Paris"),
    );
    refused("apply_key_bundle", core.apply_key_bundle(b"x"));
    refused("seed_identity_chain", core.seed_identity_chain(b"x"));
    refused("deadline_candidates", core.deadline_candidates());
    refused("security_events", core.security_events());
    refused("oldest_unacked_ms", core.oldest_unacked_ms());
}
