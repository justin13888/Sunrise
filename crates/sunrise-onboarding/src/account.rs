//! Account creation request / response shapes.
//!
//! Per `docs/06-server/api.md` §POST /api/v1/accounts.

use serde::{Deserialize, Serialize};

/// `POST /api/v1/accounts` request body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountCreateRequest {
    /// User email (will be normalized server-side).
    pub email: String,
    /// Identity Ed25519 public key (32 bytes, base64url no-pad).
    pub identity_signing_pub: String,
    /// Identity X25519 public key (32 bytes, base64url no-pad).
    pub identity_dh_pub: String,
    /// Encrypted recovery blob (opaque to server; base64url no-pad).
    pub recovery_blob: String,
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
