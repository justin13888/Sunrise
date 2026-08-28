//! Typed CBOR payloads for the sync message kinds beyond the handshake.
//!
//! Per `docs/05-sync/wire-protocol.md`, each `msg_kind` carries a canonical
//! CBOR payload (the frame layer applies the magic prefix + header; the
//! payload itself is bare CBOR, exactly like [`crate::Hello`] /
//! [`crate::HelloAck`]). These structs are the typed shapes for:
//!
//! - [`OpBatchPayload`]  — `MsgKind::OpBatch`  (`0x03`)
//! - [`AckPayload`]      — `MsgKind::Ack`      (`0x04`)
//! - [`NackPayload`]     — `MsgKind::Nack`     (`0x05`)
//! - [`SubscribePayload`]— `MsgKind::Subscribe`(`0x06`)
//! - [`CaughtUpPayload`] — `MsgKind::StreamUpdate` (`0x07`), see below
//! - [`ErrorPayload`]    — `MsgKind::Error`    (`0x0E`)
//!
//! # CaughtUp msg_kind
//!
//! The v1 kind space is fully allocated (15 kinds, `0x01..=0x0F`) with no
//! reserved slot, so CaughtUp cannot claim a fresh discriminator without
//! renumbering. It reuses `MsgKind::StreamUpdate` (`0x07`), whose doc role is
//! "server → client stream update": a CaughtUp is precisely a control update
//! on a stream. There is no collision with live-op fan-out because the v1
//! relay forwards live `OpBatch` frames *verbatim* (they keep `msg_kind =
//! OpBatch`), leaving `StreamUpdate` otherwise unused on the wire.
//!
//! # Encoding
//!
//! All payloads use canonical CBOR via [`sunrise_cbor`] (RFC 8949 §4.2 core
//! deterministic encoding). Struct fields are declared in canonical map-key
//! order (text keys sorted by encoded length, then bytewise) so an
//! independent canonical encoder reproduces the same bytes. 16-byte ids are
//! encoded as compact CBOR byte strings via `serde_bytes`, matching the
//! `sunrise-domain` convention. Opaque op-envelope blobs likewise encode as
//! byte strings. `decode_*` uses [`sunrise_cbor::decode_canonical`], which
//! rejects non-canonical input and trailing garbage.

use serde::{Deserialize, Serialize};
use sunrise_cbor::{decode_canonical, encode_canonical, CanonicalError};
use sunrise_error::ErrorCode;

/// Serialize `Vec<Vec<u8>>` as a CBOR array of byte strings (not arrays of
/// integers), so opaque op blobs stay compact and canonical.
mod ops_as_bstr {
    use serde::de::Deserialize as _;
    use serde::ser::SerializeSeq;
    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(ops: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(ops.len()))?;
        for op in ops {
            seq.serialize_element(serde_bytes::Bytes::new(op))?;
        }
        seq.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        let v: Vec<serde_bytes::ByteBuf> = Vec::deserialize(d)?;
        Ok(v.into_iter().map(serde_bytes::ByteBuf::into_vec).collect())
    }
}

/// `MsgKind::OpBatch` payload: a batch of opaque op envelopes on one stream.
///
/// Fields are ordered for canonical CBOR: `ops` (3), `batch_id` (8),
/// `stream_id` (9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpBatchPayload {
    /// Opaque, already-encoded `OpEnvelope` bytes. The server never inspects
    /// their contents; each element rides the wire as a CBOR byte string.
    #[serde(with = "ops_as_bstr")]
    pub ops: Vec<Vec<u8>>,
    /// Client-generated idempotency key for the batch (ULID low-bits / seq).
    pub batch_id: u64,
    /// 16-byte stream id the batch targets.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
}

/// `MsgKind::Ack` payload: positive ack for a published batch.
///
/// Fields are ordered for canonical CBOR: `batch_id` (8), `stream_id` (9),
/// `server_first_seen_ms` (20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckPayload {
    /// Echoes the acked batch's `batch_id`.
    pub batch_id: u64,
    /// 16-byte stream id the batch targeted.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
    /// Server wall-clock (ms) at which the batch was first seen; the
    /// out-of-band annotation used for clock-skew clamping.
    pub server_first_seen_ms: u64,
}

