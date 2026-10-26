//! A fixed-width field the client hands back as hex, across the seam. `UniFFI`
//! has no fixed-size array, so a digest crosses as a string, and whatever the
//! string holds is answered with `BindingError::BadFixedBytes`, never a panic
//! inside Rust. `flavor = "multi_thread"` for the reason `seam.rs` gives.

use sunrise_core_bindings::dto::AttachPreviewIn;
use sunrise_core_bindings::{BindingError, CoreCommand};

mod support;
use support::{draft, open_core};

/// A hex field the right number of bytes long but holding a multi-byte
/// character is refused as the field's width error. It used to panic inside
/// the seam: the field was sliced two bytes at a time, and the second slice
/// started inside the character.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_ascii_hex_field_is_a_typed_error_not_a_panic() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Odd digest"),
        })
        .await
        .expect("create")
        .entity;
    let mut att = core
        .attach_file(
            task,
            "n.txt".into(),
            "text/plain".into(),
            b"hello".to_vec(),
            AttachPreviewIn::default(),
        )
        .await
        .expect("attach");

    // 32 bytes, as a 16-byte id's hex is, but `é` straddles every pair.
    att.blob_id = format!("0{}0", "\u{e9}".repeat(15));
    assert_eq!(att.blob_id.len(), 32);
    assert!(matches!(
        core.attachment_is_local(att),
        Err(BindingError::BadFixedBytes { field, expected: 16 }) if field == "blob_id"
    ));
}
