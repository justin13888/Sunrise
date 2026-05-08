//! v1 message kind discriminators.
//!
//! Per `spec/05-sync/wire-protocol.md`:
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
//! ```

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
        ];
        for k in all {
            assert_eq!(MsgKind::from_byte(k.as_byte()).unwrap(), k);
        }
    }

    #[test]
    fn unknown_kind_rejected() {
        assert_eq!(MsgKind::from_byte(0x00), Err(UnknownMsgKind(0x00)));
        assert_eq!(MsgKind::from_byte(0x10), Err(UnknownMsgKind(0x10)));
        assert_eq!(MsgKind::from_byte(0xff), Err(UnknownMsgKind(0xff)));
    }
}
