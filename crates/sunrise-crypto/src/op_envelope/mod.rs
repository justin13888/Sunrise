//! `OpEnvelope` — byte-exact CBOR codec, sign, encrypt, verify, decrypt.
//!
//! Per `docs/03-crypto/data-encryption-format.md`:
//!
//! ```cddl
//! OpEnvelope = {
//!     1: uint,            ; v             (writer's ENVELOPE_FORMAT_V, currently 3)
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
//!     ? 13: bstr .size 8, ; schema_fp     (first 8 bytes of fingerprint(doc_schema_v))
//!     ? 14: bstr .size 32,; prev_hash     (op_hash of this device's op at seq - 1)
//!     ? 15: [+ head],     ; heads         (other devices' tips seen since the last op)
//!     * uint => any       ; later fields, preserved verbatim
//! }
//! head = [bstr .size 16, uint, bstr .size 32]   ; [device_id, seq, op_hash]
//! ```
//!
//! Fields 14 and 15 chain each device's ops and name what its writer had
//! seen (ADR-0043 §1–§2, issue #325). Both are optional and additive under
//! ADR-0045 §5: an envelope without them is a *legacy link* that asserts
//! nothing about its predecessor, every build before them keeps them verbatim
//! as unknowns, and they sit inside the AAD and the signature by the same
//! exclusion rule as field 13. [`op_hash`] is the hash both fields carry.
//!
//! Field 13 binds the document schema's *identity* to its number (ADR-0045
//! §3, issue #323). Field 12 says which version the payload was written
//! against; field 13 carries the first 8 bytes of that version's registered
//! fingerprint (`sunrise_cbor::version::DOC_SCHEMA_FINGERPRINTS`), so two
//! builds that disagree about what a version means can tell. A writer at
//! `doc_schema_v >= DOC_SCHEMA_FP_FIRST` MUST emit it; [`encode_envelope`]
//! does. An envelope below that version has none and is read under the legacy
//! rules. Adding it is not a container change: every build already preserves
//! an unknown field 13 and covers it with the AAD and the signature.
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
//! carries the writer's `ENVELOPE_FORMAT_FLOOR`, and field 1 the writer's
//! `ENVELOPE_FORMAT_V` (ADR-0045 §5). A reader accepts a container whose floor
//! it implements and whose field 1 is at least that floor
//! (`sunrise_cbor::envelope_format_readable`), and keeps every field it does
//! not know in [`OpEnvelope::unknown`].
//!
//! AAD (when `aead_alg = 1`) = canonical CBOR of the same map with fields 10
//! and 11 removed — i.e. `{1..9, 12, 13, ...}`.
//!
//! Signature input = `"sunrise.op_envelope.v1" || BLAKE3(canonical_cbor_without_field_11, 32)`
//! — i.e. over `{1..10, 12, 13, ...}`.
//!
//! This file holds the types and the sign, seal, decode, verify and open API.
//! The canonical encoding the AAD, the signature input and [`op_hash`] are
//! built from, and the field readers [`decode_envelope`] uses, are in `codec`.

use crate::aead::{aead_open_xchacha, AEAD_NONCE_LEN};
use crate::keys::{verify_ed25519, DeviceSigningKeyPair, StreamKey};
use crate::suite::{aead_alg_id, sig_alg_id, AeadAlgId, SigAlgId};
use codec::{
    bytes16_from, bytes_n_from, check_heads, contains_float_value, encode_aad, encode_cbor,
    heads_from, hlc_from, sig_input_bytes, u32_from, u64_from, Omit,
};
use std::collections::BTreeMap;
use sunrise_cbor::envelope_header::{envelope_floor_readable, envelope_format_readable};
use sunrise_cbor::hlc::Hlc;
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MAGIC_LEN};
use sunrise_cbor::version::{
    doc_schema_fp_prefix, DOC_SCHEMA_FLOOR, DOC_SCHEMA_FP_PREFIX_LEN, DOC_SCHEMA_V,
    ENVELOPE_FORMAT_FLOOR, ENVELOPE_FORMAT_V,
};
use sunrise_error::ErrorCode;
use thiserror::Error;

