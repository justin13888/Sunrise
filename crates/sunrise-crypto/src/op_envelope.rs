//! `OpEnvelope` — byte-exact CBOR codec, sign, encrypt, verify, decrypt.
//!
//! Per `docs/03-crypto/data-encryption-format.md`:
//!
//! ```cddl
//! OpEnvelope = {
//!     1: uint,            ; v             (= ENVELOPE_FORMAT_V, currently 3)
//!     2: bstr .size 16,   ; stream_id     (0x00..00 = vault-meta; otherwise a Stream)
//!     3: bstr .size 16,   ; device_id     (signing device)
//!     4: uint,            ; seq           (per-(stream_id, device_id) monotonic; starts at 1)
//!     5: [uint, uint],    ; hlc           ([physical_ms, logical]; see sunrise_cbor::hlc)
//!     6: uint,            ; aead_alg      (1 = XChaCha20-Poly1305; 0 = none/control)
//!     7: uint,            ; sig_alg       (1 = Ed25519)
//!     8: uint,            ; epoch         (Stream-key epoch; 0 if aead_alg = 0)
//!     9: bstr .size 24,   ; nonce         (random 192-bit)
//!     10: bstr,           ; ciphertext_or_payload
//!     11: bstr .size 64,  ; sig           (Ed25519)
//!     12: uint,           ; doc_schema_v  (schema of the *payload*, >= DOC_SCHEMA_FLOOR)
//! }
//! ```
//!
//! Field 5 is a hybrid logical clock, not a bare wall clock: a two-element array
//! `[physical_ms, logical]`. Ordering ops by an unbounded device wall clock let
//! one skewed device win every conflict forever (issue #21); the logical
//! component is what makes the order consistent with causality when the wall
//! clocks disagree.
//!
//! Field 1 is the **container format** version and field 12 the **document
//! schema** version. They are separate on purpose (ADR-0015): a decoder that
//! does not implement the container format cannot find the payload and must
//! refuse, whereas a decoder meeting an unfamiliar document schema can still
//! locate, authenticate, and decrypt the payload — so it accepts anything at or
//! above [`DOC_SCHEMA_FLOOR`].
//!
//! Field ids are non-negative CBOR integers, so deterministic (RFC 8949 §4.2.1)
//! key ordering is plain numeric ordering: field 12 encodes as `0x0c` and
//! therefore sorts *after* field 11. It is nonetheless covered by both the AAD
//! and the signature, because both are defined by which fields they *exclude*,
//! not by an upper bound.
//!
//! The 5-byte magic prefix from `sunrise-cbor::magic` precedes the canonical
//! CBOR on the wire / on disk; readers MUST verify it before decoding. It
//! carries `ENVELOPE_FORMAT_V`.
//!
//! AAD (when `aead_alg = 1`) = canonical CBOR of the same map with fields 10
//! and 11 removed — i.e. `{1..9, 12}`.
//!
//! Signature input = `"sunrise.op_envelope.v1" || BLAKE3(canonical_cbor_without_field_11, 32)`
//! — i.e. over `{1..10, 12}`.

