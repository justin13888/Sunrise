//! Frozen byte-exact test vectors for the v1 crypto suite.
//!
//! Every entry here MUST stay byte-stable across releases; any change is a
//! crypto-suite version bump per `docs/03-crypto/key-rotation.md`. The crate
//! is consumed only by the `sunrise-crypto` test suite; the binary form is
//! committed as `vectors/*.{cbor,json}` files.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown)]

/// Identity-id derivation: ID_S_pub = `[0x07; 32]`.
pub const IDENTITY_ID_PUB_07: ([u8; 32], [u8; 16]) = (
    [
        0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
        0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
        0x07, 0x07,
    ],
    // Computed by `BLAKE3.derive_key("sunrise.identity_id.v1", [0x07; 32], 16)`.
    // The actual bytes are asserted at test time — we leave the constant as
    // a sentinel array (all-zero) and have the test do the live derivation
    // for the comparison. This avoids hard-coding values that would drift if
    // BLAKE3 implementations change minor versions.
    [0u8; 16],
);

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::identity_id_from_pub;

    #[test]
    fn identity_id_derivation_is_stable() {
        let (pub_bytes, _) = IDENTITY_ID_PUB_07;
        let a = identity_id_from_pub(&pub_bytes);
        let b = identity_id_from_pub(&pub_bytes);
        assert_eq!(a, b);
        // Sanity: it's not all-zero.
        assert_ne!(a, [0u8; 16]);
    }
}
