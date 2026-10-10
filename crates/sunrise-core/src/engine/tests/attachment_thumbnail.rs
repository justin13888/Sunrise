//! An attachment's thumbnail and dimensions, the `attachment.thumbnail`
//! feature (ADR-0053 §2, issue #346).

use super::*;
use crate::control_op::DeviceFeaturesPayload;
use std::collections::BTreeMap;
use sunrise_domain::Thumbnail;

fn thumb() -> Thumbnail {
    Thumbnail {
        blob_id: [21u8; 16],
        blob_key: [23u8; 32],
        mime_type: "image/png".into(),
        size_bytes: 4096,
        content_hash: [25u8; 32],
        ciphertext_hash: [27u8; 32],
    }
}

fn with_thumbnail(task: EntityRef) -> AttachmentDraft {
    AttachmentDraft {
        width: Some(1920),
        height: Some(1080),
        thumbnail: Some(thumb()),
        ..attachment_draft(task)
    }
}

fn required(db: &Db) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT feature FROM vault_required_features ORDER BY feature")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The thumbnail is written with the attachment, the vault requires the
/// feature first, and a second device that applies the op reads the same
/// fields back.
#[test]
fn a_thumbnail_is_written_with_its_attachment_and_replicates() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], ca);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "Scan the receipts");
    let att = ea
        .apply(&mut dba, Command::AttachFile(with_thumbnail(task)))
        .unwrap()
        .entity;
    assert_eq!(required(&dba), vec!["attachment.thumbnail".to_owned()]);

    let a = read_attachment(dba.conn(), att.bytes()).unwrap().unwrap();
    assert_eq!((a.width, a.height), (Some(1920), Some(1080)));
    assert_eq!(a.thumbnail(), Some(thumb()));
    assert!(a.unknown.is_empty());

    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, att.bytes(), "attachment.create"),
    )
    .unwrap();
    let b = read_attachment(dbb.conn(), att.bytes()).unwrap().unwrap();
    assert_eq!(b.thumbnail(), Some(thumb()));
    assert_eq!((b.width, b.height), (Some(1920), Some(1080)));
}

/// While a paired device has not said it supports the feature, the
/// attachment is written without the thumbnail rather than refused, and the
/// vault does not start requiring the feature.
#[test]
fn a_thumbnail_is_dropped_while_a_paired_device_lacks_the_feature() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], ca);
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let task = new_task(&ea, &mut dba, "Scan the receipts");
    let att = ea
        .apply(&mut dba, Command::AttachFile(with_thumbnail(task)))
        .unwrap()
        .entity;
    let a = read_attachment(dba.conn(), att.bytes()).unwrap().unwrap();
    assert!(!a.uses_thumbnail_feature(), "got {a:?}");
    assert!(required(&dba).is_empty());

    // Once the other device advertises the feature, the next one keeps it.
    let inner = InnerOp::DeviceFeatures(DeviceFeaturesPayload {
        features: vec!["attachment.thumbnail".to_owned()],
        unknown: Unknowns::new(),
    });
    let b_id = eb.keychain.device_id();
    dba.with_tx(|tx| {
        ea.apply_control_op(tx, &inner, &b_id, Hlc::at(10), T0, 1)
            .map(|_| ())
    })
    .unwrap();
    let att2 = ea
        .apply(&mut dba, Command::AttachFile(with_thumbnail(task)))
        .unwrap()
        .entity;
    let a2 = read_attachment(dba.conn(), att2.bytes()).unwrap().unwrap();
    assert_eq!(a2.thumbnail(), Some(thumb()));
}

/// A row a build without the columns projected kept the fields in `extra`.
/// After the upgrade they read back as fields, and leave the unknowns, so the
/// row is not written with each key twice.
#[test]
fn fields_an_older_build_kept_in_extra_are_read_as_fields() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Scan the receipts");
    let att = e
        .apply(&mut db, Command::AttachFile(attachment_draft(task)))
        .unwrap()
        .entity;

    let mut extra: BTreeMap<String, ciborium::Value> = BTreeMap::new();
    extra.insert("width".into(), ciborium::Value::Integer(640.into()));
    extra.insert(
        "thumbnail_blob_id".into(),
        ciborium::Value::Bytes(vec![21u8; 16]),
    );
    extra.insert("future_field".into(), ciborium::Value::Bool(true));
    let mut blob = Vec::new();
    ciborium::ser::into_writer(&extra, &mut blob).unwrap();
    db.conn()
        .execute(
            "UPDATE attachments SET extra = ? WHERE id = ?",
            params![blob, &att.bytes()[..]],
        )
        .unwrap();

    let a = read_attachment(db.conn(), att.bytes()).unwrap().unwrap();
    assert_eq!(a.width, Some(640));
    assert_eq!(a.thumbnail_blob_id, Some([21u8; 16]));
    assert_eq!(
        a.unknown.keys().cloned().collect::<Vec<_>>(),
        vec!["future_field".to_owned()]
    );
}
