//! Account creation request / response shapes, and the recovery blob's
//! transport encoding.
//!
//! Per `docs/06-server/api.md` §POST /api/v1/accounts.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// What a client asks `POST /api/v1/accounts` to record.
///
/// The three key-shaped fields carry **bytes**, not the base64url strings the
/// wire does, and [`encode_public_key`] / [`encode_recovery_blob`] are applied
/// once by `sunrise_relay_client::bootstrap`. That is the same correction
/// `DeviceIdentity::device_pub_s` took: a `String` field accepts any string at
/// all, and the one caller that filled one by hand registered the hex of a
/// 16-byte device id as a public key and could not authenticate afterwards. A
/// field that takes `[u8; 32]` has nowhere for that mistake to live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountCreateRequest {
    /// User email (will be normalized server-side).
    pub email: String,
    /// Identity Ed25519 public key (`ID_S_pub`).
    pub identity_signing_pub: [u8; 32],
    /// Identity X25519 public key (`ID_D_pub`).
    pub identity_dh_pub: [u8; 32],
    /// The sealed recovery blob, or `None` from a device that cannot produce
    /// one.
    ///
    /// `None` is not "no recovery": it means *this device* holds no
    /// `ID_D_priv` to seal — every device admitted by pairing — and the field
    /// is then absent from the request rather than empty, so the server's
    /// write-once column keeps whatever the founding device stored.
    pub recovery_blob: Option<Vec<u8>>,
    /// Terms acceptance timestamp (ms since epoch).
    pub terms_at_ms: u64,
}

/// `GET /api/v1/accounts/me` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountInfo {
    /// 16-byte identity id, hex.
    pub identity_id: String,
    /// User email (normalized).
    pub email: String,
    /// Subscription tier.
    pub tier: String,
    /// Number of registered devices.
    pub device_count: u32,
    /// Account creation time (ms since epoch).
    pub created_at_ms: u64,
}

/// The encoding `recovery_blob` travels in: base64url, no padding.
///
/// Declared here, beside the field, rather than at each call site. The blob is
/// opaque bytes to everything between the sealing device and the recovering
/// one — `docs/06-server/relay-and-blob-storage.md` records that the server
/// does not introspect it — so the one thing that must be agreed is how those
/// bytes become a JSON string, and the producer and the consumer now read that
/// from the same place.
const BLOB_B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Encode a sealed recovery blob for [`AccountCreateRequest::recovery_blob`].
#[must_use]
pub fn encode_recovery_blob(blob: &[u8]) -> String {
    BLOB_B64.encode(blob)
}

/// Decode the `recovery_blob` a server served back.
///
/// # Errors
/// The string is not base64url. Nothing further is checked here: the blob's
/// magic prefix, its version and its authenticity are
/// `sunrise_crypto::unseal_recovery_blob`'s to judge, and judging them twice
/// would put two answers in the tree about what a valid blob is.
pub fn decode_recovery_blob(encoded: &str) -> Result<Vec<u8>, base64::DecodeError> {
    BLOB_B64.decode(encoded)
}

/// Encode an identity public key the way `POST /api/v1/accounts` reads it.
///
/// Beside [`encode_recovery_blob`] and using the same alphabet, because the
/// request carries both and a client that formats either by hand is a client
/// that can format it differently from the one that reads it back.
#[must_use]
pub fn encode_public_key(key: &[u8; 32]) -> String {
    BLOB_B64.encode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_survives_the_transport_encoding() {
        // A byte run with the two characters standard base64 differs on, so a
        // decoder that took the wrong alphabet would fail here rather than on
        // one blob in sixty-four.
        let blob: Vec<u8> = (0u8..=255).collect();
        let encoded = encode_recovery_blob(&blob);
        assert!(!encoded.contains(['+', '/', '=']), "{encoded}");
        assert_eq!(decode_recovery_blob(&encoded).unwrap(), blob);
    }
}
