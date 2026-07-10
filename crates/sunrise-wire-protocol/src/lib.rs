//! Sunrise sync wire protocol.
//!
//! Implements `docs/05-sync/wire-protocol.md`. The wire is framed:
//! 11 byte header (magic + kind + version + msg_kind + flags + len),
//! followed by a payload (canonical CBOR, optionally zstd-compressed).
//!
//! v1 surface:
//! - [`FrameHeader`] / [`encode_frame`] / [`decode_frame`] — framing.
//! - [`MsgKind`] — discriminator for the 15 v1 message kinds.
//! - [`Hello`] / [`HelloAck`] — session negotiation.
//! - [`Capability`] — capability bitfield with required-bits enforcement.
//!
//! Higher-level messages (OpBatch, Subscribe, etc.) are typed in
//! [`messages`] but their full payloads ship in Phase 8 (sync).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod capability;
pub mod frame;
pub mod messages;
pub mod negotiation;

pub use capability::{Capability, CapabilityBits, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS};
pub use frame::{
    decode_frame, encode_frame, FrameError, FrameFlags, FrameHeader, FRAME_HEADER_LEN,
    MAX_DECOMPRESSED_BYTES, MAX_FRAME_BYTES,
};
pub use messages::MsgKind;
pub use negotiation::{Hello, HelloAck, NegotiationError};
