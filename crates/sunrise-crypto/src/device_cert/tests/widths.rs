//! Field-width guards.
//!
//! The CDDL at the top of the `device_cert` module fixes **five** fields at an
//! exact byte width, and every one is enforced by a match guard on `b.len()`:
//! four in the body — `device_id` and `identity_id` at 16, `D_S_pub` and
//! `D_D_pub` at 32 — in `body_from_cbor`, and the 64-byte `sig` of the outer
//! map in `DeviceCert::from_cbor`. All five are pinned below.
//!
//! A guard that stopped guarding would not fail any test that only ever feeds
//! it well-formed certs, so the tests below feed it the one thing that tells
//! the two apart: a certificate identical to a good one except that a single
//! field is the wrong width.

use super::*;

/// Pins the ids **`body_to_cbor` emits**, and only those: [`body_fields`]
/// is derived from the encoder, so this observes the encoding side alone.
/// The decoder's own numbering is already pinned by the eight tests that
/// decode a body and assert what came back.
///
/// Deriving [`body_fields`] means a renumbering moves the tests with it —
/// which is the point, but it also means no width test would notice one.
/// The key ids are a wire contract (the CDDL at the top of the module), so
/// pin them rather than leave the numbering unasserted in the crate.
#[test]
fn body_to_cbor_emits_exactly_the_eight_cddl_key_ids() {
    let ids: Vec<i64> = body_fields().into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        ids,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        "the device-cert body's key ids are a wire contract; renumbering one \
         breaks every peer that already speaks this format"
    );
}

/// Anchors every test below it: each wrong-width body differs from this one
/// in exactly one field, so a refusal there is attributable to the width
/// and to nothing else about how these bodies are built.
#[test]
fn the_unmodified_field_map_decodes_to_the_fixture() {
    let decoded = body_from_cbor(&encode_fields(body_fields())).expect("well-formed body");
    assert_eq!(decoded, fixture([1u8; 32]));
}

/// Wrong widths for a field the CDDL fixes at `width`: empty, one short,
/// one long. Each must be refused by the guard rather than truncated,
/// zero-extended, or (with the guard gone) panicked over inside
/// `copy_from_slice`.
fn assert_every_wrong_width_is_refused(id: i64, width: usize) {
    for n in [0, width - 1, width + 1] {
        let bytes = body_cbor_with(id, Value::Bytes(vec![0xab; n]));
        match body_from_cbor(&bytes) {
            Err(DeviceCertError::BadField("body field shape")) => {}
            other => panic!(
                "body field {id} at {n} bytes (the CDDL fixes it at {width}): expected \
                 Err(BadField(\"body field shape\")), got {other:?}"
            ),
        }
    }
}

#[test]
fn device_id_at_any_width_but_16_is_refused() {
    assert_every_wrong_width_is_refused(2, 16);
}

#[test]
fn d_s_pub_at_any_width_but_32_is_refused() {
    assert_every_wrong_width_is_refused(3, 32);
}

#[test]
fn d_d_pub_at_any_width_but_32_is_refused() {
    assert_every_wrong_width_is_refused(4, 32);
}

#[test]
fn identity_id_at_any_width_but_16_is_refused() {
    assert_every_wrong_width_is_refused(5, 16);
}

/// A 32-byte value in a 16-byte field is the confusion a guard exists to
/// stop: without it the decode would take the first 16 bytes of a public
/// key and call them a device id.
#[test]
fn a_public_key_sized_value_is_not_a_device_id() {
    let bytes = body_cbor_with(2, Value::Bytes(vec![0xcd; 32]));
    assert!(matches!(
        body_from_cbor(&bytes),
        Err(DeviceCertError::BadField("body field shape"))
    ));
}

fn outer_cbor_with_sig_of(width: usize) -> Vec<u8> {
    outer_cbor(
        body_to_cbor(&fixture([1u8; 32])).expect("the fixture encodes"),
        width,
    )
}

/// The fifth length guard, and the only one on a **public** entry point.
///
/// `sunrise-core` feeds `DeviceCert::from_cbor` certificates published by
/// *other* devices on the `DeviceCertPublish` path, and that caller handles
/// a refusal by warning and returning — so a guard that stopped guarding
/// would not merely mis-parse, it would turn an expected `Err` into a
/// `copy_from_slice` panic on the op-apply path.
#[test]
fn a_signature_at_any_width_but_64_is_refused() {
    // The control: at the right width this map shape decodes, so a refusal
    // below is the signature width and not the way the map is built.
    DeviceCert::from_cbor(&outer_cbor_with_sig_of(64)).expect("a 64-byte signature decodes");

    for n in [0, 63, 65] {
        match DeviceCert::from_cbor(&outer_cbor_with_sig_of(n)) {
            Err(DeviceCertError::BadField("device cert outer")) => {}
            other => panic!(
                "a {n}-byte signature (the CDDL fixes it at 64): expected \
                 Err(BadField(\"device cert outer\")), got {other:?}"
            ),
        }
    }
}
