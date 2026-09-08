//! v1 message kind discriminators.
//!
//! Per `docs/05-sync/wire-protocol.md`:
//!
//! ```text
//! 0x01  Hello           C → S
//! 0x02  HelloAck        S → C
//! 0x03  OpBatch         C ↔ S
//! 0x04  Ack             S → C
//! 0x05  Nack            S → C
//! 0x06  Subscribe       C → S
//! 0x07  StreamUpdate    S → C (server-pushed)
//! 0x08  SnapshotReq     C → S
//! 0x09  SnapshotResp    S → C
//! 0x0A  PresenceBeacon  C → S
//! 0x0B  PresenceUpdate  S → C
//! 0x0C  Ping            C ↔ S
//! 0x0D  Pong            C ↔ S
//! 0x0E  Error           S → C
//! 0x0F  Close           C ↔ S
//! 0x12  RefreshToken    C → S
//! 0x13  RefreshTokenAck S → C
//! ```
//!
//! `0x10` and `0x11` are deliberately unassigned; `RefreshToken` takes `0x12`
//! so the v1 block `0x01..=0x0F` stays contiguous and a new kind is visibly a
//! later addition rather than an edit of the original table.

use thiserror::Error;

/// v1 wire message kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgKind {
    /// `0x01` Client → Server: announce versions / capabilities.
    Hello = 0x01,
    /// `0x02` Server → Client: confirm versions / capabilities.
    HelloAck = 0x02,
    /// `0x03` Bidirectional: batched op envelopes.
    OpBatch = 0x03,
    /// `0x04` Server → Client: positive ack for a batch.
    Ack = 0x04,
    /// `0x05` Server → Client: rejection for a batch.
    Nack = 0x05,
    /// `0x06` Client → Server: subscribe to stream updates.
    Subscribe = 0x06,
    /// `0x07` Server → Client: server-pushed stream update.
    StreamUpdate = 0x07,
    /// `0x08` Client → Server: request a snapshot.
    SnapshotReq = 0x08,
    /// `0x09` Server → Client: deliver a snapshot.
    SnapshotResp = 0x09,
    /// `0x0A` Client → Server: presence beacon.
    PresenceBeacon = 0x0A,
    /// `0x0B` Server → Client: aggregated presence update.
    PresenceUpdate = 0x0B,
    /// `0x0C` Bidirectional: liveness probe.
    Ping = 0x0C,
    /// `0x0D` Bidirectional: liveness probe response.
    Pong = 0x0D,
    /// `0x0E` Server → Client: terminal-level error message.
    Error = 0x0E,
    /// `0x0F` Bidirectional: graceful close.
    Close = 0x0F,
    /// `0x12` Client → Server: present a fresh bearer token on an ALREADY
    /// OPEN session.
    ///
    /// Out of band on purpose. A token expires mid-session — that is what
    /// tokens do — and without this frame the only remedy is to close the
    /// socket and renegotiate, which drops the subscription, re-runs the
    /// handshake, and loses every op in flight. With it, the server closes
    /// with [`sunrise_error::ErrorCode::AuthTokenExpired`] only when the client fails to
    /// refresh, not merely because time passed.
    ///
    /// This commit adds the protocol surface; the behaviour behind it is
    /// wired separately (issue #7).
    RefreshToken = 0x12,
    /// `0x13` Server → Client: the bearer in a `RefreshToken` frame verified,
    /// and the session's deadline has moved out to its `exp`.
    ///
    /// Sent only when `Capability::SrvTokenRefresh` was agreed at Hello. It
    /// exists because silence is ambiguous: a server that accepted the refresh
    /// and one too old to know `0x12` both say nothing, and the client's right
    /// response differs — keep the session, or stop trusting it. A rejected
    /// refresh still answers with `Error`, so every outcome is now explicit.
    RefreshTokenAck = 0x13,
}

/// Error from [`MsgKind::from_byte`].
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown message kind: {0:#x}")]
pub struct UnknownMsgKind(pub u8);

impl MsgKind {
    /// Underlying byte.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// Decode from byte.
    pub const fn from_byte(b: u8) -> Result<Self, UnknownMsgKind> {
        match b {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::HelloAck),
            0x03 => Ok(Self::OpBatch),
            0x04 => Ok(Self::Ack),
            0x05 => Ok(Self::Nack),
            0x06 => Ok(Self::Subscribe),
            0x07 => Ok(Self::StreamUpdate),
            0x08 => Ok(Self::SnapshotReq),
            0x09 => Ok(Self::SnapshotResp),
            0x0A => Ok(Self::PresenceBeacon),
            0x0B => Ok(Self::PresenceUpdate),
            0x0C => Ok(Self::Ping),
            0x0D => Ok(Self::Pong),
            0x0E => Ok(Self::Error),
            0x0F => Ok(Self::Close),
            0x12 => Ok(Self::RefreshToken),
            0x13 => Ok(Self::RefreshTokenAck),
            other => Err(UnknownMsgKind(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_kinds() {
        let all = [
            MsgKind::Hello,
            MsgKind::HelloAck,
            MsgKind::OpBatch,
            MsgKind::Ack,
            MsgKind::Nack,
            MsgKind::Subscribe,
            MsgKind::StreamUpdate,
            MsgKind::SnapshotReq,
            MsgKind::SnapshotResp,
            MsgKind::PresenceBeacon,
            MsgKind::PresenceUpdate,
            MsgKind::Ping,
            MsgKind::Pong,
            MsgKind::Error,
            MsgKind::Close,
            MsgKind::RefreshToken,
            MsgKind::RefreshTokenAck,
        ];
        for k in all {
            assert_eq!(MsgKind::from_byte(k.as_byte()).unwrap(), k);
        }
    }

    #[test]
    fn unknown_kind_rejected() {
        assert_eq!(MsgKind::from_byte(0x00), Err(UnknownMsgKind(0x00)));
        // 0x10 and 0x11 are unassigned and stay that way.
        assert_eq!(MsgKind::from_byte(0x10), Err(UnknownMsgKind(0x10)));
        assert_eq!(MsgKind::from_byte(0x11), Err(UnknownMsgKind(0x11)));
        assert_eq!(MsgKind::from_byte(0x13), Ok(MsgKind::RefreshTokenAck));
        assert_eq!(MsgKind::from_byte(0xff), Err(UnknownMsgKind(0xff)));
    }

    /// A message KIND is not a flag: an unrecognised one means the frame's
    /// payload cannot be interpreted at all, so it is refused rather than
    /// skipped. That is the opposite of the rule for `FrameFlags`, and the
    /// difference is deliberate — see `frame.rs`.
    #[test]
    fn refresh_token_is_a_distinct_kind() {
        assert_eq!(MsgKind::RefreshToken.as_byte(), 0x12);
        assert_eq!(MsgKind::RefreshTokenAck.as_byte(), 0x13);
        assert_ne!(MsgKind::RefreshToken, MsgKind::Hello);
    }
}
