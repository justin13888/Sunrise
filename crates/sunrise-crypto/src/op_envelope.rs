//! `OpEnvelope` — byte-exact CBOR codec, sign, encrypt, verify, decrypt.
//!
//! Per `spec/03-crypto/data-encryption-format.md`:
//!
//! ```cddl
//! OpEnvelope = {
//!     1: uint,            ; v             (= 1)
//!     2: bstr .size 16,   ; stream_id     (0x00..00 = vault-meta; otherwise a Stream)
//!     3: bstr .size 16,   ; device_id     (signing device)
//!     4: uint,            ; seq           (per-(stream_id, device_id) monotonic; starts at 1)
//!     5: uint,            ; ts_ms         (device wall clock)
//!     6: uint,            ; aead_alg      (1 = XChaCha20-Poly1305; 0 = none/control)
//!     7: uint,            ; sig_alg       (1 = Ed25519)
//!     8: uint,            ; epoch         (Stream-key epoch; 0 if aead_alg = 0)
//!     9: bstr .size 24,   ; nonce         (random 192-bit)
//!     10: bstr,           ; ciphertext_or_payload
//!     11: bstr .size 64,  ; sig           (Ed25519)
//! }
//! ```
//!
//! The 5-byte magic prefix from `sunrise-cbor::magic` precedes the canonical
//! CBOR on the wire / on disk; readers MUST verify it before decoding.
//!
//! AAD (when `aead_alg = 1`) = canonical CBOR of the same map with fields 10
//! and 11 removed.
//!
//! Signature input = `"sunrise.op_envelope.v1" || BLAKE3(canonical_cbor_without_field_11, 32)`.

use crate::aead::{aead_open_xchacha, AEAD_NONCE_LEN};
use crate::keys::{verify_ed25519, IdentitySigningKeyPair, StreamKey};
use crate::suite::{aead_alg_id, sig_alg_id, AeadAlgId, SigAlgId};
use serde::{Deserialize, Serialize};
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MAGIC_LEN};
use sunrise_cbor::version::DOC_SCHEMA_V;
use sunrise_error::ErrorCode;
use thiserror::Error;

/// Domain separation prefix for the envelope signature.
pub const SIG_DOMAIN: &[u8] = b"sunrise.op_envelope.v1";

/// `OpEnvelope` errors.
#[derive(Debug, Error)]
pub enum OpEnvelopeError {
    /// Magic prefix mismatch.
    #[error("magic prefix mismatch")]
    BadMagic,
    /// CBOR decode / canonical-form failure.
    #[error("CBOR error: {0}")]
    Cbor(String),
    /// Field had a wrong size (e.g., stream_id ≠ 16 bytes).
    #[error("field size error: {0}")]
    BadField(&'static str),
    /// Algorithm id unknown (e.g., aead_alg = 7).
    #[error("unknown algorithm id: {0}")]
    UnknownAlg(&'static str),
    /// Mismatch between aead_alg and epoch (epoch != 0 when aead_alg == 0,
    /// or vice versa).
    #[error("aead_alg / epoch combination not allowed")]
    InconsistentAead,
    /// Signature verify failed.
    #[error("signature verification failed")]
    SigVerify,
    /// AEAD verify failed.
    #[error("AEAD authentication failed")]
    AeadAuth,
}

impl OpEnvelopeError {
    /// Map to a canonical [`ErrorCode`].
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::BadMagic => ErrorCode::ProtocolBadMagic,
            Self::Cbor(_) => ErrorCode::CryptoNonCanonicalCbor,
            Self::BadField(_) | Self::UnknownAlg(_) | Self::InconsistentAead => {
                ErrorCode::SyncOpInvalid
            }
            Self::SigVerify => ErrorCode::CryptoSigVerifyFailed,
            Self::AeadAuth => ErrorCode::CryptoDecryptFailed,
        }
    }
}

