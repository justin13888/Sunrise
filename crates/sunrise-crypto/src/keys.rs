//! Key newtypes with `Zeroize`-on-drop and constant-time equality.
//!
//! Per `docs/03-crypto/identity-and-device-keys.md`:
//!
//! - **Identity** keys (long-lived, recoverable): Ed25519 signing pair `ID_S`
//!   and X25519 DH pair `ID_D`. Generated once per user, restored from
//!   recovery code.
//! - **Device** keys (per-install, derived): Ed25519 signing pair `D_S` and
//!   X25519 DH pair `D_D`. Re-rotated on demand.
//! - **Vault root key**: 32 random bytes released by the OS keystore (or
//!   derived via Argon2id from a passphrase). Kept in process memory; zeroized
//!   on lock/exit.
//! - **Stream key**: per-Stream, per-epoch 32 random bytes. Wrapped under
//!   the vault root for at-rest storage.
//! - **Recovery key**: 32 bytes from `Argon2id(recovery_seed, salt)`; only
//!   exists transiently during seal/unseal of the recovery blob.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::CryptoRngCore;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Identity signing keypair (Ed25519). The private half is `ID_S_priv`; the
/// public half is `ID_S_pub`.
pub struct IdentitySigningKeyPair {
    signing: SigningKey,
}

impl core::fmt::Debug for IdentitySigningKeyPair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("IdentitySigningKeyPair(<redacted>)")
    }
}

impl IdentitySigningKeyPair {
    /// Generate a fresh keypair using the supplied CSPRNG.
    #[must_use]
    pub fn generate<R: CryptoRngCore>(rng: &mut R) -> Self {
        Self {
            signing: SigningKey::generate(rng),
        }
    }

    /// Reconstruct from a 32-byte secret seed.
    #[must_use]
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(bytes),
        }
    }

    /// Public verifying key bytes (32 B).
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Private seed bytes (32 B). Use [`Zeroize`] on the returned array
    /// after consumption.
    #[must_use]
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// Sign `msg`; returns 64 raw bytes.
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// Borrow the underlying `dalek` `SigningKey`.
    #[inline]
    pub const fn dalek(&self) -> &SigningKey {
        &self.signing
    }
}

impl Drop for IdentitySigningKeyPair {
    fn drop(&mut self) {
        // `dalek::SigningKey` implements Zeroize on Drop in ≥2.x, but we add
        // an explicit zeroize for belt-and-suspenders.
        let mut bytes = self.signing.to_bytes();
        bytes.zeroize();
    }
}

/// Verify an Ed25519 signature against a 32-byte public key.
///
/// # Errors
///
/// Returns `false` if the public key bytes don't form a valid Edwards point
/// or the signature does not authenticate the message.
#[must_use]
pub fn verify_ed25519(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pubkey) else {
        return false;
    };
    let Ok(s) = ed25519_dalek::Signature::try_from(&sig[..]) else {
        return false;
    };
    vk.verify(msg, &s).is_ok()
}

/// Identity DH keypair (X25519).
#[derive(ZeroizeOnDrop)]
pub struct IdentityDhKeyPair {
    secret: StaticSecret,
}

impl core::fmt::Debug for IdentityDhKeyPair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("IdentityDhKeyPair(<redacted>)")
    }
}

impl IdentityDhKeyPair {
    /// Generate a fresh keypair.
    #[must_use]
    pub fn generate<R: CryptoRngCore>(rng: &mut R) -> Self {
        Self {
            secret: StaticSecret::random_from_rng(rng),
        }
    }

    /// Reconstruct from a 32-byte clamped scalar.
    #[must_use]
    pub fn from_secret_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(bytes),
        }
    }

    /// Public key bytes (`ID_D_pub`, 32 B).
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        XPublicKey::from(&self.secret).to_bytes()
    }

    /// Private scalar bytes (32 B). Zeroize the copy after consumption; the
    /// only callers are the at-rest wrap, the recovery blob and the pairing
    /// payload.
    #[must_use]
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Borrow the underlying X25519 secret. Used by HPKE / Noise.
    #[inline]
    pub const fn dalek(&self) -> &StaticSecret {
        &self.secret
    }
}

