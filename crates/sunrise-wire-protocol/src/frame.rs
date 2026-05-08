//! 11-byte frame header + framing codec.
//!
//! Layout per `spec/05-sync/wire-protocol.md`:
//!
//! ```text
//! 0:2     magic       "SR" (ASCII)
//! 2:3     kind        u8 = 1 (wire frame)
//! 3:5     version     u16 big-endian (= WIRE_PROTO_V)
//! 5:6     msg_kind    u8 (see [`MsgKind`])
//! 6:7     flags       u8 (bit 0 = zstd; other bits MUST be 0)
//! 7:11    len_prefix  u32 big-endian (decompressed length)
//! 11:N    payload     CBOR (zstd-compressed if flag bit 0 set)
//! ```

use crate::messages::{MsgKind, UnknownMsgKind};
use sunrise_cbor::magic::{MagicKind, MAGIC_FIRST_TWO};
use sunrise_cbor::version::WIRE_PROTO_V;
use sunrise_error::ErrorCode;
use thiserror::Error;

/// Length of the frame header in bytes.
pub const FRAME_HEADER_LEN: usize = 11;

/// Maximum size of a single frame (post-decompression payload included).
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

/// Maximum size of a decompressed payload — protects against zip-bombs.
pub const MAX_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

/// Frame flags bitfield (only one bit defined in v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameFlags(u8);

impl FrameFlags {
    /// Empty flags.
    pub const EMPTY: Self = Self(0);

    /// `zstd` compression bit.
    pub const ZSTD: Self = Self(0b0000_0001);

    /// Whether zstd is set.
    #[must_use]
    pub const fn zstd(&self) -> bool {
        self.0 & Self::ZSTD.0 != 0
    }

    /// Underlying byte.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self.0
    }

    /// Construct from a raw byte; returns `None` if reserved bits are set.
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        if b & !Self::ZSTD.0 == 0 {
            Some(Self(b))
        } else {
            None
        }
    }
}

/// Decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Wire protocol version (matches `WIRE_PROTO_V`).
    pub version: u16,
    /// Message kind.
    pub msg_kind: MsgKind,
    /// Flags (compression, etc.).
    pub flags: FrameFlags,
    /// Length of payload AFTER decompression.
    pub decompressed_len: u32,
}

/// Frame errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    /// Frame buffer shorter than the 11-byte header.
    #[error("frame buffer shorter than {FRAME_HEADER_LEN} bytes (got {0})")]
    HeaderTruncated(usize),
    /// Magic prefix mismatch.
    #[error("frame magic prefix mismatch")]
    BadMagic,
    /// Wire protocol version unsupported.
    #[error("frame version {got} not supported (expected {wanted})")]
    BadVersion {
        /// Expected version.
        wanted: u16,
        /// Decoded version.
        got: u16,
    },
    /// Message kind unknown.
    #[error("unknown msg_kind: {0}")]
    UnknownMsgKind(#[from] UnknownMsgKind),
    /// Reserved flag bits were set.
    #[error("reserved flag bits set: {0:#b}")]
    ReservedFlags(u8),
    /// Frame size exceeded the 4 MiB cap.
    #[error("frame too large: {0} bytes > {MAX_FRAME_BYTES}")]
    FrameTooLarge(usize),
    /// Decompressed size exceeded the 16 MiB cap.
    #[error("decompressed payload exceeds {MAX_DECOMPRESSED_BYTES} bytes (got {0})")]
    DecompressBomb(usize),
    /// zstd decode failed.
    #[error("zstd decode failed")]
    DecompressError,
    /// Payload bytes shorter than the frame's `decompressed_len` claim
    /// (uncompressed mode only).
    #[error("frame payload truncated")]
    PayloadTruncated,
}

impl FrameError {
    /// Map to a canonical [`ErrorCode`] for the wire.
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::BadMagic
            | Self::BadVersion { .. }
            | Self::HeaderTruncated(_)
            | Self::ReservedFlags(_)
            | Self::PayloadTruncated => ErrorCode::ProtocolBadMagic,
            Self::UnknownMsgKind(_) => ErrorCode::SyncOpInvalid,
            Self::FrameTooLarge(_) => ErrorCode::ProtocolFrameTooLarge,
            Self::DecompressBomb(_) | Self::DecompressError => ErrorCode::ProtocolDecompressBomb,
        }
    }
}

