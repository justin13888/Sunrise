//! The one error type of the pairing codecs, shared by the payload
//! (`crate::payload`) and the three wire messages (`crate::protocol`), and
//! re-exported from the first.

use crate::payload::MAX_PAIRING_PAYLOAD;
use thiserror::Error;

/// Errors from the pairing codecs — the payload and all three wire messages.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PairingPayloadError {
    /// CBOR encode/decode failure.
    #[error("pairing payload cbor: {0}")]
    Cbor(String),
    /// A field was missing, or had the wrong CBOR type or byte length.
    #[error("invalid pairing payload field: {0}")]
    BadField(&'static str),
    /// `identity_id` is not the id derived from `ID_S_pub`.
    ///
    /// Checked by the receiver before anything is written: the sender chose
    /// every byte here, and a payload whose id names one identity while its key
    /// belongs to another would install a device that signs under one identity
    /// and files itself under a different one.
    #[error("pairing payload identity_id does not match its ID_S_pub")]
    IdentityMismatch,
    /// `genesis_identity_id` is not the id derived from `genesis_id_s_pub`.
    ///
    /// Checked for the same reason [`Self::IdentityMismatch`] is: both are
    /// values the sender chose, and a mismatched pair would anchor the
    /// receiver's identity-chain fold at a point no other replica of the
    /// account can reach.
    #[error("pairing payload genesis_identity_id does not match its genesis ID_S_pub")]
    GenesisMismatch,
    /// A pairing request's `device_id` is not the id derived from its
    /// `D_S_pub`.
    ///
    /// The joiner does not get to name its own id. Choosing it is how a device
    /// would claim an id the account's revocation register has no row for, or
    /// one that collides with a sibling's.
    #[error("pairing request device_id does not match its D_S_pub")]
    DeviceIdMismatch,
    /// The granted `DeviceCert` does not decode, does not verify under the
    /// identity the offer named, or names a different device.
    #[error("pairing grant certificate: {0}")]
    BadCert(&'static str),
    /// The encoded payload does not fit in one Noise transport message.
    ///
    /// Surfaced rather than silently truncated. Chunking it across several
    /// transport messages is the eventual answer — the channel is
    /// bidirectional and already carries multiple frames — but it needs a
    /// framing contract that reaches the Swift seam, where a sealed message is
    /// currently one base64 string. A vault this large is far past anything today
    /// produces, and a wrong answer here is a device that silently cannot read
    /// half its own content.
    #[error("pairing payload is {len} bytes, over the {MAX_PAIRING_PAYLOAD}-byte transport limit")]
    TooLarge {
        /// Encoded length that did not fit.
        len: usize,
    },
    /// The CBOR nests deeper than [`sunrise_cbor::MAX_VALUE_DEPTH`] arrays,
    /// maps and tags.
    ///
    /// The deepest message is the payload, at three: its map, the Stream-key
    /// map and one stream's epoch map. A field a newer build adds is held to
    /// the same eight, the message's own map included.
    #[error("pairing payload cbor nests too deep")]
    TooDeep,
}
