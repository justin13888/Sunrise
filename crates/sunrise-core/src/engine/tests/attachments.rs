//! Attachments: their metadata, routing and tombstones.

use super::*;

// ---- attachments (docs/02-domain/attachments.md) ----

#[test]
fn an_attached_file_round_trips_with_its_key() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let task = new_task(&e, &mut db, "File the expenses");
    let res = e
        .apply(&mut db, Command::AttachFile(attachment_draft(task)))
        .unwrap();
    assert_eq!(res.entity.kind(), EntityKind::Attachment);
    assert_eq!(inner_op_variant(&e, &db, &res.op_id), "AttachmentCreate");

    let rows = attachments_of(&e, &db, task);
    assert_eq!(rows.len(), 1);
    let a = &rows[0];
    assert_eq!(a.id, res.entity);
    assert_eq!(a.parent, task);
    assert_eq!(a.filename, "receipt.pdf");
    assert_eq!(a.mime_type, "application/pdf");
    assert_eq!(a.size_bytes, 4096);
    assert_eq!(a.chunk_count, 2);
    // The per-blob key is what makes the ciphertext readable anywhere else;
    // losing it on the way through storage would orphan the blob.
    assert_eq!(a.blob_key, [7u8; 32]);
    assert_eq!(a.blob_id, [9u8; 16]);
    assert_eq!(a.content_hash, [11u8; 32]);
    assert!(!a.deleted);
}

/// The op routes under the parent task's Stream, not the meta stream, so
/// an attachment travels with the work it belongs to.
#[test]
fn an_attachment_routes_under_its_tasks_stream() {
    let mut db = db();
    let e = engine();
    let stream = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "File the expenses".into(),
                stream_id: Some(stream),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let res = e
        .apply(&mut db, Command::AttachFile(attachment_draft(task)))
        .unwrap();
    let env = sunrise_crypto::decode_envelope(&env_bytes(&db, &res.op_id)).unwrap();
    assert_eq!(&env.stream_id, stream.bytes());
}

#[test]
fn attaching_to_an_unknown_task_is_rejected() {
    let mut db = db();
    let e = engine();
    let ghost = EntityRef::new(EntityKind::Task, [0xee; 16]);
    assert!(matches!(
        e.apply(&mut db, Command::AttachFile(attachment_draft(ghost))),
        Err(EngineError::NotFound(_))
    ));
}

#[test]
fn an_invalid_draft_is_rejected_before_any_op_is_written() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "File the expenses");
    let before = op_count(&db);
    let bad = AttachmentDraft {
        size_bytes: 0,
        ..attachment_draft(task)
    };
    assert!(e.apply(&mut db, Command::AttachFile(bad)).is_err());
    assert_eq!(op_count(&db), before, "a rejected draft writes no op");
}

#[test]
fn detaching_tombstones_the_metadata_and_hides_it_from_the_task() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let task = new_task(&e, &mut db, "File the expenses");
    let att = e
        .apply(&mut db, Command::AttachFile(attachment_draft(task)))
        .unwrap()
        .entity;
    assert_eq!(attachments_of(&e, &db, task).len(), 1);

    let res = e.apply(&mut db, Command::DetachFile(att)).unwrap();
    assert_eq!(inner_op_variant(&e, &db, &res.op_id), "AttachmentDelete");
    assert!(attachments_of(&e, &db, task).is_empty());
}

/// Attachment metadata is write-once, so there is no `AttachmentUpdate` to
/// race a delete against — but the *ordering* half of the same defect is
/// live. A delete that overtakes its create had no row to tombstone and was
/// dropped; the create then landed live and the replica stayed permanently
/// out of step with the one that had deleted.
///
/// Carrying the attachment's full state makes the delete self-sufficient.
#[test]
fn an_attachment_delete_that_overtakes_its_create_still_lands_as_a_tombstone() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "File the expenses");
    let att = ea
        .apply(&mut dba, Command::AttachFile(attachment_draft(task)))
        .unwrap()
        .entity;
    set_clock(&ca, T0 + 1_000);
    ea.apply(&mut dba, Command::DetachFile(att)).unwrap();

    // Delete first, create second.
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, att.bytes(), "attachment.delete"),
    )
    .unwrap();
    let rb = read_attachment(dbb.conn(), att.bytes())
        .unwrap()
        .expect("the delete materialized a row of its own");
    assert!(rb.deleted);
    // The delete carries the metadata, so the tombstone is a real record
    // rather than a bare id: the blob GC can still identify what it names.
    assert_eq!(rb.filename, "receipt.pdf");
    assert_eq!(rb.content_hash, [11u8; 32]);
    assert_eq!(rb.parent, task);

    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, att.bytes(), "attachment.create"),
    )
    .unwrap();
    assert!(
        read_attachment(dbb.conn(), att.bytes())
            .unwrap()
            .unwrap()
            .deleted,
        "the older create loses LWW and does not resurrect it"
    );
    assert_eq!(
        row_stamp(&dba, "attachments", "id", &att),
        row_stamp(&dbb, "attachments", "id", &att),
    );
}

#[test]
fn attachments_list_oldest_first_and_only_for_their_own_task() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], Arc::clone(&clock));
    let mut db = db_root(ROOT);
    let a_task = new_task(&e, &mut db, "One");
    let b_task = new_task(&e, &mut db, "Two");
    for (i, (task, name)) in [
        (a_task, "first.pdf"),
        (a_task, "second.pdf"),
        (b_task, "other.pdf"),
    ]
    .into_iter()
    .enumerate()
    {
        // Distinct instants: "oldest first" is an ordering on
        // `created_at_ms`, and three attachments in one millisecond have
        // no oldest.
        set_clock(&clock, T0 + i as u64 * 1_000);
        e.apply(
            &mut db,
            Command::AttachFile(AttachmentDraft {
                filename: name.into(),
                ..attachment_draft(task)
            }),
        )
        .unwrap();
    }
    let names: Vec<_> = attachments_of(&e, &db, a_task)
        .into_iter()
        .map(|a| a.filename)
        .collect();
    assert_eq!(
        names,
        vec!["first.pdf".to_string(), "second.pdf".to_string()]
    );
    assert_eq!(attachments_of(&e, &db, b_task).len(), 1);
}

#[test]
fn task_attachments_rejects_a_non_task_id() {
    let db = db();
    let e = engine();
    assert!(e
        .query(
            &db,
            Query::TaskAttachments(EntityRef::new(EntityKind::Stream, [1u8; 16]))
        )
        .is_err());
}

/// Attachment metadata that arrives before its parent task still lands:
/// nothing about the row depends on the parent existing, and the two join
/// up when the task's own op turns up.
#[test]
fn an_attachment_op_that_overtakes_its_task_still_materializes() {
    let clock_a = Arc::new(FakeClock(PLMutex::new(T0)));
    let clock_b = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], clock_a);
    let eb = engine_seeded(ROOT, [2u8; 32], clock_b);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "File the expenses");
    let att = ea
        .apply(&mut dba, Command::AttachFile(attachment_draft(task)))
        .unwrap();

    // Only the attachment op crosses; the task's create stays behind.
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &att.op_id))
        .unwrap();
    assert!(
        attachments_of(&eb, &dbb, task).len() == 1,
        "the attachment materializes without its parent"
    );

    // The task arrives afterwards and the two agree.
    eb.apply_remote(&mut dbb, &create_env_for(&dba, task.bytes()))
        .unwrap();
    let rows = attachments_of(&eb, &dbb, task);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].blob_key, [7u8; 32]);
    assert_eq!(rows[0].content_hash, [11u8; 32]);
}