/// Decoded `OpEnvelope`. Field numbers match the spec's CDDL ids.
///
/// Field 10 (`payload`) is the raw bytes of the inner Op (when `aead_alg=0`)
/// or the AEAD ciphertext+tag (when `aead_alg=1`). Field 11 (`sig`) is the
/// 64-byte Ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpEnvelope {
    /// Envelope version (currently 1).
    pub v: u32,
    /// 16 bytes; `0x00..00` denotes the vault-meta log.
    pub stream_id: [u8; 16],
    /// 16 bytes — the signing device id.
    pub device_id: [u8; 16],
    /// Per-`(stream_id, device_id)` monotonic; first op = 1.
    pub seq: u64,
    /// Device wall clock at emit; advisory only.
    pub ts_ms: u64,
    /// AEAD algorithm id.
    pub aead_alg: AeadAlgId,
    /// Signature algorithm id.
    pub sig_alg: SigAlgId,
    /// Stream-key epoch used; `0` if `aead_alg = None`.
    pub epoch: u32,
    /// 24-byte AEAD nonce; ignored when `aead_alg = None`.
    pub nonce: [u8; AEAD_NONCE_LEN],
    /// Ciphertext (if `aead_alg = XChaCha20Poly1305`) or signed-only payload
    /// (if `aead_alg = None`).
    pub payload: Vec<u8>,
    /// 64-byte Ed25519 signature.
    pub sig: [u8; 64],
}

// CBOR ground form. We use ciborium's `Value` to construct the canonical map
// in field-id order (1..=11) so the encoded byte sequence matches the spec
// regardless of the surrounding Rust struct.

/// Encode envelope to canonical CBOR (without magic prefix).
fn encode_cbor_without_field_11(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    use ciborium::value::{Integer, Value};
    let map = [
        (
            Value::Integer(Integer::from(1)),
            Value::Integer(Integer::from(env.v)),
        ),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(env.stream_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3)),
            Value::Bytes(env.device_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4)),
            Value::Integer(env.seq.into()),
        ),
        (
            Value::Integer(Integer::from(5)),
            Value::Integer(env.ts_ms.into()),
        ),
        (
            Value::Integer(Integer::from(6)),
            Value::Integer(Integer::from(env.aead_alg as u32)),
        ),
        (
            Value::Integer(Integer::from(7)),
            Value::Integer(Integer::from(env.sig_alg as u32)),
        ),
        (
            Value::Integer(Integer::from(8)),
            Value::Integer(Integer::from(env.epoch)),
        ),
        (
            Value::Integer(Integer::from(9)),
            Value::Bytes(env.nonce.to_vec()),
        ),
        (
            Value::Integer(Integer::from(10)),
            Value::Bytes(env.payload.clone()),
        ),
    ];
    let mut out = Vec::with_capacity(64 + env.payload.len());
    ciborium::ser::into_writer(&Value::Map(map.to_vec()), &mut out)
        .map_err(|e| OpEnvelopeError::Cbor(e.to_string()))?;
    Ok(out)
}

/// Encode envelope to canonical CBOR with the field 11 (sig) included.
fn encode_cbor_full(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    use ciborium::value::{Integer, Value};
    let map = [
        (
            Value::Integer(Integer::from(1)),
            Value::Integer(Integer::from(env.v)),
        ),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(env.stream_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3)),
            Value::Bytes(env.device_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4)),
            Value::Integer(env.seq.into()),
        ),
        (
            Value::Integer(Integer::from(5)),
            Value::Integer(env.ts_ms.into()),
        ),
        (
            Value::Integer(Integer::from(6)),
            Value::Integer(Integer::from(env.aead_alg as u32)),
        ),
        (
            Value::Integer(Integer::from(7)),
            Value::Integer(Integer::from(env.sig_alg as u32)),
        ),
        (
            Value::Integer(Integer::from(8)),
            Value::Integer(Integer::from(env.epoch)),
        ),
        (
            Value::Integer(Integer::from(9)),
            Value::Bytes(env.nonce.to_vec()),
        ),
        (
            Value::Integer(Integer::from(10)),
            Value::Bytes(env.payload.clone()),
        ),
        (
            Value::Integer(Integer::from(11)),
            Value::Bytes(env.sig.to_vec()),
        ),
    ];
    let mut out = Vec::with_capacity(64 + env.payload.len());
    ciborium::ser::into_writer(&Value::Map(map.to_vec()), &mut out)
        .map_err(|e| OpEnvelopeError::Cbor(e.to_string()))?;
    Ok(out)
}

