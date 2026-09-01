//! Unlock material passed at `Core::open` time.

use sunrise_crypto::keys::VaultRootKey;
use sunrise_pairing::PairingPayload;

/// Three unlock modes per `docs/01-architecture/shared-core.md`.
///
/// Two of them now carry more than a vault root. Since ADR-0024 the root alone
/// no longer reconstructs the key schedule — Stream keys are random, not
/// derived — so a device arriving by pairing or by recovery has to be handed
/// the account identity as well, and that is what the extra field is.
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
    /// being added, and carries the identity keys and Stream keys the sending
    /// device sealed through the Noise channel.
    DevicePaired {
        /// The vault root.
        root: VaultRootKey,
        /// The pairing payload, on a device's very first open.
        paired: Option<Box<PairingPayload>>,
    },
    /// Recovery code path: the recovery blob has been unsealed and its identity
    /// keys are in hand.
    ///
    /// The identity is the whole point of this mode. It opens the
    /// identity-sealed half of every `key_envelope` in the op log, which is
    /// what makes a recovery restore *readable* content rather than a vault
    /// full of ciphertext — the dilemma `recovery.md` used to conclude was
    /// non-negotiable.
    RecoveryCode {
        /// The vault root.
        root: VaultRootKey,
        /// The identity material the recovery blob carried, in the same shape
        /// pairing uses.
        identity: Box<PairingPayload>,
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
            Self::RecoveryCode { root, identity } => (root, Some(identity)),
        }
    }
}
