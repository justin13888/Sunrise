//! Cleartext routing header of an `OpEnvelope`, without its contents.
//!
//! A relay has to route and de-duplicate op envelopes it is forbidden to read.
//! `docs/06-server/overview.md` lists reading op *contents* as an explicit
//! non-responsibility, while `docs/03-crypto/data-encryption-format.md` puts
//! `stream_id` (field 2), `device_id` (field 3) and `seq` (field 4) in the
//! clear — they are covered by the signature and the AAD, so they are
//! authenticated, but they are not secret. Routing by them is by design; the
//! whole cursor mechanism in `docs/05-sync/` presumes a relay can tell which
//! device wrote which `seq`.
//!
//! This module is that capability and nothing more. It deliberately lives in
//! `sunrise-cbor` rather than in `sunrise-crypto`: a server that links only
//! this crate has no key material, no AEAD, and no way to open field 10 even
//! by mistake. [`EnvelopeHeader`] has no payload field, so the boundary the
//! docs describe is enforced by the type rather than by discipline.
//!
//! What it does check is the container format, by the floor rule of ADR-0045
//! §5 that [`envelope_format_readable`] states and the client decoder shares:
//! the 5-byte magic prefix must name [`MagicKind::OpEnvelope`] at a container
//! floor this build implements, and field 1 must be at least that floor. A
//! newer container that keeps this build's floor keeps fields 1–12 where they
//! were, so the routing ids are found and its new fields are walked past. A
//! floor this build does not implement means the field layout is unknown, so
//! the routing ids cannot be located either. The *document* schema (field 12)
//! is deliberately not consulted — the relay does not interpret the payload,
//! so a payload schema from the future is none of its business.

use crate::magic::{decode_prefix, MagicKind, MAGIC_LEN};
use crate::version::{ENVELOPE_FORMAT_FLOOR, ENVELOPE_FORMAT_V};
use sunrise_error::ErrorCode;
use thiserror::Error;

/// Whether this build implements the container floor an envelope's magic
/// prefix names: `ENVELOPE_FORMAT_FLOOR <= prefix_version <= ENVELOPE_FORMAT_V`.
///
/// The prefix carries the *writer's* [`ENVELOPE_FORMAT_FLOOR`], the oldest
/// container that still reads the envelope correctly (ADR-0045 §5). Above this
/// build's `ENVELOPE_FORMAT_V`, the writer has declared that a reader at this
/// version would misread it. Below this build's floor, it names a layout this
/// build no longer implements. Either way the bytes cannot be located.
///
/// This half of the rule needs no CBOR, so a decoder applies it before
/// parsing. [`envelope_format_readable`] is the whole rule.
#[must_use]
pub const fn envelope_floor_readable(prefix_version: u16) -> bool {
    ENVELOPE_FORMAT_FLOOR <= prefix_version && prefix_version <= ENVELOPE_FORMAT_V
}

/// The envelope-container acceptance rule of ADR-0045 §5, shared by the client
/// decoder (`sunrise_crypto::decode_envelope`) and the relay's
/// [`decode_envelope_header`].
///
/// `prefix_version` is the magic prefix's version word (the writer's floor)
/// and `v` is envelope field 1 (the writer's `ENVELOPE_FORMAT_V`). An envelope
/// is readable when [`envelope_floor_readable`] holds for its prefix and
/// `v >= prefix_version`: a writer's own container is never older than the
/// floor it claims. Field 1 has no upper bound — a newer container is the case
/// this rule exists to admit, and its unknown fields are preserved.
#[must_use]
pub const fn envelope_format_readable(prefix_version: u16, v: u64) -> bool {
    envelope_floor_readable(prefix_version) && v >= prefix_version as u64
}

/// Field id of `v`, the writer's container format, in the envelope CDDL.
const FIELD_V: i128 = 1;
/// Field id of `stream_id` in the envelope CDDL.
const FIELD_STREAM_ID: i128 = 2;
/// Field id of `device_id`.
const FIELD_DEVICE_ID: i128 = 3;
/// Field id of `seq`.
const FIELD_SEQ: i128 = 4;

/// The cleartext routing ids of one op envelope.
///
/// Intentionally not a subset-of-`OpEnvelope` struct: there is no payload, no
/// signature and no nonce here, because a consumer of this type is one that
/// must not have them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnvelopeHeader {
    /// 16-byte stream id the op belongs to (`0x00..00` = vault-meta).
    pub stream_id: [u8; 16],
    /// 16-byte id of the device that signed the op.
    pub device_id: [u8; 16],
    /// The signing device's per-`(stream_id, device_id)` counter; first op = 1.
    pub seq: u64,
}

