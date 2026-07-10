//! 5-byte magic prefix per `docs/10-cross-cutting/protocol-versioning.md` §3.
//!
//! Layout:
//!
//! ```text
//! 0:2   "SR"  (0x53 0x52)        ASCII fixed.
//! 2:3   kind  (u8)                see [`MagicKind`].
//! 3:5   version  (u16 big-endian) per-structure version constant.
//! ```

use sunrise_error::ErrorCode;
use thiserror::Error;

/// Length of the uniform 5-byte magic prefix.
pub const MAGIC_LEN: usize = 5;

/// First two bytes of every prefix: ASCII "SR".
pub const MAGIC_FIRST_TWO: [u8; 2] = [b'S', b'R'];

/// Discriminator byte assigning a magic prefix to a structure kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum MagicKind {
    /// Wire frame (versioned by `WIRE_PROTO_V`).
    Frame = 1,
    /// Op envelope (versioned by `DOC_SCHEMA_V`).
    OpEnvelope = 2,
    /// Recovery blob (versioned by recovery format version, currently 1).
    RecoveryBlob = 3,
    /// Snapshot blob (versioned by snapshot format version, currently 1).
    Snapshot = 4,
    /// Vault meta record (versioned by vault meta version, currently 1).
    VaultMeta = 5,
    /// Diagnostic bundle (versioned by bundle format version, currently 1).
    DiagBundle = 6,
    /// Pairing payload, base64url JSON inside (versioned by pairing v).
    PairingPayload = 7,
}

impl MagicKind {
    /// Underlying byte.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// Decode from byte. Returns `None` for unknown kinds.
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Frame),
            2 => Some(Self::OpEnvelope),
            3 => Some(Self::RecoveryBlob),
            4 => Some(Self::Snapshot),
            5 => Some(Self::VaultMeta),
            6 => Some(Self::DiagBundle),
            7 => Some(Self::PairingPayload),
            _ => None,
        }
    }
}

/// Decoded prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MagicPrefix {
    /// Structure kind.
    pub kind: MagicKind,
    /// Version (`u16` big-endian; structure-specific).
    pub version: u16,
}

/// Errors decoding a magic prefix.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MagicError {
    /// Buffer too short.
    #[error("magic prefix buffer must be ≥ {MAGIC_LEN} bytes; got {0}")]
    BadLength(usize),
    /// First two bytes were not `"SR"`.
    #[error("magic prefix does not start with 'SR'")]
    BadAscii,
    /// Kind byte not in the known set.
    #[error("magic prefix kind byte {0:#x} is unknown")]
    UnknownKind(u8),
    /// Version mismatch — caller wanted a specific version but found another.
    #[error("magic prefix version mismatch: wanted {wanted}, got {got}")]
    VersionMismatch {
        /// Expected version.
        wanted: u16,
        /// Decoded version.
        got: u16,
    },
}

impl MagicError {
    /// Map to a canonical [`ErrorCode`]. Per spec, all magic-prefix failures
    /// surface as `PROTOCOL_BAD_MAGIC`.
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        ErrorCode::ProtocolBadMagic
    }
}

/// Write a magic prefix to the start of `out`. Returns the number of bytes
/// written (always [`MAGIC_LEN`]).
pub fn write_prefix(out: &mut [u8], kind: MagicKind, version: u16) -> usize {
    assert!(out.len() >= MAGIC_LEN, "buffer too small for magic prefix");
    out[0] = MAGIC_FIRST_TWO[0];
    out[1] = MAGIC_FIRST_TWO[1];
    out[2] = kind.as_byte();
    out[3..5].copy_from_slice(&version.to_be_bytes());
    MAGIC_LEN
}

/// Decode a magic prefix from the start of `buf`.
pub fn decode_prefix(buf: &[u8]) -> Result<MagicPrefix, MagicError> {
    if buf.len() < MAGIC_LEN {
        return Err(MagicError::BadLength(buf.len()));
    }
    if buf[0..2] != MAGIC_FIRST_TWO {
        return Err(MagicError::BadAscii);
    }
    let kind = MagicKind::from_byte(buf[2]).ok_or(MagicError::UnknownKind(buf[2]))?;
    let version = u16::from_be_bytes([buf[3], buf[4]]);
    Ok(MagicPrefix { kind, version })
}

/// Convenience: decode and assert a specific kind + version.
///
/// # Errors
/// Returns the decode error or a [`MagicError::VersionMismatch`] /
/// [`MagicError::UnknownKind`] if the prefix doesn't match the expected
/// `(kind, version)` pair.
pub fn expect_prefix(buf: &[u8], kind: MagicKind, version: u16) -> Result<(), MagicError> {
    let p = decode_prefix(buf)?;
    if p.kind != kind {
        return Err(MagicError::UnknownKind(p.kind.as_byte()));
    }
    if p.version != version {
        return Err(MagicError::VersionMismatch {
            wanted: version,
            got: p.version,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::{DOC_SCHEMA_V, WIRE_PROTO_V};

    #[test]
    fn round_trip_all_kinds() {
        let kinds = [
            MagicKind::Frame,
            MagicKind::OpEnvelope,
            MagicKind::RecoveryBlob,
            MagicKind::Snapshot,
            MagicKind::VaultMeta,
            MagicKind::DiagBundle,
            MagicKind::PairingPayload,
        ];
        for k in kinds {
            for v in [0u16, 1, 0xff, 0x1234, 0xffff] {
                let mut buf = [0u8; MAGIC_LEN];
                let n = write_prefix(&mut buf, k, v);
                assert_eq!(n, MAGIC_LEN);
                let p = decode_prefix(&buf).unwrap();
                assert_eq!(
                    p,
                    MagicPrefix {
                        kind: k,
                        version: v
                    }
                );
            }
        }
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(decode_prefix(&[]), Err(MagicError::BadLength(0)));
        assert_eq!(decode_prefix(b"SR"), Err(MagicError::BadLength(2)));
    }

    #[test]
    fn rejects_bad_ascii() {
        let mut buf = [0u8; MAGIC_LEN];
        write_prefix(&mut buf, MagicKind::Frame, 1);
        buf[0] = b'X';
        assert_eq!(decode_prefix(&buf), Err(MagicError::BadAscii));
    }

    #[test]
    fn rejects_unknown_kind() {
        let buf = [b'S', b'R', 0xff, 0, 1];
        assert_eq!(decode_prefix(&buf), Err(MagicError::UnknownKind(0xff)));
    }

    #[test]
    fn frame_v1_canonical_bytes() {
        // Wire-frame magic at v1 should be `SR\x01\x00\x01`.
        let mut buf = [0u8; MAGIC_LEN];
        write_prefix(&mut buf, MagicKind::Frame, WIRE_PROTO_V);
        assert_eq!(buf, *b"SR\x01\x00\x01");
    }

    #[test]
    fn op_envelope_v1_canonical_bytes() {
        // Op-envelope magic at v1 should be `SR\x02\x00\x01`.
        let mut buf = [0u8; MAGIC_LEN];
        write_prefix(&mut buf, MagicKind::OpEnvelope, DOC_SCHEMA_V);
        assert_eq!(buf, *b"SR\x02\x00\x01");
    }

    #[test]
    fn expect_prefix_validates_pair() {
        let mut buf = [0u8; MAGIC_LEN];
        write_prefix(&mut buf, MagicKind::Frame, 1);
        expect_prefix(&buf, MagicKind::Frame, 1).unwrap();
        assert!(matches!(
            expect_prefix(&buf, MagicKind::Frame, 2),
            Err(MagicError::VersionMismatch { wanted: 2, got: 1 })
        ));
    }
}
