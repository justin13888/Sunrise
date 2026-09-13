//! Minting this device's own identity: its id, its two keypairs, and the
//! identity-signed cert that binds them to the account.
//!
//! The three functions are one step of one procedure, run whole on every path
//! that brings a device into an account — a founding vault, a vault admitted
//! by pairing, and a pre-ADR-0024 vault being re-certified under the account
//! identity. `device_id_from_pub` is also the check side of that step: every
//! load recomputes the id from the stored signing key and refuses a row where
//! the two disagree.

use super::{Identity, KeychainError};
use crate::config::Rng;
use sunrise_crypto::{
    derive_key, DeviceCert, DeviceCertInner, DeviceDhKeyPair, DeviceSigningKeyPair,
};
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
    let cert = DeviceCert::issue(body, &identity.signing)?;
    Ok(cert.to_cbor()?)
}