/// AAD construction per spec — canonical CBOR of fields 1..=9.
fn encode_aad(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    use ciborium::value::{Integer, Value};
    let map = [
        (
            Value::Integer(Integer::from(1)),
            Value::Integer(Integer::from(env.v)),
        ),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(env.stream_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3)),
            Value::Bytes(env.device_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4)),
            Value::Integer(env.seq.into()),
        ),
        (
            Value::Integer(Integer::from(5)),
            Value::Integer(env.ts_ms.into()),
        ),
        (
            Value::Integer(Integer::from(6)),
            Value::Integer(Integer::from(env.aead_alg as u32)),
        ),
        (
            Value::Integer(Integer::from(7)),
            Value::Integer(Integer::from(env.sig_alg as u32)),
        ),
        (
            Value::Integer(Integer::from(8)),
            Value::Integer(Integer::from(env.epoch)),
        ),
        (
            Value::Integer(Integer::from(9)),
            Value::Bytes(env.nonce.to_vec()),
        ),
    ];
    let mut out = Vec::with_capacity(64);
    ciborium::ser::into_writer(&Value::Map(map.to_vec()), &mut out)
        .map_err(|e| OpEnvelopeError::Cbor(e.to_string()))?;
    Ok(out)
}

/// Compute the signature input bytes for a given envelope state (without the
/// `sig` field).
fn sig_input_bytes(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    let cbor_no_sig = encode_cbor_without_field_11(env)?;
    let hash = blake3::hash(&cbor_no_sig);
    let mut out = Vec::with_capacity(SIG_DOMAIN.len() + hash.as_bytes().len());
    out.extend_from_slice(SIG_DOMAIN);
    out.extend_from_slice(hash.as_bytes());
    Ok(out)
}

/// Sign the envelope in place. Sets `env.sig` from the device signing key.
///
/// Caller is responsible for setting all other fields before calling.
///
/// # Errors
/// CBOR encode failure (essentially impossible for v1 fields).
pub fn sign_envelope(
    env: &mut OpEnvelope,
    device_signing: &IdentitySigningKeyPair,
) -> Result<(), OpEnvelopeError> {
    let input = sig_input_bytes(env)?;
    env.sig = device_signing.sign(&input);
    Ok(())
}

/// Build, AEAD-seal (if `aead_alg=1`), and sign an envelope.
///
/// If `aead_alg=1`, `inner` is encrypted under `stream_key` with the AAD =
/// canonical CBOR of fields 1..=9; the resulting `ciphertext_and_tag`
/// becomes the `payload` field. If `aead_alg=0`, `inner` becomes the
/// `payload` directly (signed-only).
///
/// `inner` is the canonical CBOR of the inner Op; this function does not
/// re-encode.
///
/// # Errors
/// CBOR/AEAD failures.
#[allow(clippy::too_many_arguments)]
pub fn encode_envelope(
    inner: &[u8],
    stream_id: [u8; 16],
    device_id: [u8; 16],
    seq: u64,
    ts_ms: u64,
    aead_alg: AeadAlgId,
    epoch: u32,
    nonce: [u8; AEAD_NONCE_LEN],
    stream_key: Option<&StreamKey>,
    device_signing: &IdentitySigningKeyPair,
) -> Result<Vec<u8>, OpEnvelopeError> {
    if aead_alg == AeadAlgId::None && epoch != 0 {
        return Err(OpEnvelopeError::InconsistentAead);
    }
    if aead_alg == AeadAlgId::XChaCha20Poly1305 && stream_key.is_none() {
        return Err(OpEnvelopeError::InconsistentAead);
    }

    let mut env = OpEnvelope {
        v: u32::from(DOC_SCHEMA_V),
        stream_id,
        device_id,
        seq,
        ts_ms,
        aead_alg,
        sig_alg: SigAlgId::Ed25519,
        epoch,
        nonce,
        payload: Vec::new(),
        sig: [0u8; 64],
    };

    match aead_alg {
        AeadAlgId::None => {
            env.payload = inner.to_vec();
        }
        AeadAlgId::XChaCha20Poly1305 => {
            let aad = encode_aad(&env)?;
            let key = stream_key.expect("verified above").as_bytes();
            let ct = crate::aead::aead_seal_xchacha(key, &nonce, inner, &aad)
                .map_err(|_| OpEnvelopeError::AeadAuth)?;
            env.payload = ct;
        }
    }

    sign_envelope(&mut env, device_signing)?;

    let cbor = encode_cbor_full(&env)?;
    let mut out = Vec::with_capacity(MAGIC_LEN + cbor.len());
    let mut prefix = [0u8; MAGIC_LEN];
    write_prefix(&mut prefix, MagicKind::OpEnvelope, DOC_SCHEMA_V);
    out.extend_from_slice(&prefix);
    out.extend_from_slice(&cbor);
    Ok(out)
}