mod codec;

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
    /// Envelope **container format** version: the writer's
    /// `ENVELOPE_FORMAT_V`, which may be newer than this build's.
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
    /// Field 13: the first 8 bytes of the registered fingerprint of
    /// [`OpEnvelope::doc_schema_v`] (ADR-0045 §3), or `None` when the envelope
    /// carries no field 13, as every envelope below
    /// `sunrise_cbor::DOC_SCHEMA_FP_FIRST` does.
    ///
    /// The decoder takes it as written and does not compare it with this
    /// build's registry: whether a mismatch is parked is the receiver's
    /// decision, and the bytes have to survive a hop through this build either
    /// way, because the signature covers them.
    pub schema_fp: Option<[u8; DOC_SCHEMA_FP_PREFIX_LEN]>,
    /// Field 14: the [`op_hash`] of the same device's op at
    /// `(stream_id, seq - 1)` (ADR-0043 §1), or `None` for a *legacy link*:
    /// the first op of a sequence, an op written before chaining existed, or
    /// an op whose writer could not find its own predecessor.
    pub prev_hash: Option<[u8; 32]>,
    /// Field 15: for each other device whose contiguous prefix in this stream
    /// advanced since the writer's previous op here, the tip of that prefix
    /// (ADR-0043 §2). Sorted by device id, each device at most once, and at
    /// most [`MAX_CHAIN_HEADS`] long. Empty means the field is absent.
    pub heads: Vec<ChainHead>,
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

/// One entry of envelope field 15: the tip of another device's contiguous
/// prefix in the same stream, as the writer held it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChainHead {
    /// The device whose prefix this is.
    pub device_id: [u8; 16],
    /// The last seq of that prefix.
    pub seq: u64,
    /// [`op_hash`] of that device's op at `seq`.
    pub op_hash: [u8; 32],
}

/// The most entries field 15 may carry. A writer lists one per other device
/// writing to the stream, so a real list is a handful long; the bound is what
/// stops a malformed envelope from making a receiver allocate without limit.
pub const MAX_CHAIN_HEADS: usize = 256;

/// The chain fields a writer stamps into an envelope (fields 14 and 15).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainLinks {
    /// Field 14. See [`OpEnvelope::prev_hash`].
    pub prev_hash: Option<[u8; 32]>,
    /// Field 15. See [`OpEnvelope::heads`].
    pub heads: Vec<ChainHead>,
}

/// The one hash of an op in the system (ADR-0043 §1): BLAKE3 over the
/// envelope's full canonical CBOR, **including** field 11, the signature, and
/// excluding the 5-byte magic prefix.
///
/// It is computed from a re-encoding rather than from the bytes a peer sent,
/// so two replicas holding the same envelope agree on its hash whatever
/// encoding reached them. It is the `env_hash` of
/// `docs/03-crypto/audit-and-tamper-evidence.md`.
///
/// # Errors
/// CBOR encode failure (essentially impossible).
pub fn op_hash(env: &OpEnvelope) -> Result<[u8; 32], OpEnvelopeError> {
    let cbor = encode_cbor(env, Omit::Nothing)?;
    Ok(*blake3::hash(&cbor).as_bytes())
}

/// Sign the envelope in place. Sets `env.sig` from the device signing key.
///
/// Caller is responsible for setting all other fields before calling.
///
/// # Errors
/// CBOR encode failure (essentially impossible for v1 fields).
pub fn sign_envelope(
    env: &mut OpEnvelope,
    device_signing: &DeviceSigningKeyPair,
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
    device_signing: &DeviceSigningKeyPair,
) -> Result<Vec<u8>, OpEnvelopeError> {
    encode_chained_envelope(
        inner,
        stream_id,
        device_id,
        seq,
        hlc,
        aead_alg,
        epoch,
        nonce,
        ChainLinks::default(),
        stream_key,
        device_signing,
    )
}

