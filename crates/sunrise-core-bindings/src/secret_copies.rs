//! The keychain migration's decision table, lowered onto UniFFI:
//! [`sunrise_client_core::secret_copies`]. The platform reads both keychains
//! and compares the bytes. This decides which copy survives and what is
//! written or deleted. No secret crosses: the inputs are facts about the two
//! copies.

use sunrise_client_core::secret_copies::{
    self, SecretCopies, SecretCopyPlan, SecretKind, VerifyPlan, WriteReadBack,
};

/// See [`sunrise_client_core::secret_copies::SecretKind`].
#[uniffi::remote(Enum)]
pub enum SecretKind {
    VaultRoot,
    RelayDeviceId,
    Credential,
}

/// See [`sunrise_client_core::secret_copies::SecretCopies`].
#[uniffi::remote(Enum)]
pub enum SecretCopies {
    Neither,
    DestinationOnly,
    SourceOnly,
    Identical,
    Differ {
        destination_expires_at_ms: Option<u64>,
        source_expires_at_ms: Option<u64>,
    },
}

/// See [`sunrise_client_core::secret_copies::SecretCopyPlan`].
#[uniffi::remote(Enum)]
pub enum SecretCopyPlan {
    Nothing,
    UseDestination,
    MoveSource,
    DeleteSource,
    Refuse,
}

/// See [`sunrise_client_core::secret_copies::WriteReadBack`].
#[uniffi::remote(Enum)]
pub enum WriteReadBack {
    Matches,
    Gone,
    Differs,
}

/// See [`sunrise_client_core::secret_copies::VerifyPlan`].
#[uniffi::remote(Enum)]
pub enum VerifyPlan {
    DeleteSource,
    FallBackToSource,
    Refuse,
}

/// What a keychain migration does with what its two reads found: refuse two
/// differing vault roots or device ids, and keep the credential copy that
/// expires later. See [`sunrise_client_core::secret_copies`].
#[uniffi::export]
#[must_use]
pub fn plan_secret_copies(kind: SecretKind, copies: SecretCopies) -> SecretCopyPlan {
    secret_copies::plan_secret_copies(kind, copies)
}

/// What a keychain migration does after writing the destination, from what
/// reading it back found.
#[uniffi::export]
#[must_use]
pub fn verify_secret_write(read_back: WriteReadBack) -> VerifyPlan {
    secret_copies::verify_write(read_back)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_export_lowers_the_client_layer_unchanged() {
        let stale_source = SecretCopies::Differ {
            destination_expires_at_ms: Some(2),
            source_expires_at_ms: Some(1),
        };
        assert_eq!(
            plan_secret_copies(SecretKind::Credential, stale_source),
            SecretCopyPlan::DeleteSource
        );
        assert_eq!(
            plan_secret_copies(SecretKind::VaultRoot, stale_source),
            SecretCopyPlan::Refuse
        );
        assert_eq!(
            verify_secret_write(WriteReadBack::Gone),
            VerifyPlan::FallBackToSource
        );
    }
}