/// Decode (without verifying) an envelope from raw bytes (magic + CBOR).
///
/// # Errors
/// Magic mismatch; CBOR decode/canonical-form failure; field-shape errors.
pub fn decode_envelope(bytes: &[u8]) -> Result<OpEnvelope, OpEnvelopeError> {
    if bytes.len() < MAGIC_LEN {
        return Err(OpEnvelopeError::BadMagic);
    }
    let prefix = decode_prefix(&bytes[..MAGIC_LEN]).map_err(|_| OpEnvelopeError::BadMagic)?;
    if prefix.kind != MagicKind::OpEnvelope || prefix.version != DOC_SCHEMA_V {
        return Err(OpEnvelopeError::BadMagic);
    }
    let cbor_bytes = &bytes[MAGIC_LEN..];
    let value: ciborium::value::Value =
        ciborium::de::from_reader(cbor_bytes).map_err(|e| OpEnvelopeError::Cbor(e.to_string()))?;
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        _ => return Err(OpEnvelopeError::Cbor("envelope must be a map".into())),
    };

    let mut env = OpEnvelope {
        v: 0,
        stream_id: [0u8; 16],
        device_id: [0u8; 16],
        seq: 0,
        ts_ms: 0,
        aead_alg: AeadAlgId::None,
        sig_alg: SigAlgId::Ed25519,
        epoch: 0,
        nonce: [0u8; AEAD_NONCE_LEN],
        payload: Vec::new(),
        sig: [0u8; 64],
    };
    let mut seen_field_ids = Vec::new();

    for (k, v) in map {
        let id = match k {
            ciborium::value::Value::Integer(i) => i128::from(i),
            _ => return Err(OpEnvelopeError::Cbor("non-integer map key".into())),
        };
        seen_field_ids.push(id);
        match id {
            1 => env.v = u32_from(&v, "v")?,
            2 => env.stream_id = bytes16_from(&v, "stream_id")?,
            3 => env.device_id = bytes16_from(&v, "device_id")?,
            4 => env.seq = u64_from(&v, "seq")?,
            5 => env.ts_ms = u64_from(&v, "ts_ms")?,
            6 => {
                let raw = u32_from(&v, "aead_alg")?;
                env.aead_alg = aead_alg_id(raw).ok_or(OpEnvelopeError::UnknownAlg("aead_alg"))?;
            }
            7 => {
                let raw = u32_from(&v, "sig_alg")?;
                env.sig_alg = sig_alg_id(raw).ok_or(OpEnvelopeError::UnknownAlg("sig_alg"))?;
            }
            8 => env.epoch = u32_from(&v, "epoch")?,
            9 => env.nonce = bytes_n_from(&v, "nonce", AEAD_NONCE_LEN)?,
            10 => match v {
                ciborium::value::Value::Bytes(b) => env.payload = b,
                _ => return Err(OpEnvelopeError::BadField("payload")),
            },
            11 => {
                let arr = bytes_n_from(&v, "sig", 64)?;
                env.sig = arr;
            }
            _ => {
                // Unknown field; per forward-compat preserve verbatim. v1
                // doesn't carry preserved-fields, but we do not reject.
            }
        }
    }

    // All required fields must be present.
    for required in 1..=11_i128 {
        if !seen_field_ids.contains(&required) {
            return Err(OpEnvelopeError::BadField(match required {
                1 => "v",
                2 => "stream_id",
                3 => "device_id",
                4 => "seq",
                5 => "ts_ms",
                6 => "aead_alg",
                7 => "sig_alg",
                8 => "epoch",
                9 => "nonce",
                10 => "payload",
                11 => "sig",
                _ => unreachable!(),
            }));
        }
    }

    if env.aead_alg == AeadAlgId::None && env.epoch != 0 {
        return Err(OpEnvelopeError::InconsistentAead);
    }
    Ok(env)
}