use crate::aead::{aead_open_xchacha, AEAD_NONCE_LEN};
use crate::keys::{verify_ed25519, IdentitySigningKeyPair, StreamKey};
use crate::suite::{aead_alg_id, sig_alg_id, AeadAlgId, SigAlgId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use sunrise_cbor::hlc::Hlc;
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MAGIC_LEN};
use sunrise_cbor::version::{DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, ENVELOPE_FORMAT_V};
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
    /// The envelope's `doc_schema_v` (field 12) is below this build's floor.
    #[error("doc_schema_v {got} is below the readable floor {floor}")]
    DocSchemaTooOld {
        /// The envelope's declared document schema version.
        got: u32,
        /// This build's [`DOC_SCHEMA_FLOOR`].
        floor: u32,
    },
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
            Self::DocSchemaTooOld { .. } => ErrorCode::DocSchemaTooOld,
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
    /// Envelope **container format** version — `ENVELOPE_FORMAT_V`.
    pub v: u32,
    /// 16 bytes; `0x00..00` denotes the vault-meta log.
    pub stream_id: [u8; 16],
    /// 16 bytes — the signing device id.
    pub device_id: [u8; 16],
    /// Per-`(stream_id, device_id)` monotonic; first op = 1.
    pub seq: u64,
    /// Hybrid logical clock at emit. Its `physical_ms` doubles as the device
    /// wall clock for cert resolution; the pair as a whole is the ordering key
    /// for last-writer-wins.
    pub hlc: Hlc,
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
    /// Document schema version of the payload (field 12). Independent of
    /// [`OpEnvelope::v`]; any value `>= DOC_SCHEMA_FLOOR` is readable.
    pub doc_schema_v: u32,
    /// Envelope fields this build does not know, kept verbatim and re-emitted
    /// in their canonical position.
    ///
    /// Discarding them — which is what the decoder did until now, with a
    /// comment claiming otherwise — silently broke the signature for anyone
    /// downstream: the sender signed over the field, so a re-encode without it
    /// no longer verifies. Preserving them is what makes an envelope from a
    /// newer container format survive a hop through this build.
    pub unknown: BTreeMap<u64, sunrise_cbor::CborValue>,
}

// CBOR ground form. We build the canonical map explicitly, in ascending
// field-id order, so the encoded byte sequence matches the spec regardless of
// the surrounding Rust struct. Field ids are non-negative integers, so RFC 8949
// deterministic key ordering is numeric ordering (1..=12 all encode as a single
// byte 0x01..=0x0c).
//
// The three encodings the format needs differ ONLY in which fields they omit,
// so they are one function. Defining them by exclusion — rather than as
// "fields 1..=9" and "fields 1..=10" — is what makes a newly added field land
// inside the AAD and the signature automatically instead of silently escaping
// both.

/// Which fields to omit from a canonical encoding of the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Omit {
    /// Everything: the full on-the-wire encoding.
    Nothing,
    /// Field 11 (`sig`) — the signature input.
    Sig,
    /// Fields 10 (`payload`) and 11 (`sig`) — the AEAD associated data.
    PayloadAndSig,
}

impl Omit {
    const fn omits(self, field_id: u8) -> bool {
        match self {
            Self::Nothing => false,
            Self::Sig => field_id == 11,
            Self::PayloadAndSig => field_id == 10 || field_id == 11,
        }
    }
}

/// Encode the envelope to canonical CBOR, omitting the fields `omit` names.
fn encode_cbor(env: &OpEnvelope, omit: Omit) -> Result<Vec<u8>, OpEnvelopeError> {
    use ciborium::value::{Integer, Value};

    let mut map: Vec<(u64, Value)> = Vec::with_capacity(12 + env.unknown.len());
    let mut put = |id: u8, v: Value| {
        if !omit.omits(id) {
            map.push((u64::from(id), v));
        }
    };

    put(1, Value::Integer(Integer::from(env.v)));
    put(2, Value::Bytes(env.stream_id.to_vec()));
    put(3, Value::Bytes(env.device_id.to_vec()));
    put(4, Value::Integer(env.seq.into()));
    put(
        5,
        Value::Array(vec![
            Value::Integer(env.hlc.physical_ms.into()),
            Value::Integer(Integer::from(env.hlc.logical)),
        ]),
    );
    put(6, Value::Integer(Integer::from(env.aead_alg as u32)));
    put(7, Value::Integer(Integer::from(env.sig_alg as u32)));
    put(8, Value::Integer(Integer::from(env.epoch)));
    put(9, Value::Bytes(env.nonce.to_vec()));
    put(10, Value::Bytes(env.payload.clone()));
    put(11, Value::Bytes(env.sig.to_vec()));
    put(12, Value::Integer(Integer::from(env.doc_schema_v)));

    // Preserved unknowns sit at their own field ids. Sorting the whole map by
    // id afterwards puts each one exactly where its author put it: field ids
    // are non-negative integers, so deterministic CBOR key order IS numeric
    // order, and a byte-identical re-emission is what keeps the ORIGINAL
    // signature verifiable after this build has handled the envelope.
    for (id, value) in &env.unknown {
        map.push((*id, value.0.clone()));
    }
    map.sort_by_key(|(id, _)| *id);

    let entries: Vec<(Value, Value)> = map
        .into_iter()
        .map(|(id, v)| (Value::Integer(Integer::from(id)), v))
        .collect();
    let mut out = Vec::with_capacity(64 + env.payload.len());
    ciborium::ser::into_writer(&Value::Map(entries), &mut out)
        .map_err(|e| OpEnvelopeError::Cbor(e.to_string()))?;
    Ok(out)
}