/// Why a routing header could not be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnvelopeHeaderError {
    /// Missing / wrong magic prefix, or a container floor this build does not
    /// implement.
    #[error(
        "not an op envelope with a container floor in \
         {ENVELOPE_FORMAT_FLOOR}..={ENVELOPE_FORMAT_V}"
    )]
    BadMagic,
    /// The CBOR after the prefix was malformed or not a map.
    #[error("envelope CBOR malformed: {0}")]
    Cbor(String),
    /// A routing field was absent or of the wrong shape.
    #[error("envelope routing field invalid or missing: {0}")]
    BadField(&'static str),
}

impl EnvelopeHeaderError {
    /// Map to a canonical [`ErrorCode`].
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::BadMagic => ErrorCode::ProtocolBadMagic,
            Self::Cbor(_) => ErrorCode::CryptoNonCanonicalCbor,
            Self::BadField(_) => ErrorCode::SyncOpInvalid,
        }
    }
}

/// Read `(stream_id, device_id, seq)` out of an encoded op envelope.
///
/// Verifies nothing cryptographically — a relay holds no key that could — so a
/// caller must treat the result as *routing metadata asserted by the sender*,
/// not as authenticated fact. It is safe in that role because every consequence
/// of getting it wrong (a frame skipped, a frame replayed) is bounded by the
/// receiving client re-checking the signature before it applies anything.
///
/// # Errors
/// [`EnvelopeHeaderError`] when the prefix, the CBOR, or a routing field is not
/// what the container format requires.
pub fn decode_envelope_header(bytes: &[u8]) -> Result<EnvelopeHeader, EnvelopeHeaderError> {
    if bytes.len() < MAGIC_LEN {
        return Err(EnvelopeHeaderError::BadMagic);
    }
    let prefix = decode_prefix(&bytes[..MAGIC_LEN]).map_err(|_| EnvelopeHeaderError::BadMagic)?;
    if prefix.kind != MagicKind::OpEnvelope || !envelope_floor_readable(prefix.version) {
        return Err(EnvelopeHeaderError::BadMagic);
    }

    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[MAGIC_LEN..])
        .map_err(|e| EnvelopeHeaderError::Cbor(e.to_string()))?;
    let ciborium::value::Value::Map(entries) = value else {
        return Err(EnvelopeHeaderError::Cbor("envelope must be a map".into()));
    };

    let mut format_v = None;
    let mut stream_id = None;
    let mut device_id = None;
    let mut seq = None;
    for (k, v) in &entries {
        let ciborium::value::Value::Integer(i) = k else {
            return Err(EnvelopeHeaderError::Cbor("non-integer map key".into()));
        };
        // Only the container version and the three routing fields are looked
        // at. Field 10 (the ciphertext) is walked past without being copied
        // out, which is the point of this function existing at all.
        match i128::from(*i) {
            FIELD_V => format_v = Some(u64_of(v, "v")?),
            FIELD_STREAM_ID => stream_id = Some(bytes16(v, "stream_id")?),
            FIELD_DEVICE_ID => device_id = Some(bytes16(v, "device_id")?),
            FIELD_SEQ => seq = Some(u64_of(v, "seq")?),
            _ => {}
        }
    }

    let format_v = format_v.ok_or(EnvelopeHeaderError::BadField("v"))?;
    if !envelope_format_readable(prefix.version, format_v) {
        return Err(EnvelopeHeaderError::BadField("v"));
    }

    Ok(EnvelopeHeader {
        stream_id: stream_id.ok_or(EnvelopeHeaderError::BadField("stream_id"))?,
        device_id: device_id.ok_or(EnvelopeHeaderError::BadField("device_id"))?,
        seq: seq.ok_or(EnvelopeHeaderError::BadField("seq"))?,
    })
}

fn bytes16(
    v: &ciborium::value::Value,
    field: &'static str,
) -> Result<[u8; 16], EnvelopeHeaderError> {
    let ciborium::value::Value::Bytes(b) = v else {
        return Err(EnvelopeHeaderError::BadField(field));
    };
    let arr: [u8; 16] = b
        .as_slice()
        .try_into()
        .map_err(|_| EnvelopeHeaderError::BadField(field))?;
    Ok(arr)
}

