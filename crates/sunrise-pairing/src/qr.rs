//! Pairing QR payload encoder/decoder.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md` §QR. The payload is a
//! UTF-8 JSON object with lex-ordered keys. Field encodings:
//!
//! - `magic_v1`: 5-byte magic prefix (PairingPayload kind, version 1) as
//!   10 lowercase hex chars.
//! - `pair_id`: 16 random bytes, base64url no-pad.
//! - `n_static_pub`: N's ephemeral X25519 static public, base64url no-pad.
//! - `account_email_hash`: 4-byte hash of the normalized email, 8 hex chars.
//! - `relay_url`: HTTPS or WSS, ≤ 256 bytes.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sunrise_cbor::magic::{MagicKind, MAGIC_FIRST_TWO};
use thiserror::Error;

const PAIRING_VERSION: u16 = 1;

/// 10 lowercase hex chars representing the 5-byte magic prefix at v1.
pub const MAGIC_V1_HEX: &str = "53520700010";
// (Above: "SR" is 5352, kind=07 (PairingPayload), version=0001 → "53 52 07 00 01"
// = 10 lower-hex chars: "5352070001")

/// QR payload as parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrPayload {
    /// 10 lowercase hex chars of the 5-byte magic prefix.
    pub magic_v1: String,
    /// 16-byte pair-session id, base64url no-pad.
    pub pair_id: String,
    /// 32-byte X25519 ephemeral static public key, base64url no-pad.
    pub n_static_pub: String,
    /// 8 hex chars of the 4-byte email hash.
    pub account_email_hash: String,
    /// HTTPS or WSS URL, ≤ 256 bytes.
    pub relay_url: String,
}

/// QR payload errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum QrPayloadError {
    /// JSON parse failure.
    #[error("JSON error: {0}")]
    Json(String),
    /// Magic prefix string mismatch.
    #[error("magic_v1 mismatch")]
    BadMagic,
    /// Field had wrong shape.
    #[error("field {0} has wrong shape")]
    BadField(&'static str),
    /// `relay_url` longer than 256 bytes.
    #[error("relay_url > 256 bytes")]
    RelayUrlTooLong,
}

/// Encode a QR payload to a UTF-8 JSON string with lex-ordered keys.
pub fn encode_qr_payload(p: &QrPayload) -> Result<String, QrPayloadError> {
    if p.relay_url.len() > 256 {
        return Err(QrPayloadError::RelayUrlTooLong);
    }
    let canonical = serde_json::json!({
        "account_email_hash": p.account_email_hash,
        "magic_v1": p.magic_v1,
        "n_static_pub": p.n_static_pub,
        "pair_id": p.pair_id,
        "relay_url": p.relay_url,
    });
    serde_json::to_string(&canonical).map_err(|e| QrPayloadError::Json(e.to_string()))
}

/// Decode a QR payload from a UTF-8 JSON string. Verifies the magic prefix.
pub fn decode_qr_payload(s: &str) -> Result<QrPayload, QrPayloadError> {
    let p: QrPayload = serde_json::from_str(s).map_err(|e| QrPayloadError::Json(e.to_string()))?;
    if p.magic_v1.len() != 10 {
        return Err(QrPayloadError::BadMagic);
    }
    let bytes = hex::decode(&p.magic_v1).map_err(|_| QrPayloadError::BadMagic)?;
    if bytes.len() != 5
        || bytes[0..2] != MAGIC_FIRST_TWO
        || bytes[2] != MagicKind::PairingPayload.as_byte()
        || u16::from_be_bytes([bytes[3], bytes[4]]) != PAIRING_VERSION
    {
        return Err(QrPayloadError::BadMagic);
    }
    // Validate the base64url fields decode and have the right length.
    let pair_id = URL_SAFE_NO_PAD
        .decode(p.pair_id.as_bytes())
        .map_err(|_| QrPayloadError::BadField("pair_id"))?;
    if pair_id.len() != 16 {
        return Err(QrPayloadError::BadField("pair_id"));
    }
    let pubk = URL_SAFE_NO_PAD
        .decode(p.n_static_pub.as_bytes())
        .map_err(|_| QrPayloadError::BadField("n_static_pub"))?;
    if pubk.len() != 32 {
        return Err(QrPayloadError::BadField("n_static_pub"));
    }
    if p.account_email_hash.len() != 8 || hex::decode(&p.account_email_hash).is_err() {
        return Err(QrPayloadError::BadField("account_email_hash"));
    }
    if p.relay_url.len() > 256 {
        return Err(QrPayloadError::RelayUrlTooLong);
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> QrPayload {
        // Construct a magic_v1 string that matches the spec.
        let mut magic = [0u8; 5];
        magic[0..2].copy_from_slice(&MAGIC_FIRST_TWO);
        magic[2] = MagicKind::PairingPayload.as_byte();
        magic[3..5].copy_from_slice(&PAIRING_VERSION.to_be_bytes());
        let magic_hex = hex::encode(magic);

        let pair_id = URL_SAFE_NO_PAD.encode([7u8; 16]);
        let n_static = URL_SAFE_NO_PAD.encode([3u8; 32]);
        QrPayload {
            magic_v1: magic_hex,
            pair_id,
            n_static_pub: n_static,
            account_email_hash: "abcdef01".to_string(),
            relay_url: "wss://relay.example/sync".to_string(),
        }
    }

    #[test]
    fn round_trip() {
        let p = fixture();
        let s = encode_qr_payload(&p).unwrap();
        let back = decode_qr_payload(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut p = fixture();
        p.magic_v1 = "0000000000".to_string();
        let s = encode_qr_payload(&p).unwrap();
        assert_eq!(decode_qr_payload(&s), Err(QrPayloadError::BadMagic));
    }

    #[test]
    fn rejects_short_pair_id() {
        let mut p = fixture();
        p.pair_id = URL_SAFE_NO_PAD.encode([0u8; 8]);
        let s = encode_qr_payload(&p).unwrap();
        assert_eq!(
            decode_qr_payload(&s),
            Err(QrPayloadError::BadField("pair_id"))
        );
    }

    #[test]
    fn rejects_long_relay_url() {
        let mut p = fixture();
        p.relay_url = "x".repeat(257);
        assert_eq!(encode_qr_payload(&p), Err(QrPayloadError::RelayUrlTooLong));
    }
}
