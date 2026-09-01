//! Stable error code enum.
//!
//! **GENERATED** mirror of `crates/sunrise-error/codes.toml`. Until the
//! build-script lands (target: Phase 17), this file is hand-maintained.
//! `tests/manifest_in_sync.rs` asserts the two stay aligned.

use crate::kind::ErrorKind;
use serde::{Deserialize, Serialize};

/// Canonical error code. Stable across versions; never repurposed.
///
/// Adding a code requires updating `codes.toml` and (for now) this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Older client received an unrecognized code.
    InternalUnknownCode,

    // Validation
    /// Title invalid (empty, too long, etc.).
    ValidationInvalidTitle,
    /// `due_at` < `scheduled_at`.
    ValidationDueBeforeScheduled,
    /// Encoded payload exceeds the per-Task envelope cap.
    ValidationPayloadTooLarge,
    /// `blocked_by` graph contains a cycle.
    ValidationBlockedByCycle,
    /// Generic validation failure with structured diagnostic.
    ValidationField,

    // Auth
    /// OIDC bearer token rejected.
    AuthTokenInvalid,
    /// OIDC bearer token expired.
    AuthTokenExpired,
    /// Device cert was revoked.
    AuthDeviceRevoked,
    /// A `header_sig_v2` device binding was present, resolved to an active
    /// device on the authenticated account, and still did not check out — a
    /// bad signature, an absent `Date`, or one outside the ±300 s replay
    /// window. Never emitted for a caller whose device did not resolve: that
    /// would make the code an account/device enumeration oracle.
    AuthDeviceSigInvalid,
    // `AuthQuotaExceeded` (203) and `StorageQuotaExceeded` (300) were removed
    // with ADR-0027, which takes per-account quotas out of v1; nothing ever
    // emitted either. Both ids stay burned in `codes.toml`.

    // Storage
    /// The vault is already open — another process holds the OS lock on
    /// `core.lock`, or another `Core` in this process holds it.
    StorageVaultLocked,
    /// Local DB schema is newer than this binary supports.
    StorageVTooNew,
    /// Local DB schema needs upgrade.
    StorageVTooOld,

    // Crypto
    /// AEAD open / signature / KDF failed.
    CryptoDecryptFailed,
    /// Recovery blob invalid (wrong code, corrupted ciphertext, ...).
    CryptoRecoveryBlobInvalid,
    /// AAD computed locally did not match envelope's expected AAD.
    CryptoAadMismatch,
    /// Inbound CBOR was not in canonical form.
    CryptoNonCanonicalCbor,
    /// Ed25519 signature verify failed.
    CryptoSigVerifyFailed,
    /// No common crypto suite with peer.
    CryptoSuiteMismatch,

    // Sync / wire
    /// No common wire-protocol version with peer.
    SyncProtocolVersionMismatch,
    /// Op chain root drift detected — possible tampering.
    SyncTamperDetected,
    /// Network unreachable.
    SyncNetworkUnavailable,
    /// `OpBatch` exceeded the batch-size limit.
    SyncBatchTooLarge,
    /// Op decoded but failed semantic checks.
    SyncOpInvalid,
    /// Stream id not found on this server.
    SyncStreamNotFound,
    /// The relay evicted retained frames past a subscriber's cursor.
    SyncCursorGap,
    /// First 5 bytes of an envelope/frame did not match the expected magic.
    ProtocolBadMagic,
    /// Frame larger than 4 MiB cap.
    ProtocolFrameTooLarge,
    /// Decompressed payload exceeded 16 MiB cap.
    ProtocolDecompressBomb,
    /// Server's `doc_schema_floor` exceeds client's `doc_schema_max`.
    DocSchemaTooOld,
    /// Vault was written by a binary with newer doc-schema.
    DocSchemaTooNew,
    /// Required capability bit missing during negotiation.
    CapabilityRequiredMissing,

    // Relay
    /// Receiver's grant for this Stream was revoked.
    RelayGrantRevoked,
    /// The relay could not read or write its own durable op log, so it can
    /// neither ack a batch nor prove a replay was complete. Transient: the
    /// client keeps the op and retries.
    RelayStorageUnavailable,

    // Integrations
    /// Integration OAuth refresh failed; user must re-auth.
    IntegrationReauthRequired,
    /// External provider returned 429.
    IntegrationRateLimited,

    // Internal
    /// Catastrophic internal error (e.g., panic caught at FFI boundary).
    FatalInternal,
}