fn u64_of(v: &ciborium::value::Value, field: &'static str) -> Result<u64, EnvelopeHeaderError> {
    let ciborium::value::Value::Integer(i) = v else {
        return Err(EnvelopeHeaderError::BadField(field));
    };
    u64::try_from(*i).map_err(|_| EnvelopeHeaderError::BadField(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::magic::write_prefix;
    use ciborium::value::{Integer, Value};

    /// Build an envelope-shaped CBOR map with the given routing ids plus a
    /// stand-in for every other field, prefixed as an op envelope at this
    /// build's own container (prefix and field 1 both this build's).
    fn envelope_bytes(stream: [u8; 16], device: [u8; 16], seq: u64) -> Vec<u8> {
        envelope_at(
            stream,
            device,
            seq,
            ENVELOPE_FORMAT_FLOOR,
            ENVELOPE_FORMAT_V,
            Vec::new(),
        )
    }

    /// [`envelope_bytes`] with the prefix (the writer's floor), field 1 (the
    /// writer's container) and any further fields chosen by the caller.
    fn envelope_at(
        stream: [u8; 16],
        device: [u8; 16],
        seq: u64,
        prefix: u16,
        v: u16,
        extra: Vec<(Value, Value)>,
    ) -> Vec<u8> {
        let mut entries: Vec<(Value, Value)> = vec![
            (Value::Integer(1.into()), Value::Integer(v.into())),
            (Value::Integer(2.into()), Value::Bytes(stream.to_vec())),
            (Value::Integer(3.into()), Value::Bytes(device.to_vec())),
            (Value::Integer(4.into()), Value::Integer(seq.into())),
            (
                Value::Integer(5.into()),
                Value::Array(vec![
                    Value::Integer(Integer::from(1u32)),
                    Value::Integer(Integer::from(0u32)),
                ]),
            ),
            (Value::Integer(10.into()), Value::Bytes(vec![0xAA; 64])),
            (Value::Integer(11.into()), Value::Bytes(vec![0x11; 64])),
            (Value::Integer(12.into()), Value::Integer(3.into())),
        ];
        entries.extend(extra);
        entries.sort_by_key(|(k, _)| match k {
            Value::Integer(i) => i128::from(*i),
            _ => unreachable!("integer keys only"),
        });
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::OpEnvelope, prefix);
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        out
    }

    #[test]
    fn reads_the_three_routing_fields() {
        let bytes = envelope_bytes([7u8; 16], [9u8; 16], 42);
        let h = decode_envelope_header(&bytes).unwrap();
        assert_eq!(h.stream_id, [7u8; 16]);
        assert_eq!(h.device_id, [9u8; 16]);
        assert_eq!(h.seq, 42);
    }

    /// ADR-0045 §5: a container one step newer that keeps this build's floor
    /// is routed. Its new field is walked past like any other non-routing
    /// field, because the floor promises fields 1–12 kept their meaning.
    #[test]
    fn a_newer_container_at_this_floor_is_routed() {
        let bytes = envelope_at(
            [7u8; 16],
            [9u8; 16],
            42,
            ENVELOPE_FORMAT_FLOOR,
            ENVELOPE_FORMAT_V + 1,
            vec![(Value::Integer(23.into()), Value::Text("future".into()))],
        );
        let h = decode_envelope_header(&bytes).expect("a newer container is routed");
        assert_eq!(h.stream_id, [7u8; 16]);
        assert_eq!(h.device_id, [9u8; 16]);
        assert_eq!(h.seq, 42);
    }

    /// A writer whose floor is above this build's container has declared that
    /// this build would misread it: the field layout is exactly what changed,
    /// so the routing ids may not be at ids 2/3/4 at all. It is refused rather
    /// than parsed hopefully, and so is a floor below the oldest container
    /// this build implements.
    #[test]
    fn a_floor_this_build_does_not_implement_is_refused() {
        for prefix in [ENVELOPE_FORMAT_V + 1, ENVELOPE_FORMAT_FLOOR - 1] {
            let bytes = envelope_at(
                [1u8; 16],
                [2u8; 16],
                1,
                prefix,
                prefix.max(ENVELOPE_FORMAT_V),
                Vec::new(),
            );
            assert_eq!(
                decode_envelope_header(&bytes),
                Err(EnvelopeHeaderError::BadMagic),
                "prefix {prefix}"
            );
        }
    }

    /// Field 1 is the writer's own container, which cannot be older than the
    /// floor the same writer stamped in the prefix.
    #[test]
    fn a_container_below_its_own_floor_is_refused() {
        let bytes = envelope_at(
            [1u8; 16],
            [2u8; 16],
            1,
            ENVELOPE_FORMAT_FLOOR,
            ENVELOPE_FORMAT_FLOOR - 1,
            Vec::new(),
        );
        assert_eq!(
            decode_envelope_header(&bytes),
            Err(EnvelopeHeaderError::BadField("v"))
        );
    }

    /// The rule is one function, so the relay and the client cannot drift.
    #[test]
    fn the_floor_rule_admits_exactly_the_documented_window() {
        let f = ENVELOPE_FORMAT_FLOOR;
        let v = u64::from(ENVELOPE_FORMAT_V);
        assert!(envelope_format_readable(f, u64::from(f)));
        assert!(envelope_format_readable(f, v + 1));
        assert!(envelope_format_readable(ENVELOPE_FORMAT_V, v));
        assert!(!envelope_format_readable(f, u64::from(f) - 1));
        assert!(!envelope_format_readable(ENVELOPE_FORMAT_V + 1, v + 1));
        assert!(!envelope_format_readable(f - 1, v));
    }

    #[test]
    fn a_frame_prefix_is_not_an_envelope_prefix() {
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::Frame, 1);
        out.push(0xA0); // empty CBOR map
        assert_eq!(
            decode_envelope_header(&out),
            Err(EnvelopeHeaderError::BadMagic)
        );
    }

    #[test]
    fn truncated_input_is_bad_magic_not_a_panic() {
        assert_eq!(
            decode_envelope_header(&[]),
            Err(EnvelopeHeaderError::BadMagic)
        );
        assert_eq!(
            decode_envelope_header(b"SR"),
            Err(EnvelopeHeaderError::BadMagic)
        );
    }

    #[test]
    fn a_missing_routing_field_is_an_error_not_a_zero() {
        let entries: Vec<(Value, Value)> = vec![
            (
                Value::Integer(1.into()),
                Value::Integer(ENVELOPE_FORMAT_V.into()),
            ),
            (Value::Integer(2.into()), Value::Bytes(vec![3u8; 16])),
            (Value::Integer(4.into()), Value::Integer(1.into())),
        ];
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        assert_eq!(
            decode_envelope_header(&out),
            Err(EnvelopeHeaderError::BadField("device_id"))
        );
    }

    /// Without field 1 the floor rule has nothing to compare the prefix
    /// against, so the container is unknown rather than assumed.
    #[test]
    fn a_missing_container_version_is_refused() {
        let entries: Vec<(Value, Value)> = vec![
            (Value::Integer(2.into()), Value::Bytes(vec![3u8; 16])),
            (Value::Integer(3.into()), Value::Bytes(vec![4u8; 16])),
            (Value::Integer(4.into()), Value::Integer(1.into())),
        ];
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        assert_eq!(
            decode_envelope_header(&out),
            Err(EnvelopeHeaderError::BadField("v"))
        );
    }

    #[test]
    fn a_wrong_width_id_is_rejected() {
        let entries: Vec<(Value, Value)> = vec![
            (
                Value::Integer(1.into()),
                Value::Integer(ENVELOPE_FORMAT_V.into()),
            ),
            (Value::Integer(2.into()), Value::Bytes(vec![3u8; 15])),
            (Value::Integer(3.into()), Value::Bytes(vec![4u8; 16])),
            (Value::Integer(4.into()), Value::Integer(1.into())),
        ];
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        assert_eq!(
            decode_envelope_header(&out),
            Err(EnvelopeHeaderError::BadField("stream_id"))
        );
    }

    /// Unknown field ids from a newer document schema ride along untouched:
    /// they are not routing fields, so they are skipped, not refused.
    #[test]
    fn unknown_fields_do_not_prevent_routing() {
        let entries: Vec<(Value, Value)> = vec![
            (
                Value::Integer(1.into()),
                Value::Integer(ENVELOPE_FORMAT_V.into()),
            ),
            (Value::Integer(2.into()), Value::Bytes(vec![3u8; 16])),
            (Value::Integer(3.into()), Value::Bytes(vec![4u8; 16])),
            (Value::Integer(4.into()), Value::Integer(5.into())),
            (Value::Integer(77.into()), Value::Text("future".into())),
        ];
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        let h = decode_envelope_header(&out).unwrap();
        assert_eq!(h.seq, 5);
    }
}
