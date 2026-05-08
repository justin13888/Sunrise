//! BLAKE3 KDF wrapper.
//!
//! Per `spec/03-crypto/primitives.md`, every `derive_key` call MUST pass a
//! unique, descriptive context string. The thin wrapper here forces every
//! caller to supply a non-empty static `&str` context.

/// Length of a BLAKE3 hash (also our default KDF output length).
pub const BLAKE3_OUT_LEN: usize = 32;

/// Derive `out_len` bytes from `key_material` using `BLAKE3.derive_key` with
/// `context`.
///
/// The context string MUST be unique to its purpose; reuse is a security
/// bug. Spec convention: `"sunrise.<purpose>.v<version>"`.
pub fn derive_key(context: &'static str, key_material: &[u8], out_len: usize) -> Vec<u8> {
    assert!(!context.is_empty(), "BLAKE3 KDF context must be non-empty");
    let mut out = vec![0u8; out_len];
    blake3::Hasher::new_derive_key(context)
        .update(key_material)
        .finalize_xof()
        .fill(&mut out);
    out
}

/// Convenience: derive exactly `BLAKE3_OUT_LEN` bytes into a fixed array.
pub fn derive_key_32(context: &'static str, key_material: &[u8]) -> [u8; 32] {
    let mut out = [0u8; BLAKE3_OUT_LEN];
    blake3::Hasher::new_derive_key(context)
        .update(key_material)
        .finalize_xof()
        .fill(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_output() {
        let a = derive_key_32("sunrise.test.v1", b"input");
        let b = derive_key_32("sunrise.test.v1", b"input");
        assert_eq!(a, b);
    }

    #[test]
    fn different_contexts_diverge() {
        let a = derive_key_32("sunrise.test.v1", b"input");
        let b = derive_key_32("sunrise.test.v2", b"input");
        assert_ne!(a, b);
    }

    #[test]
    fn variable_length() {
        let bytes = derive_key("sunrise.test.v1", b"key", 24);
        assert_eq!(bytes.len(), 24);
        let bytes2 = derive_key("sunrise.test.v1", b"key", 64);
        assert_eq!(bytes2.len(), 64);
        // First 24 bytes should match — XOF is prefix-stable.
        assert_eq!(&bytes[..], &bytes2[..24]);
    }

    #[test]
    #[should_panic(expected = "context must be non-empty")]
    fn empty_context_panics() {
        let _ = derive_key("", b"x", 32);
    }
}