/// AAD construction per spec — canonical CBOR with fields 10 and 11 removed.
fn encode_aad(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    encode_cbor(env, Omit::PayloadAndSig)
}

/// Compute the signature input bytes for a given envelope state (without the
/// `sig` field).
fn sig_input_bytes(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    let cbor_no_sig = encode_cbor(env, Omit::Sig)?;
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
    hlc: Hlc,
    aead_alg: AeadAlgId,
    epoch: u32,
    nonce: [u8; AEAD_NONCE_LEN],
    stream_key: Option<&StreamKey>,
    device_signing: &IdentitySigningKeyPair,
) -> Result<Vec<u8>, OpEnvelopeError> {
    let env = OpEnvelope {
        v: u32::from(ENVELOPE_FORMAT_V),
        doc_schema_v: u32::from(DOC_SCHEMA_V),
        stream_id,
        device_id,
        seq,
        hlc,
        aead_alg,
        sig_alg: SigAlgId::Ed25519,
        epoch,
        nonce,
        payload: inner.to_vec(),
        sig: [0u8; 64],
        unknown: BTreeMap::new(),
    };
    seal_envelope(env, stream_key, device_signing)
}

/// Seal (when `aead_alg = 1`), sign, and encode a pre-built envelope.
///
/// `env.payload` holds the *plaintext* inner op on entry; `env.sig` is ignored.
/// This is the seam [`encode_envelope`] is built on, exposed so a caller that
/// must control fields the convenience wrapper stamps for it — notably
/// `doc_schema_v` — can do so without reimplementing the codec.
///
/// # Errors
/// `InconsistentAead` for an aead/epoch/key mismatch; CBOR/AEAD failures.
pub fn seal_envelope(
    mut env: OpEnvelope,
    stream_key: Option<&StreamKey>,
    device_signing: &IdentitySigningKeyPair,
) -> Result<Vec<u8>, OpEnvelopeError> {
    if env.aead_alg == AeadAlgId::None && env.epoch != 0 {
        return Err(OpEnvelopeError::InconsistentAead);
    }
    if env.aead_alg == AeadAlgId::XChaCha20Poly1305 && stream_key.is_none() {
        return Err(OpEnvelopeError::InconsistentAead);
    }

    if env.aead_alg == AeadAlgId::XChaCha20Poly1305 {
        let inner = std::mem::take(&mut env.payload);
        let aad = encode_aad(&env)?;
        let key = stream_key.expect("verified above").as_bytes();
        env.payload = crate::aead::aead_seal_xchacha(key, &env.nonce, &inner, &aad)
            .map_err(|_| OpEnvelopeError::AeadAuth)?;
    }

    sign_envelope(&mut env, device_signing)?;

    let cbor = encode_cbor(&env, Omit::Nothing)?;
    let mut out = Vec::with_capacity(MAGIC_LEN + cbor.len());
    let mut prefix = [0u8; MAGIC_LEN];
    write_prefix(&mut prefix, MagicKind::OpEnvelope, ENVELOPE_FORMAT_V);
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
    // The prefix carries the CONTAINER FORMAT version. A mismatch is the one
    // thing a decoder cannot work around — it does not know the field layout,
    // so it cannot even find the payload. The document schema is field 12 and
    // is checked further down against the floor, not for equality.
    if prefix.kind != MagicKind::OpEnvelope || prefix.version != ENVELOPE_FORMAT_V {
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
        doc_schema_v: 0,
        stream_id: [0u8; 16],
        device_id: [0u8; 16],
        seq: 0,
        hlc: Hlc::at(0),
        aead_alg: AeadAlgId::None,
        sig_alg: SigAlgId::Ed25519,
        epoch: 0,
        nonce: [0u8; AEAD_NONCE_LEN],
        payload: Vec::new(),
        sig: [0u8; 64],
        unknown: BTreeMap::new(),
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
            5 => env.hlc = hlc_from(&v)?,
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
            12 => env.doc_schema_v = u32_from(&v, "doc_schema_v")?,
            other => {
                // Forward compat, for real this time. A field from a newer
                // container format is kept at its own id and re-emitted in
                // exactly the position it arrived in, so the sender's signature
                // still verifies downstream. A negative id cannot appear in a
                // Sunrise envelope and is refused rather than silently coerced.
                let id = u64::try_from(other)
                    .map_err(|_| OpEnvelopeError::BadField("negative field id"))?;
                if contains_float_value(&v) {
                    return Err(OpEnvelopeError::BadField("float in envelope"));
                }
                env.unknown.insert(id, sunrise_cbor::CborValue(v));
            }
        }
    }

    // All required fields must be present.
    for required in 1..=12_i128 {
        if !seen_field_ids.contains(&required) {
            return Err(OpEnvelopeError::BadField(match required {
                1 => "v",
                2 => "stream_id",
                3 => "device_id",
                4 => "seq",
                5 => "hlc",
                6 => "aead_alg",
                7 => "sig_alg",
                8 => "epoch",
                9 => "nonce",
                10 => "payload",
                11 => "sig",
                12 => "doc_schema_v",
                _ => unreachable!(),
            }));
        }
    }

    if env.v != u32::from(ENVELOPE_FORMAT_V) {
        return Err(OpEnvelopeError::BadField("v"));
    }
    // Forward compatibility: a NEWER document schema is readable. Only an
    // older-than-floor one is not.
    if env.doc_schema_v < u32::from(DOC_SCHEMA_FLOOR) {
        return Err(OpEnvelopeError::DocSchemaTooOld {
            got: env.doc_schema_v,
            floor: u32::from(DOC_SCHEMA_FLOOR),
        });
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

/// Canonical Sunrise CBOR has no floats anywhere, so a float in an unknown
/// envelope field is malformed rather than merely unfamiliar.
fn contains_float_value(v: &ciborium::value::Value) -> bool {
    use ciborium::value::Value;
    match v {
        Value::Float(_) => true,
        Value::Array(items) => items.iter().any(contains_float_value),
        Value::Map(entries) => entries
            .iter()
            .any(|(k, val)| contains_float_value(k) || contains_float_value(val)),
        Value::Tag(_, inner) => contains_float_value(inner),
        _ => false,
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

/// Field 5 is `[physical_ms, logical]`. A bare integer is the pre-HLC shape
/// and is not accepted: it would silently decode as `logical = 0`, which is a
/// *valid* HLC and would therefore be indistinguishable from a real one.
fn hlc_from(v: &ciborium::value::Value) -> Result<Hlc, OpEnvelopeError> {
    let arr = match v {
        ciborium::value::Value::Array(a) if a.len() == 2 => a,
        _ => return Err(OpEnvelopeError::BadField("hlc")),
    };
    Ok(Hlc {
        physical_ms: u64_from(&arr[0], "hlc.physical_ms")?,
        logical: u32_from(&arr[1], "hlc.logical")?,
    })
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
            Hlc::at(1_700_000_000_000),
            AeadAlgId::None,
            0,
            [0u8; AEAD_NONCE_LEN],
            None,
            &signing,
        )
        .unwrap();
        // Magic prefix carries ENVELOPE_FORMAT_V: `SR\x02\x00\x03`.
        assert_eq!(&bytes[..MAGIC_LEN], b"SR\x02\x00\x03");
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
            Hlc::at(1_700_000_000_000),
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

    /// The relay routes by `(stream_id, device_id, seq)` using a header-only
    /// reader that links no crypto at all. If the two decoders ever disagreed
    /// about where those ids live, the relay would filter the wrong frames —
    /// so a real sealed envelope is checked against both here, in the crate
    /// that owns the format.
    #[test]
    fn header_only_reader_agrees_with_the_full_decoder() {
        let signing = fixed_signing();
        let stream_key = fixed_stream_key();
        let bytes = encode_envelope(
            b"opaque to a relay",
            [0x5au8; 16],
            [0xc3u8; 16],
            77,
            Hlc::at(1_700_000_000_000),
            AeadAlgId::XChaCha20Poly1305,
            4,
            [0xaau8; AEAD_NONCE_LEN],
            Some(&stream_key),
            &signing,
        )
        .unwrap();
        let full = decode_envelope(&bytes).unwrap();
        let head = sunrise_cbor::decode_envelope_header(&bytes).unwrap();
        assert_eq!(head.stream_id, full.stream_id);
        assert_eq!(head.device_id, full.device_id);
        assert_eq!(head.seq, full.seq);
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
            Hlc::at(1),
            AeadAlgId::XChaCha20Poly1305,
            1,
            [0u8; AEAD_NONCE_LEN],
            Some(&stream_key),
            &signing,
        )
        .unwrap();
        // Flip a byte in the signature region. Field 11 is no longer last —
        // field 12 (`doc_schema_v`) encodes as the trailing `0c 01` — so step
        // back past it to land inside the 64-byte signature.
        let in_sig = bytes.len() - 3;
        bytes[in_sig] ^= 0x01;
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
            Hlc::at(1),
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
            Hlc::at(1),
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

    /// Build an envelope carrying an arbitrary `doc_schema_v`, the way a
    /// future build would.
    fn envelope_at_doc_schema(doc_schema_v: u32, signing: &IdentitySigningKeyPair) -> Vec<u8> {
        seal_envelope(
            OpEnvelope {
                v: u32::from(ENVELOPE_FORMAT_V),
                doc_schema_v,
                stream_id: [2u8; 16],
                device_id: [3u8; 16],
                seq: 1,
                hlc: Hlc::at(1_700_000_000_000),
                aead_alg: AeadAlgId::None,
                sig_alg: SigAlgId::Ed25519,
                epoch: 0,
                nonce: [0u8; AEAD_NONCE_LEN],
                payload: b"inner".to_vec(),
                sig: [0u8; 64],
                unknown: BTreeMap::new(),
            },
            None,
            signing,
        )
        .unwrap()
    }

    #[test]
    fn envelope_carries_both_versions_separately() {
        let signing = fixed_signing();
        let bytes = encode_envelope(
            b"x",
            [2u8; 16],
            [3u8; 16],
            1,
            Hlc::at(1),
            AeadAlgId::None,
            0,
            [0u8; AEAD_NONCE_LEN],
            None,
            &signing,
        )
        .unwrap();
        let env = decode_envelope(&bytes).unwrap();
        assert_eq!(env.v, u32::from(ENVELOPE_FORMAT_V));
        assert_eq!(env.doc_schema_v, u32::from(DOC_SCHEMA_V));
    }

    /// The point of the split. A build that has never heard of doc schema 99
    /// still decodes, authenticates, and opens the envelope — it only cannot
    /// interpret whatever new fields the *payload* carries.
    #[test]
    fn newer_doc_schema_still_decodes_and_verifies() {
        let signing = fixed_signing();
        let pub_bytes = signing.public_bytes();
        let bytes = envelope_at_doc_schema(99, &signing);
        let env = decode_envelope(&bytes).expect("a newer doc schema is readable");
        assert_eq!(env.doc_schema_v, 99);
        assert_eq!(env.v, u32::from(ENVELOPE_FORMAT_V));
        let plaintext = open_envelope(&env, &pub_bytes, None).unwrap();
        assert_eq!(plaintext, b"inner");
    }

    /// ...but a schema below the floor is refused, with its own error rather
    /// than the container-format one.
    #[test]
    fn doc_schema_below_floor_is_refused() {
        let signing = fixed_signing();
        let bytes = envelope_at_doc_schema(u32::from(DOC_SCHEMA_FLOOR) - 1, &signing);
        assert!(matches!(
            decode_envelope(&bytes),
            Err(OpEnvelopeError::DocSchemaTooOld { got: 0, floor: 1 })
        ));
    }

    /// A container-format mismatch is fatal in the prefix, before any CBOR is
    /// parsed — the decoder does not know the layout, so it cannot find a
    /// payload to salvage.
    #[test]
    fn foreign_envelope_format_is_rejected_at_the_prefix() {
        let signing = fixed_signing();
        let mut bytes = envelope_at_doc_schema(1, &signing);
        // Bump the prefix's version word to a format this build has never seen.
        bytes[3] = 0xff;
        bytes[4] = 0xff;
        assert!(matches!(
            decode_envelope(&bytes),
            Err(OpEnvelopeError::BadMagic)
        ));
    }

    /// Field 12 rides inside the signature input. Rewriting it therefore
    /// invalidates the signature rather than passing unnoticed.
    #[test]
    fn doc_schema_is_covered_by_the_signature() {
        let signing = fixed_signing();
        let pub_bytes = signing.public_bytes();
        let bytes = envelope_at_doc_schema(1, &signing);
        let mut env = decode_envelope(&bytes).unwrap();
        verify_envelope(&env, &pub_bytes).unwrap();
        env.doc_schema_v = 2;
        assert!(matches!(
            verify_envelope(&env, &pub_bytes),
            Err(OpEnvelopeError::SigVerify)
        ));
    }

    /// ...and inside the AEAD associated data, so it cannot be rewritten by
    /// anyone who does not hold the Stream key either.
    #[test]
    fn doc_schema_is_covered_by_the_aad() {
        let signing = fixed_signing();
        let pub_bytes = signing.public_bytes();
        let stream_key = fixed_stream_key();
        let bytes = seal_envelope(
            OpEnvelope {
                v: u32::from(ENVELOPE_FORMAT_V),
                doc_schema_v: 1,
                stream_id: [2u8; 16],
                device_id: [3u8; 16],
                seq: 1,
                hlc: Hlc::at(1),
                aead_alg: AeadAlgId::XChaCha20Poly1305,
                sig_alg: SigAlgId::Ed25519,
                epoch: 1,
                nonce: [0u8; AEAD_NONCE_LEN],
                payload: b"secret".to_vec(),
                sig: [0u8; 64],
                unknown: BTreeMap::new(),
            },
            Some(&stream_key),
            &signing,
        )
        .unwrap();
        let mut env = decode_envelope(&bytes).unwrap();
        env.doc_schema_v = 7;
        // Re-sign, so the signature gate cannot be what rejects it.
        sign_envelope(&mut env, &signing).unwrap();
        assert!(matches!(
            open_envelope(&env, &pub_bytes, Some(&stream_key)),
            Err(OpEnvelopeError::AeadAuth)
        ));
    }

    #[test]
    fn missing_doc_schema_field_is_refused() {
        // A well-formed v2 container MUST carry field 12; a map without it is
        // not a v2 envelope, whatever its prefix claims.
        let signing = fixed_signing();
        let bytes = envelope_at_doc_schema(1, &signing);
        let mut value: ciborium::value::Value =
            ciborium::de::from_reader(&bytes[MAGIC_LEN..]).unwrap();
        if let ciborium::value::Value::Map(m) = &mut value {
            m.retain(
                |(k, _)| !matches!(k, ciborium::value::Value::Integer(i) if i128::from(*i) == 12),
            );
        }
        let mut out = bytes[..MAGIC_LEN].to_vec();
        ciborium::ser::into_writer(&value, &mut out).unwrap();
        assert!(matches!(
            decode_envelope(&out),
            Err(OpEnvelopeError::BadField("doc_schema_v"))
        ));
    }

    fn contains_subseq(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || needle.len() > haystack.len() {
            return false;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