/// [`encode_envelope`], stamping the chain fields `chain` carries (fields 14
/// and 15, ADR-0043). An empty `chain` writes neither, which is exactly what
/// [`encode_envelope`] produces.
///
/// # Errors
/// CBOR/AEAD failures.
#[allow(clippy::too_many_arguments)]
pub fn encode_chained_envelope(
    inner: &[u8],
    stream_id: [u8; 16],
    device_id: [u8; 16],
    seq: u64,
    hlc: Hlc,
    aead_alg: AeadAlgId,
    epoch: u32,
    nonce: [u8; AEAD_NONCE_LEN],
    chain: ChainLinks,
    stream_key: Option<&StreamKey>,
    device_signing: &DeviceSigningKeyPair,
) -> Result<Vec<u8>, OpEnvelopeError> {
    let env = OpEnvelope {
        prev_hash: chain.prev_hash,
        heads: chain.heads,
        v: u32::from(ENVELOPE_FORMAT_V),
        doc_schema_v: u32::from(DOC_SCHEMA_V),
        // Field 13 is required from the first fingerprinted version on, and
        // this build's own version is always registered: a test in
        // `sunrise-core` fails otherwise.
        schema_fp: doc_schema_fp_prefix(u32::from(DOC_SCHEMA_V)),
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
    device_signing: &DeviceSigningKeyPair,
) -> Result<Vec<u8>, OpEnvelopeError> {
    if env.aead_alg == AeadAlgId::None && env.epoch != 0 {
        return Err(OpEnvelopeError::InconsistentAead);
    }
    if env.aead_alg == AeadAlgId::XChaCha20Poly1305 && stream_key.is_none() {
        return Err(OpEnvelopeError::InconsistentAead);
    }
    // The decoder refuses a field 15 out of shape, so the writer must not
    // emit one: an envelope nobody can read is worse than a refused write.
    check_heads(&env.heads)?;

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
    // The prefix names the writer's floor, not its container (ADR-0045 §5):
    // the oldest container that still reads these bytes correctly.
    write_prefix(&mut prefix, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
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
    // The prefix carries the writer's CONTAINER FLOOR (ADR-0045 §5). A floor
    // this build does not implement is the one thing a decoder cannot work
    // around — it does not know the field layout, so it cannot even find the
    // payload. A newer container at a floor this build implements keeps fields
    // 1–12 where they were, and its new fields land in `unknown` below. The
    // relay's header decoder applies the same rule through the same function.
    // The document schema is field 12 and is checked further down against its
    // own floor.
    if prefix.kind != MagicKind::OpEnvelope || !envelope_floor_readable(prefix.version) {
        return Err(OpEnvelopeError::BadMagic);
    }
    let cbor_bytes = &bytes[MAGIC_LEN..];
    // Nesting is bounded (`sunrise_cbor::MAX_VALUE_DEPTH`): an unknown field
    // is kept and re-encoded whole, so its depth is this decoder's to limit.
    let value = sunrise_cbor::decode_value(&mut &cbor_bytes[..])
        .map_err(|e| OpEnvelopeError::Cbor(e.to_string()))?;
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        _ => return Err(OpEnvelopeError::Cbor("envelope must be a map".into())),
    };

    let mut env = OpEnvelope {
        v: 0,
        doc_schema_v: 0,
        schema_fp: None,
        prev_hash: None,
        heads: Vec::new(),
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
            // Optional, and taken as written: comparing it with the registry
            // is the receiver's job (ADR-0045 §3), not the codec's.
            13 => {
                env.schema_fp = Some(bytes_n_from(&v, "schema_fp", DOC_SCHEMA_FP_PREFIX_LEN)?);
            }
            // Taken as written, like field 13: whether a link holds is a
            // question about the receiver's op log, not about these bytes.
            14 => env.prev_hash = Some(bytes_n_from(&v, "prev_hash", 32)?),
            15 => {
                env.heads = heads_from(&v)?;
                check_heads(&env.heads)?;
            }
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

    // Field 1 is the writer's own container, which may be newer than this
    // build's but never older than the floor the same writer stamped.
    if !envelope_format_readable(prefix.version, u64::from(env.v)) {
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

/// Decrypt an envelope's payload **without checking the sender's signature**.
///
/// There is exactly one legitimate caller, and it exists because of a genuine
/// ordering problem rather than a convenience: a device publishing its own
/// certificate for the first time is not yet in the receiver's device table, so
/// the receiver has no `D_S_pub` to verify against until it has read the cert
/// out of the payload. Every other caller must use [`open_envelope`].
///
/// What this still guarantees is not nothing. The payload is authenticated by
/// the AEAD tag over an AAD that covers every envelope field, so a caller
/// reaching this function knows the bytes were sealed under a Stream key it
/// holds and have not been altered — it just does not yet know *which member*
/// of the account sealed them. The caller must call [`verify_envelope`] with
/// the key it recovers before acting on the contents; the doc comment on
/// `Engine::self_authenticating_signer` states both checks and why neither
/// alone is enough.
///
/// # Errors
/// AEAD auth failure, or [`OpEnvelopeError::InconsistentAead`] for a
/// signed-only envelope, which has no ciphertext and so no reason to be here.
pub fn open_envelope_unverified(
    env: &OpEnvelope,
    stream_key: &StreamKey,
) -> Result<Vec<u8>, OpEnvelopeError> {
    match env.aead_alg {
        AeadAlgId::None => Err(OpEnvelopeError::InconsistentAead),
        AeadAlgId::XChaCha20Poly1305 => {
            let aad = encode_aad(env)?;
            aead_open_xchacha(stream_key.as_bytes(), &env.nonce, &env.payload, &aad)
                .map_err(|_| OpEnvelopeError::AeadAuth)
        }
    }
}

#[cfg(test)]
mod tests;