/// Encode a frame: header + (optionally compressed) payload.
///
/// `payload` is the decompressed CBOR bytes; if `flags.zstd()` is set the
/// payload is compressed before writing. Returns the full byte buffer
/// (header + payload).
///
/// # Errors
/// `FrameTooLarge` if the resulting frame exceeds the 4 MiB cap.
/// `DecompressBomb` if `payload.len()` already exceeds the 16 MiB cap (we
/// never encode a frame whose decompressed payload would be over the cap).
pub fn encode_frame(
    msg_kind: MsgKind,
    flags: FrameFlags,
    payload: &[u8],
) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_DECOMPRESSED_BYTES {
        return Err(FrameError::DecompressBomb(payload.len()));
    }
    let body: Vec<u8> = if flags.zstd() {
        zstd::encode_all(payload, 0).map_err(|_| FrameError::DecompressError)?
    } else {
        payload.to_vec()
    };
    let total_len = FRAME_HEADER_LEN + body.len();
    if total_len > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge(total_len));
    }
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&MAGIC_FIRST_TWO);
    out.push(MagicKind::Frame.as_byte());
    out.extend_from_slice(&WIRE_PROTO_V.to_be_bytes());
    out.push(msg_kind.as_byte());
    out.push(flags.as_byte());
    let dec_len: u32 =
        u32::try_from(payload.len()).map_err(|_| FrameError::DecompressBomb(payload.len()))?;
    out.extend_from_slice(&dec_len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a frame: parse header, decompress if needed, return
/// `(header, payload)`.
///
/// # Errors
/// Header validation, size caps, decompress failures.
pub fn decode_frame(buf: &[u8]) -> Result<(FrameHeader, Vec<u8>), FrameError> {
    if buf.len() < FRAME_HEADER_LEN {
        return Err(FrameError::HeaderTruncated(buf.len()));
    }
    if buf.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge(buf.len()));
    }
    if buf[0..2] != MAGIC_FIRST_TWO {
        return Err(FrameError::BadMagic);
    }
    if buf[2] != MagicKind::Frame.as_byte() {
        return Err(FrameError::BadMagic);
    }
    let version = u16::from_be_bytes([buf[3], buf[4]]);
    if version != WIRE_PROTO_V {
        return Err(FrameError::BadVersion {
            wanted: WIRE_PROTO_V,
            got: version,
        });
    }
    let msg_kind = MsgKind::from_byte(buf[5])?;
    let flags = FrameFlags::from_byte(buf[6]).ok_or(FrameError::ReservedFlags(buf[6]))?;
    let decompressed_len = u32::from_be_bytes([buf[7], buf[8], buf[9], buf[10]]);
    if (decompressed_len as usize) > MAX_DECOMPRESSED_BYTES {
        return Err(FrameError::DecompressBomb(decompressed_len as usize));
    }
    let body = &buf[FRAME_HEADER_LEN..];
    let payload = if flags.zstd() {
        let dec = zstd::decode_all(body).map_err(|_| FrameError::DecompressError)?;
        if dec.len() != decompressed_len as usize {
            // Spec says `len_prefix` is the decompressed length; refuse
            // mismatch.
            return Err(FrameError::PayloadTruncated);
        }
        if dec.len() > MAX_DECOMPRESSED_BYTES {
            return Err(FrameError::DecompressBomb(dec.len()));
        }
        dec
    } else {
        if body.len() != decompressed_len as usize {
            return Err(FrameError::PayloadTruncated);
        }
        body.to_vec()
    };
    Ok((
        FrameHeader {
            version,
            msg_kind,
            flags,
            decompressed_len,
        },
        payload,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_uncompressed() {
        let payload = b"hello frame";
        let bytes = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, payload).unwrap();
        // Magic prefix sanity.
        assert_eq!(&bytes[..3], b"SR\x01");
        let (hdr, pl) = decode_frame(&bytes).unwrap();
        assert_eq!(hdr.version, WIRE_PROTO_V);
        assert_eq!(hdr.msg_kind, MsgKind::Hello);
        assert!(!hdr.flags.zstd());
        assert_eq!(pl, payload);
    }

    #[test]
    fn round_trip_compressed() {
        let payload = b"hello frame, compressed".repeat(64);
        let bytes = encode_frame(MsgKind::OpBatch, FrameFlags::ZSTD, &payload).unwrap();
        assert!(bytes.len() < payload.len() + FRAME_HEADER_LEN);
        let (hdr, pl) = decode_frame(&bytes).unwrap();
        assert!(hdr.flags.zstd());
        assert_eq!(pl, payload);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode_frame(MsgKind::Ping, FrameFlags::EMPTY, b"x").unwrap();
        bytes[0] = b'X';
        assert_eq!(decode_frame(&bytes), Err(FrameError::BadMagic));
    }

    #[test]
    fn rejects_unknown_msg_kind() {
        let mut bytes = encode_frame(MsgKind::Ping, FrameFlags::EMPTY, b"x").unwrap();
        bytes[5] = 0xff; // unknown kind
        assert!(matches!(
            decode_frame(&bytes),
            Err(FrameError::UnknownMsgKind(_))
        ));
    }

    #[test]
    fn rejects_reserved_flags() {
        let mut bytes = encode_frame(MsgKind::Ping, FrameFlags::EMPTY, b"x").unwrap();
        bytes[6] = 0b0000_0010;
        assert!(matches!(
            decode_frame(&bytes),
            Err(FrameError::ReservedFlags(_))
        ));
    }

    #[test]
    fn rejects_oversize_frame() {
        let huge = vec![0u8; MAX_FRAME_BYTES];
        let err = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &huge).unwrap_err();
        assert!(matches!(err, FrameError::FrameTooLarge(_)));
    }

    #[test]
    fn rejects_decompress_bomb_marker() {
        // Manually craft a frame whose len_prefix claims 17 MiB.
        let mut bytes = encode_frame(MsgKind::Ping, FrameFlags::EMPTY, b"x").unwrap();
        #[allow(clippy::cast_possible_truncation)]
        let bomb_len: u32 = (MAX_DECOMPRESSED_BYTES as u32) + 1;
        bytes[7..11].copy_from_slice(&bomb_len.to_be_bytes());
        assert!(matches!(
            decode_frame(&bytes),
            Err(FrameError::DecompressBomb(_))
        ));
    }
}
