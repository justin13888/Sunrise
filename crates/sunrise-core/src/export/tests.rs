//! The export format's pure half: the kinds it covers, file naming, and the
//! manifest's encoding. The end-to-end export of a real vault is tested in
//! `crates/sunrise-client-core/tests/export.rs`.

use super::*;

#[test]
fn every_stored_kind_is_exported_and_nothing_else() {
    let stored: Vec<&str> = sunrise_id::registry::ENTITIES
        .iter()
        .filter(|e| e.records.first().is_some_and(|r| r.storage.is_some()))
        .map(|e| e.tag)
        .collect();
    let mut exported = EXPORTED_KINDS.to_vec();
    let mut want = stored.clone();
    exported.sort_unstable();
    want.sort_unstable();
    assert_eq!(
        exported, want,
        "a kind with its own table must have an entities/<kind>.jsonl"
    );
}

#[test]
fn an_empty_vault_still_writes_one_file_per_kind() {
    let files = entity_files(&VaultRecords::default()).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let mut want: Vec<String> = EXPORTED_KINDS
        .iter()
        .map(|k| format!("entities/{k}.jsonl"))
        .collect();
    want.sort();
    assert_eq!(paths, want);
    assert!(files.iter().all(|f| f.bytes.is_empty()));
}

#[test]
fn a_filename_cannot_leave_its_directory() {
    assert_eq!(safe_filename("scan.pdf"), "scan.pdf");
    assert_eq!(safe_filename("../../etc/passwd"), "_.._etc_passwd");
    assert_eq!(safe_filename(".."), "file");
    assert_eq!(safe_filename(""), "file");
    assert_eq!(safe_filename("a\\b:c\u{0}d"), "a_b_c_d");
    assert_eq!(safe_filename(".hidden"), "hidden");
    assert_eq!(safe_filename("trailing. "), "trailing");
    assert_eq!(safe_filename("CON?.txt"), "CON_.txt");
    let long = "é".repeat(300);
    let cut = safe_filename(&long);
    assert!(cut.len() <= 200 && cut.chars().all(|c| c == 'é'));
}

#[test]
fn the_manifest_reads_back_and_its_bytes_do_not_depend_on_insertion_order() {
    let mut a = Manifest::new(1_767_225_600_000, "America/New_York");
    a.files
        .push(ManifestFile::of("entities/task.jsonl", b"{}\n"));
    a.files
        .push(ManifestFile::of("calendar.ics", b"BEGIN:VCALENDAR"));
    let mut b = a.clone();
    b.files.reverse();
    assert_eq!(a.to_bytes(), b.to_bytes());

    let back = Manifest::from_bytes(&a.to_bytes()).unwrap();
    assert_eq!(back.format, EXPORT_FORMAT);
    assert_eq!(back.format_v, EXPORT_FORMAT_V);
    assert_eq!(back.exported_at, "2026-01-01T00:00:00Z");
    assert_eq!(back.zone, "America/New_York");
    assert_eq!(
        back.doc_schema_v,
        sunrise_cbor::version::DOC_SCHEMA_V,
        "the records' schema is named"
    );
    assert!(back.doc_schema_fingerprint.is_some());
    assert_eq!(back.files[0].path, "calendar.ics", "files are sorted");
    assert!(back.file("entities/task.jsonl").unwrap().matches(b"{}\n"));
    assert!(!back.file("entities/task.jsonl").unwrap().matches(b"{}"));
}

#[test]
fn a_manifest_of_another_format_is_refused() {
    // A manifest complete in every other field, so it is the format check
    // and not the shape that refuses it.
    let mut other = Manifest::new(1_767_225_600_000, "America/New_York");
    other.format = "something-else".into();
    let err = Manifest::from_bytes(&other.to_bytes()).unwrap_err();
    assert!(
        matches!(&err, ExportError::Manifest(why) if why == r#"format is "something-else""#),
        "{err:?}"
    );
}
