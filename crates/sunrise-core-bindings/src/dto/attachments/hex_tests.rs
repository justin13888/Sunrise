//! `from_hex` is the seam's reader of the digests Swift hands back as hex. A
//! field arrives from outside Rust, so whatever it holds is answered with
//! [`crate::BindingError::BadFixedBytes`] or bytes, never a panic.

use super::from_hex;
use crate::BindingError;

#[test]
fn lowercase_and_uppercase_hex_read_back() {
    assert_eq!(from_hex::<2>("0aFf", "f").unwrap(), [0x0a, 0xff]);
}

/// Four bytes long, but `é` straddles the boundary between the two pairs.
#[test]
fn a_multi_byte_character_is_the_width_error() {
    let s = "0\u{e9}0";
    assert_eq!(s.len(), 4);
    assert!(matches!(
        from_hex::<2>(s, "blob_id"),
        Err(BindingError::BadFixedBytes { field, expected: 2 }) if field == "blob_id"
    ));
}

/// A character that fills a pair exactly is not hex either.
#[test]
fn a_two_byte_character_on_a_pair_is_the_width_error() {
    assert!(matches!(
        from_hex::<2>("\u{e9}00", "blob_id"),
        Err(BindingError::BadFixedBytes { .. })
    ));
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// Any string reads as bytes or as the width error, never a panic.
    #[test]
    fn any_string_reads_without_panicking(s in proptest::prelude::any::<String>()) {
        let _ = from_hex::<16>(&s, "f");
    }

    /// Strings of exactly the expected byte length, which is the shape that
    /// reaches the per-pair slice, with multi-byte characters mixed in.
    #[test]
    fn any_string_of_the_right_width_reads_without_panicking(
        s in "[0-9a-f\u{e9}\u{65e5}\u{1f600}]{0,32}",
    ) {
        let mut s = s;
        while s.len() < 32 {
            s.push('0');
        }
        if s.len() == 32 {
            let _ = from_hex::<16>(&s, "f");
        }
    }
}