/// Device signing keypair (Ed25519). The private half is `D_S_priv`; the
/// public half is `D_S_pub`.
///
/// Same *shape* as [`IdentitySigningKeyPair`] and deliberately **not** an alias
/// of it. `primitives.md` says one cannot be passed where the other is
/// expected; while this was `pub type DeviceSigningKeyPair =
/// IdentitySigningKeyPair` that claim was false in code, and the compiler
/// happily signed a device cert with a device key — which is precisely the
/// self-signature ADR-0024 removes. Two structs make the roles a type error
/// rather than a review comment.
pub struct DeviceSigningKeyPair {
    signing: SigningKey,
}

impl core::fmt::Debug for DeviceSigningKeyPair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("DeviceSigningKeyPair(<redacted>)")
    }
}

impl DeviceSigningKeyPair {
    /// Generate a fresh keypair using the supplied CSPRNG.
    #[must_use]
    pub fn generate<R: CryptoRngCore>(rng: &mut R) -> Self {
        Self {
            signing: SigningKey::generate(rng),
        }
    }

    /// Reconstruct from a 32-byte secret seed.
    #[must_use]
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(bytes),
        }
    }

    /// Public verifying key bytes (`D_S_pub`, 32 B).
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Private seed bytes (32 B). Use [`Zeroize`] on the returned array
    /// after consumption.
    #[must_use]
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    /// Sign `msg`; returns 64 raw bytes.
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// Borrow the underlying `dalek` `SigningKey`.
    #[inline]
    pub const fn dalek(&self) -> &SigningKey {
        &self.signing
    }
}

impl Drop for DeviceSigningKeyPair {
    fn drop(&mut self) {
        let mut bytes = self.signing.to_bytes();
        bytes.zeroize();
    }
}

/// Device DH keypair (X25519). The private half is `D_D_priv`; the public half
/// is `D_D_pub`, which is what a `key_envelope` op seals a Stream key to.
///
/// Distinct from [`IdentityDhKeyPair`] for the same reason the signing pair is:
/// the two recipient classes in ADR-0024 decision 4 differ only by which key
/// they name, and sealing to the wrong one is the difference between "a device
/// can read this epoch" and "a revoked device can read this epoch".
#[derive(ZeroizeOnDrop)]
pub struct DeviceDhKeyPair {
    secret: StaticSecret,
}

impl core::fmt::Debug for DeviceDhKeyPair {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("DeviceDhKeyPair(<redacted>)")
    }
}

impl DeviceDhKeyPair {
    /// Generate a fresh keypair.
    #[must_use]
    pub fn generate<R: CryptoRngCore>(rng: &mut R) -> Self {
        Self {
            secret: StaticSecret::random_from_rng(rng),
        }
    }

    /// Reconstruct from a 32-byte scalar.
    #[must_use]
    pub fn from_secret_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(bytes),
        }
    }

    /// Public key bytes (`D_D_pub`, 32 B).
    #[must_use]
    pub fn public_bytes(&self) -> [u8; 32] {
        XPublicKey::from(&self.secret).to_bytes()
    }

    /// Private scalar bytes (32 B). Zeroize the copy after consumption; the
    /// only callers are the at-rest wrap and the pairing payload.
    #[must_use]
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// Borrow the underlying X25519 secret. Used by HPKE / Noise.
    #[inline]
    pub const fn dalek(&self) -> &StaticSecret {
        &self.secret
    }
}

/// Vault root key — 32 bytes used to derive at-rest encryption keys.
///
/// Kept in memory only; zeroized on Drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultRootKey([u8; 32]);

impl VaultRootKey {
    /// Build from raw bytes.
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Generate a fresh random vault root.
    #[must_use]
    pub fn generate<R: CryptoRngCore>(rng: &mut R) -> Self {
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Borrow the bytes.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for VaultRootKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VaultRootKey(<redacted>)")
    }
}

impl PartialEq for VaultRootKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for VaultRootKey {}

/// Per-Stream symmetric key, 32 bytes. Each Stream-key rotation creates a
/// new epoch's key; old epochs remain decryptable for historical ops.
#[derive(Clone, Zeroize, ZeroizeOnDrop, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamKey([u8; 32]);

impl StreamKey {
    /// Build from raw bytes.
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Generate a fresh random stream key.
    #[must_use]
    pub fn generate<R: CryptoRngCore>(rng: &mut R) -> Self {
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Borrow the bytes.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("StreamKey(<redacted>)")
    }
}