/// `MsgKind::Nack` payload: rejection for a batch the server declined.
///
/// Fields are ordered for canonical CBOR (key length then bytewise):
/// `code` (4) < `reason` (6) < `batch_id` (8) < `stream_id` (9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NackPayload {
    /// Canonical error code describing the rejection.
    pub code: ErrorCode,
    /// Human-readable reason (diagnostic; not load-bearing).
    pub reason: String,
    /// The rejected batch's `batch_id` (0 if it could not be parsed).
    pub batch_id: u64,
    /// The rejected batch's stream id (zeros if it could not be parsed).
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
}

/// One `(device_id, last_applied_seq)` cursor within a subscribe entry.
///
/// Fields are ordered for canonical CBOR: `device_id` (9),
/// `last_applied_seq` (16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorEntry {
    /// 16-byte originating device id this cursor tracks.
    #[serde(with = "serde_bytes")]
    pub device_id: [u8; 16],
    /// Highest `seq` the subscriber has already applied from that device.
    pub last_applied_seq: u64,
}

/// One stream the client wants to subscribe to, with its per-device cursors.
///
/// Fields are ordered for canonical CBOR: `cursors` (7), `stream_id` (9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribeEntry {
    /// Per-device cursors. v1 relay replays everything retained, so cursors
    /// are carried but may be unused server-side.
    pub cursors: Vec<CursorEntry>,
    /// 16-byte stream id to subscribe to.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
}

/// `MsgKind::Subscribe` payload: the set of streams (with cursors) to join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscribePayload {
    /// Streams to subscribe to.
    pub streams: Vec<SubscribeEntry>,
}

/// CaughtUp marker payload, sent over `MsgKind::StreamUpdate` after the
/// server finishes replaying retained frames for a subscribed stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaughtUpPayload {
    /// The stream whose retained backlog has been fully replayed.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
}

/// `MsgKind::Error` payload: a coded, recoverable-or-terminal error.
///
/// Fields are ordered for canonical CBOR: `code` (4), `reason` (6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    /// Canonical error code.
    pub code: ErrorCode,
    /// Human-readable reason (diagnostic).
    pub reason: String,
}

/// `MsgKind::Close` payload: `{ reason: tstr, code: tstr }` per
/// `docs/05-sync/wire-protocol.md`.
///
/// The code is a canonical [`ErrorCode`], so the reason a session ended is a
/// value the client can branch on rather than a string it has to match. That
/// matters for exactly one case today:
/// [`ClosePayload::auth_token_expired`] — a close a client should answer by
/// refreshing and reconnecting, NOT by prompting the user to sign in again.
/// Without a distinguishable code the two are indistinguishable, and every
/// expiry looks like a revocation.
///
/// Fields are ordered for canonical CBOR: `code` (4), `reason` (6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosePayload {
    /// Canonical error code naming why the session ended.
    pub code: ErrorCode,
    /// Human-readable reason (diagnostic).
    pub reason: String,
}

impl ClosePayload {
    /// The close a server sends when a session's bearer token has expired and
    /// the client did not refresh it in time.
    ///
    /// Recoverable, unlike [`ErrorCode::AuthTokenInvalid`] or
    /// [`ErrorCode::AuthDeviceRevoked`]: the credential aged out, nothing was
    /// withdrawn. A client that sees this refreshes and reconnects; a client
    /// that sees the other two stops and asks the user.
    ///
    /// The behaviour behind this — expiry tracking and the `RefreshToken`
    /// exchange — is wired separately (issue #7); this is the protocol surface.
    #[must_use]
    pub fn auth_token_expired() -> Self {
        Self {
            code: ErrorCode::AuthTokenExpired,
            reason: "bearer token expired; refresh and reconnect".to_string(),
        }
    }

    /// Whether this close is one the client should recover from on its own.
    #[must_use]
    pub const fn is_recoverable(&self) -> bool {
        matches!(self.code, ErrorCode::AuthTokenExpired)
    }
}

macro_rules! canonical_codec {
    ($ty:ty) => {
        impl $ty {
            /// Encode to canonical CBOR bytes (bare payload, no frame header).
            ///
            /// # Errors
            /// Returns [`CanonicalError`] if CBOR serialization fails.
            pub fn encode(&self) -> Result<Vec<u8>, CanonicalError> {
                encode_canonical(self)
            }

            /// Decode from canonical CBOR bytes, rejecting non-canonical
            /// input and trailing garbage.
            ///
            /// # Errors
            /// Returns [`CanonicalError`] on malformed or non-canonical input.
            pub fn decode(bytes: &[u8]) -> Result<Self, CanonicalError> {
                decode_canonical(bytes)
            }
        }
    };
}

