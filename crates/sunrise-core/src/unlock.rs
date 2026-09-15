//! Unlock material passed at `Core::open` time.

use sunrise_crypto::keys::VaultRootKey;
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_pairing::PairingPayload;

/// What a vault being created should seed its account identity from.
///
/// Split out of [`Unlock`] because the two ways a vault can be handed an
/// identity are **not** the same thing, and collapsing them is what made
/// [`Unlock::RecoveryCode`] unusable: it carried a [`PairingPayload`], and
/// since `#76` that type does not carry `ID_D_priv` at all. A vault seeded
/// from one therefore came up with `dh_secret: None` — unable to open a single
/// identity-addressed `key_envelope`, which is the only thing a recovery is
/// for. See `docs/03-crypto/recovery.md` §Recovery flow step 8.
pub enum IdentitySeed {
    /// Nothing is handed over. The vault mints its own account identity on
    /// first open, or loads the one already on disk.
    Own,
    /// A pairing payload, from a sibling device over the Noise channel.
    ///
    /// Carries `ID_S_priv` and `ID_D_pub` and deliberately **not** `ID_D_priv`
    /// — the asymmetry that lets a revocation bound a device's reads.
    Paired(Box<PairingPayload>),
    /// An opened recovery blob.
    ///
    /// Carries `ID_D_priv`, because that is the whole point: it is the key
    /// every `key_envelope`'s identity copy is sealed to, and a recovering
    /// device has no sibling to be handed a Stream key by.
    Recovered(Box<RecoveryPayload>),
}

impl std::fmt::Debug for IdentitySeed {
    /// Names the shape and nothing else, for the reason
    /// [`PairingPayload`]'s own `Debug` does: the recovered arm holds the
    /// account's private keys in the clear, and a derived `Debug` would put
    /// them in whatever log the caller writes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Own => f.write_str("IdentitySeed::Own"),
            Self::Paired(_) => f.write_str("IdentitySeed::Paired(..)"),
            Self::Recovered(_) => f.write_str("IdentitySeed::Recovered(..)"),
        }
    }
}

/// Three unlock modes per `docs/01-architecture/shared-core.md`.
///
/// Two of them now carry more than a vault root. Since ADR-0024 the root alone
/// no longer reconstructs the key schedule — Stream keys are random, not
/// derived — so a device arriving by pairing or by recovery has to be handed
/// the account identity as well, and that is what the extra field is.
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
    ///
    /// `root` is a **fresh** root this device mints. The blob carries the
    /// account identity and never the vault root, which keys the local
    /// database and nothing else (`docs/03-crypto/recovery.md` §Recovery flow
    /// step 8); there is no surviving device to be handed one by, so a
    /// recovering device makes its own.
    RecoveryCode {
        /// The vault root this recovering device mints for itself.
        root: VaultRootKey,
        /// The identity material the recovery blob carried, `ID_D_priv`
        /// included.
        identity: Box<RecoveryPayload>,
    },
}

impl std::fmt::Debug for Unlock {
    /// Names the mode and nothing else. Every arm holds key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passphrase(_) => f.write_str("Unlock::Passphrase(..)"),
            Self::DevicePaired { paired, .. } => f
                .debug_struct("Unlock::DevicePaired")
                .field("paired", &paired.is_some())
                .finish_non_exhaustive(),
            Self::RecoveryCode { .. } => f.write_str("Unlock::RecoveryCode { .. }"),
        }
    }
}

impl Unlock {
    /// Underlying vault root (consumes self, zeroizing the rest on drop).
    #[must_use]
    pub fn into_root(self) -> VaultRootKey {
        self.into_parts().0
    }

    /// The vault root and the identity material the keychain should seed
    /// itself from.
    #[must_use]
    pub fn into_parts(self) -> (VaultRootKey, IdentitySeed) {
        match self {
            Self::Passphrase(k) => (k, IdentitySeed::Own),
            Self::DevicePaired { root, paired } => {
                (root, paired.map_or(IdentitySeed::Own, IdentitySeed::Paired))
            }
            Self::RecoveryCode { root, identity } => (root, IdentitySeed::Recovered(identity)),
        }
    }
}
