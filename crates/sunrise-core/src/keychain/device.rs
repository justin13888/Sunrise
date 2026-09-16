//! Minting this device's own identity: its id, its two keypairs, and the
//! identity-signed cert that binds them to the account.
//!
//! The three functions are one step of one procedure, run whole on every path
//! that brings a device into an account — a founding vault, a vault admitted
//! by pairing, and a pre-ADR-0024 vault being re-certified under the account
//! identity. `device_id_from_pub` is also the check side of that step: every
//! load recomputes the id from the stored signing key and refuses a row where
//! the two disagree.
//!
//! Since `#105` there are *two* answers to "where does this device's cert come
//! from", and both live here beside the minting they share: a founding or
//! recovering vault holds `ID_S_priv` and names itself
//! ([`mint_own_device_keys`]), and a vault admitted by pairing holds no signing
//! key at all and adopts the cert its sponsor issued
//! ([`adopt_granted_device_keys`]). [`DeviceMaterial`] is what both produce.

use super::{Identity, KeychainError};
use crate::config::Rng;
use sunrise_crypto::{
    derive_key, DeviceCert, DeviceCertInner, DeviceDhKeyPair, DeviceSigningKeyPair,
};
use sunrise_pairing::PairingPayload;
use zeroize::Zeroize;

/// `device_id = BLAKE3.derive_key("sunrise.device_id.v1", D_S_pub)[..16]`.
pub(super) fn device_id_from_pub(d_s_pub: &[u8; 32]) -> [u8; 16] {
    let bytes = derive_key("sunrise.device_id.v1", d_s_pub, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

pub(super) fn mint_device_keys(rng: &dyn Rng) -> (DeviceSigningKeyPair, DeviceDhKeyPair, [u8; 16]) {
    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);
    let signing = DeviceSigningKeyPair::from_secret_bytes(&seed);
    seed.zeroize();
    let mut dh_seed = [0u8; 32];
    rng.fill_bytes(&mut dh_seed);
    let dh = DeviceDhKeyPair::from_secret_bytes(dh_seed);
    dh_seed.zeroize();
    let device_id = device_id_from_pub(&signing.public_bytes());
    (signing, dh, device_id)
}

pub(super) fn issue_cert(
    identity: &Identity,
    device_id: [u8; 16],
    signing: &DeviceSigningKeyPair,
    dh: &DeviceDhKeyPair,
    now_ms: u64,
    nickname: &str,
    platform: &str,
) -> Result<Vec<u8>, KeychainError> {
    let body = DeviceCertInner {
        v: 1,
        device_id,
        d_s_pub: signing.public_bytes(),
        d_d_pub: dh.public_bytes(),
        identity_id: identity.identity_id,
        created_at_ms: now_ms,
        nickname: nickname.to_string(),
        platform: platform.to_string(),
    };
    // Only ever reached on the founding and legacy-adoption paths, both of
    // which minted `ID_S` moments earlier. A paired device gets its cert from
    // its sponsor instead — see `Keychain::adopt_granted_device_keys`.
    let cert = DeviceCert::issue(body, identity.signing.require()?)?;
    Ok(cert.to_cbor()?)
}

/// This device's own keys and the cert that names them, however it got them.
///
/// A struct rather than a six-tuple because the two arms of `Keychain::create`
/// produce it by completely different routes — minting and self-signing on a
/// founding vault, adopting a sponsor's grant on a paired one — and a tuple
/// whose fifth and sixth elements are both `String` is a tuple whose two
/// `String`s can be swapped without the compiler noticing.
pub(super) struct DeviceMaterial {
    pub(super) signing: DeviceSigningKeyPair,
    pub(super) device_dh: DeviceDhKeyPair,
    pub(super) device_id: [u8; 16],
    /// Canonical CBOR `DeviceCert`, signed by the account identity.
    pub(super) cert_blob: Vec<u8>,
    /// Read out of `cert_blob` on the paired arm, so the `devices` row and the
    /// cert cannot disagree.
    pub(super) nickname: String,
    pub(super) platform: String,
}

/// Adopt the device keys a joiner minted and the cert its sponsor issued
/// for them.
///
/// The counterpart of [`mint_device_keys`], and the reason `create` no
/// longer always mints. A paired device holds no `ID_S_priv`, so it cannot
/// sign a cert for keys it mints here; the keys it uses are the ones it
/// already published in a `PairingRequest`, and the cert is the one the
/// sponsor signed over exactly those keys.
///
/// Everything is re-derived rather than trusted, because the payload
/// reached this process across a seam — a `paired_bundle` from Swift, or a
/// file on disk — and the cert inside it is the account's statement about
/// this device:
///
/// - the cert parses, and verifies under the `ID_S_pub` this vault is about
///   to adopt. `verify_binding` also recomputes `identity_id` from that key,
///   so a cert issued by some other well-formed identity is refused.
/// - the `device_id` the cert names is the one `D_S_pub` derives, and that
///   `D_S_pub` is the public half of the secret in the payload. A device
///   that installed a cert naming keys it does not hold would sign every op
///   with a key no peer associates with it.
/// - `D_D_pub` likewise, because it is what every `key_envelope` addressed
///   to this device will be sealed to.
///
/// The nickname and platform come back out of the cert rather than from
/// `std::env::consts::OS` as the founding path's do: the cert is what the
/// account signed, and a `devices` row disagreeing with the cert beside it
/// is a row that will fail the next `verify_binding` a peer runs.
///
/// # Errors
/// [`KeychainError::BadGrantCert`] for any of the above.
pub(super) fn adopt_granted_device_keys(
    identity: &Identity,
    p: &PairingPayload,
) -> Result<DeviceMaterial, KeychainError> {
    let signing = DeviceSigningKeyPair::from_secret_bytes(&p.d_s_priv);
    let device_dh = DeviceDhKeyPair::from_secret_bytes(p.d_d_priv);
    let device_id = device_id_from_pub(&signing.public_bytes());

    let cert = DeviceCert::from_cbor(&p.device_cert)
        .map_err(|_| KeychainError::BadGrantCert("the granted cert does not decode"))?;
    cert.verify_binding(&identity.signing.public_bytes(), &identity.identity_id)
        .map_err(|_| {
            KeychainError::BadGrantCert("the granted cert is not signed by this account")
        })?;
    if cert.body.d_s_pub != signing.public_bytes() {
        return Err(KeychainError::BadGrantCert(
            "the granted cert names a signing key this device does not hold",
        ));
    }
    if cert.body.d_d_pub != device_dh.public_bytes() {
        return Err(KeychainError::BadGrantCert(
            "the granted cert names a DH key this device does not hold",
        ));
    }
    if cert.body.device_id != device_id {
        return Err(KeychainError::BadGrantCert(
            "the granted cert names a device id its own signing key does not derive",
        ));
    }
    Ok(DeviceMaterial {
        signing,
        device_dh,
        device_id,
        cert_blob: p.device_cert.clone(),
        nickname: cert.body.nickname,
        platform: cert.body.platform,
    })
}

/// Mint this device's own keys and self-issue its cert.
///
/// The founding arm, unchanged in substance: a vault that is creating the
/// account holds `ID_S_priv` and so can name itself. The counterpart of
/// [`adopt_granted_device_keys`], and split out beside it so the two
/// answers to "where does this device's cert come from" sit together.
///
/// # Errors
/// [`KeychainError::IdentitySigningKeyAbsent`] if this is somehow reached
/// on an identity that carries only a public half, and
/// [`KeychainError::Cert`] for a CBOR failure.
pub(super) fn mint_own_device_keys(
    identity: &Identity,
    rng: &dyn Rng,
    now_ms: u64,
) -> Result<DeviceMaterial, KeychainError> {
    let (signing, device_dh, device_id) = mint_device_keys(rng);
    let nickname = "sunrise-device".to_string();
    let platform = std::env::consts::OS.to_string();
    let cert_blob = issue_cert(
        identity, device_id, &signing, &device_dh, now_ms, &nickname, &platform,
    )?;
    Ok(DeviceMaterial {
        signing,
        device_dh,
        device_id,
        cert_blob,
        nickname,
        platform,
    })
}

#[cfg(test)]
mod frozen_domain {
    use sunrise_crypto_test_vectors::DEVICE_ID_VECTORS;

    /// `sunrise.device_id.v1`, anchored to frozen literals — again.
    ///
    /// The derivation is spelled out twice in the tree:
    /// `sunrise_crypto::device_id_from_pub` and [`super::device_id_from_pub`].
    /// They are separate constants in separate crates, and
    /// `sunrise-crypto/tests/frozen_domains.rs` anchoring the first says
    /// nothing about the second — which matters here more than there, because
    /// this copy is what every vault open recomputes and compares against the
    /// stored `local_identity.device_id`. A drift makes every existing vault
    /// fail `DeviceIdMismatch` at the door.
    #[test]
    fn device_id_vectors_hold() {
        for v in DEVICE_ID_VECTORS {
            assert_eq!(
                super::device_id_from_pub(&v.d_s_pub),
                v.device_id,
                "the core's sunrise.device_id.v1 derivation drifted"
            );
        }
    }
}
