//! Sunrise's frozen v1 crypto suite.
//!
//! Implements:
//!
//! - `docs/03-crypto/primitives.md` — algorithm choices and parameters.
//! - `docs/03-crypto/identity-and-device-keys.md` — key hierarchy, identity-id.
//! - `docs/03-crypto/data-encryption-format.md` — `OpEnvelope` byte-exact codec.
//! - `docs/03-crypto/key-rotation.md` — Stream-key wrap/unwrap.
//! - `docs/03-crypto/recovery.md` — Argon2id-derived recovery blob.
//!
//! All algorithm choices are FROZEN for v1 (ADR-0004): Ed25519, X25519,
//! XChaCha20-Poly1305, ChaCha20-Poly1305 (HPKE-internal), HPKE Base, Noise XX,
//! BLAKE3 (with KDF mode), Argon2id, TLS 1.3.
//!
//! HPKE share grants and Noise XX pairing have type-level scaffolding here;
//! their full implementations live in `sunrise-pairing` (Phase 9 in the
//! roadmap). Within `sunrise-crypto` we own only the building blocks.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Stylistic clippy lints we relax inside this crate while it stabilizes.
// The workspace-wide policy still applies to other crates; this is a
// targeted local relaxation on a defensive boundary that uses a lot of
// CBOR `Value` matching, integer casts in CBOR conversion, and
// auto-generated CDDL field names that don't survive doc_markdown's
// backtick heuristic. To be tightened in Phase 17 (testing pass).
#![allow(
    clippy::manual_let_else,
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::missing_errors_doc
)]

pub mod aead;
pub mod blake3_kdf;
pub mod blob_chunk;
pub mod device_cert;
pub mod hpke_seal;
pub mod identity;
pub mod keys;
pub mod merkle;
pub mod op_envelope;
pub mod recovery;
pub mod stream_key;
pub mod suite;

pub use aead::{
    aead_open_xchacha, aead_seal_xchacha, AeadError, AEAD_KEY_LEN, AEAD_NONCE_LEN, AEAD_TAG_LEN,
};
pub use blake3_kdf::{derive_key, BLAKE3_OUT_LEN};
pub use blob_chunk::{
    chunk_aad, chunk_count_for, chunk_nonce, content_hash, open_chunk, seal_chunk, verify_content,
    BlobChunkError, CHUNK_PLAINTEXT_LEN,
};
pub use device_cert::{DeviceCert, DeviceCertError, DeviceCertInner};
pub use hpke_seal::{
    hpke_open, hpke_open_identity, hpke_seal, key_envelope_info, HpkeError, HPKE_ENC_LEN,
    HPKE_TAG_LEN,
};
pub use identity::{identity_id_from_pub, IdentityId};
pub use keys::{
    DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair, IdentitySigningKeyPair, RecoveryKey,
    StreamKey, VaultRootKey,
};
pub use merkle::{stream_root_init, stream_root_step};
pub use op_envelope::{
    decode_envelope, encode_envelope, seal_envelope, sign_envelope, verify_envelope, OpEnvelope,
    OpEnvelopeError,
};
pub use recovery::{
    seal_recovery_blob, unseal_recovery_blob, RecoveryError, RECOVERY_NONCE_LEN, RECOVERY_SALT_LEN,
};
pub use stream_key::{
    stream_key_id, unwrap_stream_key, wrap_stream_key, StreamKeyWrapError, STREAM_KEY_ID_LEN,
};
pub use suite::{aead_alg_id, sig_alg_id, AeadAlgId, SigAlgId, SUITE_ID};