impl ErrorCode {
    /// Stable wire-string spelling. This MUST match the `name` field in
    /// `codes.toml` byte-for-byte.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InternalUnknownCode => "INTERNAL_UNKNOWN_CODE",
            Self::ValidationInvalidTitle => "VALIDATION_INVALID_TITLE",
            Self::ValidationDueBeforeScheduled => "VALIDATION_DUE_BEFORE_SCHEDULED",
            Self::ValidationPayloadTooLarge => "VALIDATION_PAYLOAD_TOO_LARGE",
            Self::ValidationBlockedByCycle => "VALIDATION_BLOCKED_BY_CYCLE",
            Self::ValidationField => "VALIDATION_FIELD",
            Self::AuthTokenInvalid => "AUTH_TOKEN_INVALID",
            Self::AuthTokenExpired => "AUTH_TOKEN_EXPIRED",
            Self::AuthDeviceRevoked => "AUTH_DEVICE_REVOKED",
            Self::AuthDeviceSigInvalid => "AUTH_DEVICE_SIG_INVALID",
            Self::StorageVaultLocked => "STORAGE_VAULT_LOCKED",
            Self::StorageVTooNew => "STORAGE_V_TOO_NEW",
            Self::StorageVTooOld => "STORAGE_V_TOO_OLD",
            Self::CryptoDecryptFailed => "CRYPTO_DECRYPT_FAILED",
            Self::CryptoRecoveryBlobInvalid => "CRYPTO_RECOVERY_BLOB_INVALID",
            Self::CryptoAadMismatch => "CRYPTO_AAD_MISMATCH",
            Self::CryptoNonCanonicalCbor => "CRYPTO_NON_CANONICAL_CBOR",
            Self::CryptoSigVerifyFailed => "CRYPTO_SIG_VERIFY_FAILED",
            Self::CryptoSuiteMismatch => "CRYPTO_SUITE_MISMATCH",
            Self::SyncProtocolVersionMismatch => "SYNC_PROTOCOL_VERSION_MISMATCH",
            Self::SyncTamperDetected => "SYNC_TAMPER_DETECTED",
            Self::SyncNetworkUnavailable => "SYNC_NETWORK_UNAVAILABLE",
            Self::SyncBatchTooLarge => "SYNC_BATCH_TOO_LARGE",
            Self::SyncOpInvalid => "SYNC_OP_INVALID",
            Self::SyncStreamNotFound => "SYNC_STREAM_NOT_FOUND",
            Self::SyncCursorGap => "SYNC_CURSOR_GAP",
            Self::ProtocolBadMagic => "PROTOCOL_BAD_MAGIC",
            Self::ProtocolFrameTooLarge => "PROTOCOL_FRAME_TOO_LARGE",
            Self::ProtocolDecompressBomb => "PROTOCOL_DECOMPRESS_BOMB",
            Self::DocSchemaTooOld => "DOC_SCHEMA_TOO_OLD",
            Self::DocSchemaTooNew => "DOC_SCHEMA_TOO_NEW",
            Self::CapabilityRequiredMissing => "CAPABILITY_REQUIRED_MISSING",
            Self::RelayGrantRevoked => "RELAY_GRANT_REVOKED",
            Self::RelayStorageUnavailable => "RELAY_STORAGE_UNAVAILABLE",
            Self::IntegrationReauthRequired => "INTEGRATION_REAUTH_REQUIRED",
            Self::IntegrationRateLimited => "INTEGRATION_RATE_LIMITED",
            Self::FatalInternal => "FATAL_INTERNAL",
        }
    }

    /// Default error category per the manifest.
    #[must_use]
    pub const fn default_kind(self) -> ErrorKind {
        match self {
            // Internal
            Self::InternalUnknownCode
            | Self::CryptoDecryptFailed
            | Self::CryptoAadMismatch
            | Self::CryptoNonCanonicalCbor
            | Self::CryptoSigVerifyFailed
            | Self::FatalInternal => ErrorKind::Internal,
            // User
            Self::ValidationInvalidTitle
            | Self::ValidationDueBeforeScheduled
            | Self::ValidationPayloadTooLarge
            | Self::ValidationBlockedByCycle
            | Self::ValidationField
            | Self::AuthTokenInvalid
            | Self::AuthDeviceSigInvalid
            | Self::StorageVTooNew
            | Self::StorageVTooOld
            | Self::CryptoRecoveryBlobInvalid
            | Self::SyncBatchTooLarge
            | Self::SyncStreamNotFound
            | Self::IntegrationReauthRequired => ErrorKind::User,
            // Permanent
            Self::AuthDeviceRevoked
            | Self::CryptoSuiteMismatch
            | Self::SyncProtocolVersionMismatch
            | Self::SyncTamperDetected
            | Self::SyncOpInvalid
            | Self::SyncCursorGap
            | Self::ProtocolBadMagic
            | Self::ProtocolFrameTooLarge
            | Self::ProtocolDecompressBomb
            | Self::DocSchemaTooOld
            | Self::DocSchemaTooNew
            | Self::CapabilityRequiredMissing
            | Self::RelayGrantRevoked => ErrorKind::Permanent,
            // Transient
            Self::AuthTokenExpired
            | Self::StorageVaultLocked
            | Self::SyncNetworkUnavailable
            | Self::RelayStorageUnavailable
            | Self::IntegrationRateLimited => ErrorKind::Transient,
        }
    }

    /// Whether retrying the operation is sanctioned (per the manifest's
    /// `retryable` flag).
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::AuthTokenExpired
                | Self::StorageVaultLocked
                | Self::SyncNetworkUnavailable
                | Self::RelayStorageUnavailable
                | Self::IntegrationRateLimited
        )
    }

    /// Iteration over every code variant — useful for completeness tests.
    #[must_use]
    pub const fn all() -> [Self; 37] {
        [
            Self::InternalUnknownCode,
            Self::ValidationInvalidTitle,
            Self::ValidationDueBeforeScheduled,
            Self::ValidationPayloadTooLarge,
            Self::ValidationBlockedByCycle,
            Self::ValidationField,
            Self::AuthTokenInvalid,
            Self::AuthTokenExpired,
            Self::AuthDeviceRevoked,
            Self::AuthDeviceSigInvalid,
            Self::StorageVaultLocked,
            Self::StorageVTooNew,
            Self::StorageVTooOld,
            Self::CryptoDecryptFailed,
            Self::CryptoRecoveryBlobInvalid,
            Self::CryptoAadMismatch,
            Self::CryptoNonCanonicalCbor,
            Self::CryptoSigVerifyFailed,
            Self::CryptoSuiteMismatch,
            Self::SyncProtocolVersionMismatch,
            Self::SyncTamperDetected,
            Self::SyncNetworkUnavailable,
            Self::SyncBatchTooLarge,
            Self::SyncOpInvalid,
            Self::SyncStreamNotFound,
            Self::SyncCursorGap,
            Self::ProtocolBadMagic,
            Self::ProtocolFrameTooLarge,
            Self::ProtocolDecompressBomb,
            Self::DocSchemaTooOld,
            Self::DocSchemaTooNew,
            Self::CapabilityRequiredMissing,
            Self::RelayGrantRevoked,
            Self::RelayStorageUnavailable,
            Self::IntegrationReauthRequired,
            Self::IntegrationRateLimited,
            Self::FatalInternal,
        ]
    }

    /// Parse from the canonical wire string. Used by clients receiving codes
    /// from the wire.
    ///
    /// Named `from_wire_str` rather than `from_str` to avoid shadowing
    /// `std::str::FromStr::from_str` (`clippy::should_implement_trait`).
    #[must_use]
    pub fn from_wire_str(s: &str) -> Option<Self> {
        Self::all().into_iter().find(|c| c.as_str() == s)
    }
}

impl core::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for code in ErrorCode::all() {
            assert_eq!(ErrorCode::from_wire_str(code.as_str()), Some(code));
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&'static str> = ErrorCode::all().iter().map(|c| c.as_str()).collect();
        let pre = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(pre, names.len(), "duplicate error code names");
    }

    #[test]
    fn names_are_screaming_snake() {
        for code in ErrorCode::all() {
            let n = code.as_str();
            assert!(
                n.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{n:?} not SCREAMING_SNAKE_CASE"
            );
        }
    }

    #[test]
    fn retryable_implies_transient() {
        for code in ErrorCode::all() {
            if code.retryable() {
                assert_eq!(code.default_kind(), ErrorKind::Transient, "{code:?}");
            }
        }
    }
}