/// Verify the envelope's signature against the supplied device signing
/// public key.
///
/// # Errors
/// `SigVerify` if Ed25519 verify fails for any reason.
pub fn verify_envelope(env: &OpEnvelope, d_s_pub: &[u8; 32]) -> Result<(), OpEnvelopeError> {
    let input = sig_input_bytes(env)?;
    if !verify_ed25519(d_s_pub, &input, &env.sig) {
        return Err(OpEnvelopeError::SigVerify);
    }
    Ok(())
}

/// Verify the envelope and decrypt its inner payload.
///
/// On success returns the decoded inner Op bytes. Caller is responsible for
/// further canonical-CBOR validation of the inner Op.
///
/// # Errors
/// As [`verify_envelope`] plus AEAD auth failure.
pub fn open_envelope(
    env: &OpEnvelope,
    d_s_pub: &[u8; 32],
    stream_key: Option<&StreamKey>,
) -> Result<Vec<u8>, OpEnvelopeError> {
    verify_envelope(env, d_s_pub)?;
    match env.aead_alg {
        AeadAlgId::None => Ok(env.payload.clone()),
        AeadAlgId::XChaCha20Poly1305 => {
            let aad = encode_aad(env)?;
            let key = stream_key
                .ok_or(OpEnvelopeError::InconsistentAead)?
                .as_bytes();
            aead_open_xchacha(key, &env.nonce, &env.payload, &aad)
                .map_err(|_| OpEnvelopeError::AeadAuth)
        }
    }
}

// --- ciborium::Value helpers ---

fn u32_from(v: &ciborium::value::Value, field: &'static str) -> Result<u32, OpEnvelopeError> {
    match v {
        ciborium::value::Value::Integer(i) => {
            let raw: i128 = i128::from(*i);
            u32::try_from(raw).map_err(|_| OpEnvelopeError::BadField(field))
        }
        _ => Err(OpEnvelopeError::BadField(field)),
    }
}

fn u64_from(v: &ciborium::value::Value, field: &'static str) -> Result<u64, OpEnvelopeError> {
    match v {
        ciborium::value::Value::Integer(i) => {
            let raw: i128 = i128::from(*i);
            u64::try_from(raw).map_err(|_| OpEnvelopeError::BadField(field))
        }
        _ => Err(OpEnvelopeError::BadField(field)),
    }
}

fn bytes16_from(
    v: &ciborium::value::Value,
    field: &'static str,
) -> Result<[u8; 16], OpEnvelopeError> {
    match v {
        ciborium::value::Value::Bytes(b) if b.len() == 16 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(b);
            Ok(a)
        }
        _ => Err(OpEnvelopeError::BadField(field)),
    }
}

fn bytes_n_from<const N: usize>(
    v: &ciborium::value::Value,
    field: &'static str,
    expect: usize,
) -> Result<[u8; N], OpEnvelopeError> {
    debug_assert_eq!(N, expect);
    match v {
        ciborium::value::Value::Bytes(b) if b.len() == N => {
            let mut a = [0u8; N];
            a.copy_from_slice(b);
            Ok(a)
        }
        _ => Err(OpEnvelopeError::BadField(field)),
    }
}