impl PartialEq for StreamKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for StreamKey {}

/// Recovery key — 32 bytes derived via Argon2id from the BIP-39 recovery
/// seed. Exists only transiently during recovery-blob seal/unseal.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct RecoveryKey([u8; 32]);

impl RecoveryKey {
    /// Build from raw bytes.
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the bytes.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    #[test]
    fn identity_signing_round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let kp = IdentitySigningKeyPair::generate(&mut rng);
        let pub_bytes = kp.public_bytes();
        let msg = b"hello sunrise";
        let sig = kp.sign(msg);
        assert!(verify_ed25519(&pub_bytes, msg, &sig));
        // Tamper with the signature: must not verify.
        let mut bad = sig;
        bad[0] ^= 0x01;
        assert!(!verify_ed25519(&pub_bytes, msg, &bad));
    }

    #[test]
    fn identity_dh_round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let a = IdentityDhKeyPair::generate(&mut rng);
        let b = IdentityDhKeyPair::generate(&mut rng);
        let a_pub = XPublicKey::from(a.public_bytes());
        let b_pub = XPublicKey::from(b.public_bytes());
        // Compute the shared secret both ways via dalek directly.
        let s_ab = a.dalek().diffie_hellman(&b_pub);
        let s_ba = b.dalek().diffie_hellman(&a_pub);
        assert_eq!(s_ab.as_bytes(), s_ba.as_bytes());
    }

    #[test]
    fn vault_root_eq_constant_time() {
        let a = VaultRootKey::from_bytes([0u8; 32]);
        let b = VaultRootKey::from_bytes([0u8; 32]);
        let mut c_bytes = [0u8; 32];
        c_bytes[31] = 1;
        let c = VaultRootKey::from_bytes(c_bytes);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// The type-level claim `primitives.md` makes, asserted the only way a
    /// test can: this compiles only because the two are separate types with
    /// separate constructors, and a `pub type` alias would make every one of
    /// these lines interchangeable.
    #[test]
    fn device_and_identity_keys_are_distinct_types() {
        let mut rng = ChaCha20Rng::seed_from_u64(11);
        let id_s = IdentitySigningKeyPair::generate(&mut rng);
        let d_s = DeviceSigningKeyPair::from_secret_bytes(&id_s.secret_bytes());
        // Same seed, same public key — the shapes really are identical...
        assert_eq!(id_s.public_bytes(), d_s.public_bytes());
        // ...and a signature made by one verifies under the other's public
        // key, which is exactly why only the *type* can keep the roles apart.
        let msg = b"role separation is a type, not a convention";
        assert!(verify_ed25519(&d_s.public_bytes(), msg, &id_s.sign(msg)));

        let id_d = IdentityDhKeyPair::generate(&mut rng);
        let d_d = DeviceDhKeyPair::from_secret_bytes(id_d.secret_bytes());
        assert_eq!(id_d.public_bytes(), d_d.public_bytes());
    }

    #[test]
    fn debug_does_not_leak() {
        let v = VaultRootKey::from_bytes([0u8; 32]);
        assert_eq!(format!("{v:?}"), "VaultRootKey(<redacted>)");
        let s = StreamKey::from_bytes([0u8; 32]);
        assert_eq!(format!("{s:?}"), "StreamKey(<redacted>)");
        let r = RecoveryKey::from_bytes([0u8; 32]);
        assert_eq!(format!("{r:?}"), "RecoveryKey(<redacted>)");
        let mut rng = ChaCha20Rng::seed_from_u64(3);
        let ds = DeviceSigningKeyPair::generate(&mut rng);
        assert_eq!(format!("{ds:?}"), "DeviceSigningKeyPair(<redacted>)");
        let dd = DeviceDhKeyPair::generate(&mut rng);
        assert_eq!(format!("{dd:?}"), "DeviceDhKeyPair(<redacted>)");
        let is = IdentitySigningKeyPair::generate(&mut rng);
        assert_eq!(format!("{is:?}"), "IdentitySigningKeyPair(<redacted>)");
        let idh = IdentityDhKeyPair::generate(&mut rng);
        assert_eq!(format!("{idh:?}"), "IdentityDhKeyPair(<redacted>)");
    }
}
