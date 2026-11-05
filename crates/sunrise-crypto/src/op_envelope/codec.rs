//! The envelope's canonical CBOR ground form and the `ciborium::Value` field
//! readers [`super::decode_envelope`] is built on.
//!
//! We build the canonical map explicitly, in ascending field-id order, so the
//! encoded byte sequence matches the spec regardless of the surrounding Rust
//! struct. Field ids are non-negative integers, so RFC 8949 deterministic key
//! ordering is numeric ordering (1..=12 all encode as a single byte
//! 0x01..=0x0c).
//!
//! The three encodings the format needs differ ONLY in which fields they omit,
//! so they are one function. Defining them by exclusion — rather than as
//! "fields 1..=9" and "fields 1..=10" — is what makes a newly added field land
//! inside the AAD and the signature automatically instead of silently escaping
//! both.

use super::{ChainHead, OpEnvelope, OpEnvelopeError, MAX_CHAIN_HEADS, SIG_DOMAIN};
use sunrise_cbor::hlc::Hlc;

/// Which fields to omit from a canonical encoding of the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Omit {
    /// Everything: the full on-the-wire encoding.
    Nothing,
    /// Field 11 (`sig`) — the signature input.
    Sig,
    /// Fields 10 (`payload`) and 11 (`sig`) — the AEAD associated data.
    PayloadAndSig,
}

impl Omit {
    pub(super) const fn omits(self, field_id: u8) -> bool {
        match self {
            Self::Nothing => false,
            Self::Sig => field_id == 11,
            Self::PayloadAndSig => field_id == 10 || field_id == 11,
        }
    }
}

/// Encode the envelope to canonical CBOR, omitting the fields `omit` names.
pub(super) fn encode_cbor(env: &OpEnvelope, omit: Omit) -> Result<Vec<u8>, OpEnvelopeError> {
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
    if let Some(fp) = env.schema_fp {
        put(13, Value::Bytes(fp.to_vec()));
    }
    if let Some(prev) = env.prev_hash {
        put(14, Value::Bytes(prev.to_vec()));
    }
    if !env.heads.is_empty() {
        put(
            15,
            Value::Array(
                env.heads
                    .iter()
                    .map(|h| {
                        Value::Array(vec![
                            Value::Bytes(h.device_id.to_vec()),
                            Value::Integer(h.seq.into()),
                            Value::Bytes(h.op_hash.to_vec()),
                        ])
                    })
                    .collect(),
            ),
        );
    }

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
pub(super) fn encode_aad(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    encode_cbor(env, Omit::PayloadAndSig)
}

/// Compute the signature input bytes for a given envelope state (without the
/// `sig` field).
pub(super) fn sig_input_bytes(env: &OpEnvelope) -> Result<Vec<u8>, OpEnvelopeError> {
    let cbor_no_sig = encode_cbor(env, Omit::Sig)?;
    let hash = blake3::hash(&cbor_no_sig);
    let mut out = Vec::with_capacity(SIG_DOMAIN.len() + hash.as_bytes().len());
    out.extend_from_slice(SIG_DOMAIN);
    out.extend_from_slice(hash.as_bytes());
    Ok(out)
}

/// Canonical Sunrise CBOR has no floats anywhere, so a float in an unknown
/// envelope field is malformed rather than merely unfamiliar.
pub(super) fn contains_float_value(v: &ciborium::value::Value) -> bool {
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

pub(super) fn u32_from(
    v: &ciborium::value::Value,
    field: &'static str,
) -> Result<u32, OpEnvelopeError> {
    match v {
        ciborium::value::Value::Integer(i) => {
            let raw: i128 = i128::from(*i);
            u32::try_from(raw).map_err(|_| OpEnvelopeError::BadField(field))
        }
        _ => Err(OpEnvelopeError::BadField(field)),
    }
}

pub(super) fn u64_from(
    v: &ciborium::value::Value,
    field: &'static str,
) -> Result<u64, OpEnvelopeError> {
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
pub(super) fn hlc_from(v: &ciborium::value::Value) -> Result<Hlc, OpEnvelopeError> {
    let arr = match v {
        ciborium::value::Value::Array(a) if a.len() == 2 => a,
        _ => return Err(OpEnvelopeError::BadField("hlc")),
    };
    Ok(Hlc {
        physical_ms: u64_from(&arr[0], "hlc.physical_ms")?,
        logical: u32_from(&arr[1], "hlc.logical")?,
    })
}

/// Field 15: a non-empty array of `[device_id, seq, op_hash]`. An empty array
/// is refused rather than read as "no heads": the writer omits the field in
/// that case, so re-emitting an empty one would change the signed bytes.
pub(super) fn heads_from(v: &ciborium::value::Value) -> Result<Vec<ChainHead>, OpEnvelopeError> {
    let ciborium::value::Value::Array(items) = v else {
        return Err(OpEnvelopeError::BadField("heads"));
    };
    if items.is_empty() || items.len() > MAX_CHAIN_HEADS {
        return Err(OpEnvelopeError::BadField("heads"));
    }
    items
        .iter()
        .map(|item| match item {
            ciborium::value::Value::Array(h) if h.len() == 3 => Ok(ChainHead {
                device_id: bytes16_from(&h[0], "heads")?,
                seq: u64_from(&h[1], "heads")?,
                op_hash: bytes_n_from(&h[2], "heads", 32)?,
            }),
            _ => Err(OpEnvelopeError::BadField("heads")),
        })
        .collect()
}

/// Field 15's shape rule, shared by the writer and the reader: at most
/// [`MAX_CHAIN_HEADS`] entries, strictly ascending by device id, so a device
/// appears once and the encoding of a set is unique.
pub(super) fn check_heads(heads: &[ChainHead]) -> Result<(), OpEnvelopeError> {
    if heads.len() > MAX_CHAIN_HEADS || heads.windows(2).any(|w| w[0].device_id >= w[1].device_id) {
        return Err(OpEnvelopeError::BadField("heads"));
    }
    Ok(())
}

pub(super) fn bytes16_from(
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

pub(super) fn bytes_n_from<const N: usize>(
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