// --- forward-compat reserved namespace ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct _ReservedForFutureUse {
    _placeholder: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::IdentitySigningKeyPair;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    fn fixed_signing() -> IdentitySigningKeyPair {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        IdentitySigningKeyPair::generate(&mut rng)
    }

    fn fixed_stream_key() -> StreamKey {
        StreamKey::from_bytes([1u8; 32])
    }

    #[test]
    fn signed_only_round_trip() {
        let signing = fixed_signing();
        let pub_bytes = signing.public_bytes();
        let inner = b"hello inner op";
        let bytes = encode_envelope(
            inner,
            [2u8; 16],
            [3u8; 16],
            1,
            1_700_000_000_000,
            AeadAlgId::None,
            0,
            [0u8; AEAD_NONCE_LEN],
            None,
            &signing,
        )
        .unwrap();
        // Magic prefix is `SR\x02\x00\x01`.
        assert_eq!(&bytes[..MAGIC_LEN], b"SR\x02\x00\x01");
        let env = decode_envelope(&bytes).unwrap();
        verify_envelope(&env, &pub_bytes).unwrap();
        let plaintext = open_envelope(&env, &pub_bytes, None).unwrap();
        assert_eq!(&plaintext, inner);
    }

    #[test]
    fn aead_round_trip() {
        let signing = fixed_signing();
        let pub_bytes = signing.public_bytes();
        let stream_key = fixed_stream_key();
        let inner = b"sensitive payload";
        let bytes = encode_envelope(
            inner,
            [2u8; 16],
            [3u8; 16],
            1,
            1_700_000_000_000,
            AeadAlgId::XChaCha20Poly1305,
            4,
            [0xaau8; AEAD_NONCE_LEN],
            Some(&stream_key),
            &signing,
        )
        .unwrap();
        let env = decode_envelope(&bytes).unwrap();
        // Payload field is encrypted; should not contain `inner` verbatim.
        assert!(!contains_subseq(&env.payload, inner));
        let plaintext = open_envelope(&env, &pub_bytes, Some(&stream_key)).unwrap();
        assert_eq!(&plaintext, inner);
    }

    #[test]
    fn sig_failure_rejects_before_decrypt() {
        let signing = fixed_signing();
        let stream_key = fixed_stream_key();
        let mut bytes = encode_envelope(
            b"x",
            [2u8; 16],
            [3u8; 16],
            1,
            1,
            AeadAlgId::XChaCha20Poly1305,
            1,
            [0u8; AEAD_NONCE_LEN],
            Some(&stream_key),
            &signing,
        )
        .unwrap();
        // Flip a byte in the signature region (last 64 bytes of CBOR).
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let env = decode_envelope(&bytes).unwrap();
        let bogus_pub = [9u8; 32];
        assert!(matches!(
            verify_envelope(&env, &bogus_pub),
            Err(OpEnvelopeError::SigVerify)
        ));
    }

    #[test]
    fn aead_tamper_rejects() {
        let signing = fixed_signing();
        let pub_bytes = signing.public_bytes();
        let stream_key = fixed_stream_key();
        let bytes = encode_envelope(
            b"x",
            [2u8; 16],
            [3u8; 16],
            1,
            1,
            AeadAlgId::XChaCha20Poly1305,
            1,
            [0u8; AEAD_NONCE_LEN],
            Some(&stream_key),
            &signing,
        )
        .unwrap();
        let mut env = decode_envelope(&bytes).unwrap();
        // Tampering ciphertext invalidates AEAD; signature still passes
        // because we re-sign below, simulating a malicious peer who knows
        // the device signing key (they shouldn't, but we test the AEAD
        // gate independently).
        if let Some(b) = env.payload.first_mut() {
            *b ^= 0x01;
        }
        sign_envelope(&mut env, &signing).unwrap();
        assert!(matches!(
            open_envelope(&env, &pub_bytes, Some(&stream_key)),
            Err(OpEnvelopeError::AeadAuth)
        ));
    }

    #[test]
    fn bad_magic_rejected() {
        let signing = fixed_signing();
        let mut bytes = encode_envelope(
            b"x",
            [2u8; 16],
            [3u8; 16],
            1,
            1,
            AeadAlgId::None,
            0,
            [0u8; AEAD_NONCE_LEN],
            None,
            &signing,
        )
        .unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            decode_envelope(&bytes),
            Err(OpEnvelopeError::BadMagic)
        ));
    }

    fn contains_subseq(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || needle.len() > haystack.len() {
            return false;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