canonical_codec!(OpBatchPayload);
canonical_codec!(AckPayload);
canonical_codec!(NackPayload);
canonical_codec!(SubscribePayload);
canonical_codec!(CaughtUpPayload);
canonical_codec!(ErrorPayload);
canonical_codec!(ClosePayload);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_round_trip() {
        let c = ClosePayload::auth_token_expired();
        let bytes = c.encode().unwrap();
        assert_eq!(ClosePayload::decode(&bytes).unwrap(), c);
    }

    /// An expiry is recoverable; a revocation is not. A client that cannot
    /// tell them apart either re-prompts the user every hour or keeps
    /// retrying against a revoked device.
    #[test]
    fn only_an_expiry_is_recoverable() {
        assert!(ClosePayload::auth_token_expired().is_recoverable());
        for code in [
            ErrorCode::AuthTokenInvalid,
            ErrorCode::AuthDeviceRevoked,
            ErrorCode::SyncProtocolVersionMismatch,
        ] {
            let c = ClosePayload {
                code,
                reason: String::new(),
            };
            assert!(!c.is_recoverable(), "{code:?} must not look recoverable");
        }
    }

    fn sample_op_batch() -> OpBatchPayload {
        OpBatchPayload {
            ops: vec![vec![1, 2, 3], vec![], vec![0xff; 40]],
            batch_id: 0x0102_0304_0506_0708,
            stream_id: [7u8; 16],
        }
    }

    #[test]
    fn op_batch_round_trip() {
        let v = sample_op_batch();
        let bytes = v.encode().unwrap();
        let back = OpBatchPayload::decode(&bytes).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn ack_round_trip() {
        let v = AckPayload {
            batch_id: 42,
            stream_id: [9u8; 16],
            server_first_seen_ms: 1_700_000_000_000,
        };
        let bytes = v.encode().unwrap();
        assert_eq!(AckPayload::decode(&bytes).unwrap(), v);
    }

    #[test]
    fn nack_round_trip() {
        let v = NackPayload {
            code: ErrorCode::SyncOpInvalid,
            reason: "bad batch".to_string(),
            batch_id: 7,
            stream_id: [3u8; 16],
        };
        let bytes = v.encode().unwrap();
        assert_eq!(NackPayload::decode(&bytes).unwrap(), v);
    }

    #[test]
    fn subscribe_round_trip() {
        let v = SubscribePayload {
            streams: vec![
                SubscribeEntry {
                    cursors: vec![CursorEntry {
                        device_id: [1u8; 16],
                        last_applied_seq: 5,
                    }],
                    stream_id: [2u8; 16],
                },
                SubscribeEntry {
                    cursors: vec![],
                    stream_id: [4u8; 16],
                },
            ],
        };
        let bytes = v.encode().unwrap();
        assert_eq!(SubscribePayload::decode(&bytes).unwrap(), v);
    }

    #[test]
    fn caught_up_round_trip() {
        let v = CaughtUpPayload {
            stream_id: [5u8; 16],
        };
        let bytes = v.encode().unwrap();
        assert_eq!(CaughtUpPayload::decode(&bytes).unwrap(), v);
    }

    #[test]
    fn error_round_trip() {
        let v = ErrorPayload {
            code: ErrorCode::ProtocolBadMagic,
            reason: "nope".to_string(),
        };
        let bytes = v.encode().unwrap();
        assert_eq!(ErrorPayload::decode(&bytes).unwrap(), v);
    }

    #[test]
    fn op_batch_ids_encode_as_byte_strings() {
        // A 16-byte CBOR byte string is `0x50` followed by 16 bytes. If the
        // id serialized as an array of integers it would be far longer.
        let v = OpBatchPayload {
            ops: vec![],
            batch_id: 0,
            stream_id: [0xab; 16],
        };
        let bytes = v.encode().unwrap();
        let needle = {
            let mut n = vec![0x50u8];
            n.extend_from_slice(&[0xab; 16]);
            n
        };
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle.as_slice()),
            "stream_id should encode as a compact 16-byte CBOR byte string"
        );
    }

    #[test]
    fn decode_rejects_trailing_garbage() {
        let mut bytes = sample_op_batch().encode().unwrap();
        bytes.push(0x00); // trailing byte beyond the canonical item
        assert!(matches!(
            OpBatchPayload::decode(&bytes),
            Err(CanonicalError::NonCanonical { .. })
        ));
    }

    #[test]
    fn decode_rejects_malformed() {
        assert!(OpBatchPayload::decode(&[0xff, 0xff, 0xff]).is_err());
    }
}
