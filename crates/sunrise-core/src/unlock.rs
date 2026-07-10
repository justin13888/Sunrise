//! Unlock material passed at `Core::open` time.

use sunrise_crypto::keys::VaultRootKey;

/// Three unlock modes per `docs/01-architecture/shared-core.md`.
#[derive(Debug)]
pub enum Unlock {
    /// User typed a passphrase; the caller derives the vault root via
    /// Argon2id with the per-device salt and passes the resulting 32-byte
    /// root here.
    Passphrase(VaultRootKey),
    /// Device has been previously paired; the OS keystore released the
    /// vault root.
    DevicePaired(VaultRootKey),
    /// Recovery code path: caller already restored identity keys; the
    /// next session will materialize a vault root via the standard path.
    RecoveryCode(VaultRootKey),
}

impl Unlock {
    /// Underlying vault root (consumes self, zeroizing on drop).
    #[must_use]
    pub fn into_root(self) -> VaultRootKey {
        match self {
            Self::Passphrase(k) | Self::DevicePaired(k) | Self::RecoveryCode(k) => k,
        }
    }
}
