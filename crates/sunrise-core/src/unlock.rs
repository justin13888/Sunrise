//! Unlock material passed at `Core::open` time.

use sunrise_crypto::keys::VaultRootKey;
use sunrise_pairing::PairingPayload;

/// The unlock modes per `docs/01-architecture/shared-core.md`.
///
/// One of them now carries more than a vault root. Since ADR-0024 the root
/// alone no longer reconstructs the key schedule — Stream keys are random, not
/// derived — so a device arriving by pairing has to be handed the account
/// identity, its own sponsor-issued cert and every Stream key as well, and that
/// is what [`Self::DevicePaired`]'s extra field is.
///
/// # The recovery variant, and why it is gone
///
/// There was a third, `RecoveryCode`, carrying a `PairingPayload` on the
/// grounds that "the identity material the recovery blob carried" was the same
/// shape pairing used. Nothing ever constructed it — the recovery path is
/// `sunrise_onboarding::recovery`, which works in
/// `sunrise_crypto::recovery::RecoveryPayload` and never reached this enum.
///
/// It could not survive #105 in any case. A recovery restores `ID_S_priv` and
/// `ID_D_priv` — that is the whole point of the blob, and the *only* way an
/// account gets its signing key back after losing every device — and a
/// `PairingPayload` now deliberately carries neither. Keeping the variant would
/// have meant a mode that claims to restore an identity and installs a device
/// that cannot speak for it. Restoring from a recovery code needs its own
/// carrier when it is built; borrowing pairing's is what made it look done.
#[derive(Debug)]
pub enum Unlock {
    /// User typed a passphrase; the caller derives the vault root via
    /// Argon2id with the per-device salt and passes the resulting 32-byte
    /// root here.
    Passphrase(VaultRootKey),
    /// Device has been previously paired, or is being paired right now.
    ///
    /// `paired` is `None` for the ordinary case — an already-established device
    /// whose OS keystore released the root, and whose identity is already in
    /// its vault. It is `Some` exactly once, on the first open of a device
    /// being added, and carries the account's public identity, the device keys
    /// this device minted for its pairing request, the cert its sponsor issued
    /// over them, and every Stream key the sponsor held.
    DevicePaired {
        /// The vault root.
        root: VaultRootKey,
        /// The assembled pairing material, on a device's very first open.
        paired: Option<Box<PairingPayload>>,
    },
}

impl Unlock {
    /// Underlying vault root (consumes self, zeroizing the rest on drop).
    #[must_use]
    pub fn into_root(self) -> VaultRootKey {
        self.into_parts().0
    }

    /// The vault root and, where the mode carries one, the identity material
    /// the keychain should seed itself from.
    #[must_use]
    pub fn into_parts(self) -> (VaultRootKey, Option<Box<PairingPayload>>) {
        match self {
            Self::Passphrase(k) => (k, None),
            Self::DevicePaired { root, paired } => (root, paired),
        }
    }
}
