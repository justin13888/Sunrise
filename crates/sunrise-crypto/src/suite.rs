//! v1 crypto suite identifiers.
//!
//! Per `spec/03-crypto/primitives.md`. The suite identifier is byte-exact
//! and frozen; new suites get new ids and a key-rotation flow per
//! `spec/03-crypto/key-rotation.md`.

/// 16-byte HPKE suite identifier per RFC 9180 §7.1: KEM(0x0020) ||
/// KDF(0x0001) || AEAD(0x0003) — DHKEM(X25519, HKDF-SHA-256), HKDF-SHA-256,
/// ChaCha20-Poly1305.
pub const SUITE_ID: [u16; 3] = [0x0020, 0x0001, 0x0003];

/// AEAD algorithm identifier carried in `OpEnvelope.aead_alg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AeadAlgId {
    /// `aead_alg = 0` — control envelope, plaintext payload, signature only.
    None = 0,
    /// `aead_alg = 1` — XChaCha20-Poly1305 with 24-byte random nonce.
    XChaCha20Poly1305 = 1,
}

/// Signature algorithm identifier carried in `OpEnvelope.sig_alg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SigAlgId {
    /// `sig_alg = 1` — Ed25519 over BLAKE3(canonical_cbor_without_field_11).
    Ed25519 = 1,
}

/// Decode an AEAD algorithm id from its wire u32 value.
#[must_use]
pub const fn aead_alg_id(v: u32) -> Option<AeadAlgId> {
    match v {
        0 => Some(AeadAlgId::None),
        1 => Some(AeadAlgId::XChaCha20Poly1305),
        _ => None,
    }
}

/// Decode a signature algorithm id from its wire u32 value.
#[must_use]
pub const fn sig_alg_id(v: u32) -> Option<SigAlgId> {
    match v {
        1 => Some(SigAlgId::Ed25519),
        _ => None,
    }
}
