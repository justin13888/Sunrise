//! Wrapping the keychain's long-lived secrets under the vault root.
//!
//! Everything here is a pure function of a [`VaultRootKey`] and an AAD string,
//! and together they are the only code in the crate that touches
//! `sunrise_crypto`'s symmetric wrap surface for *keychain* secrets — the
//! device signing and DH seeds and the account identity's two halves. The AAD
//! builders sit with them because the AAD is not decoration: each prefix binds
//! a wrapped secret to the id it belongs to, so a device row's signing blob
//! cannot be opened as its DH blob and neither can be opened against another
//! device's id. Stream keys are wrapped by `sunrise_crypto::wrap_stream_key`
//! instead and so live in [`super::rows`] with the row that stores them.

use super::rows::AccountIdentityRow;
use super::{Identity, KeychainError};
use crate::config::Rng;
use sunrise_crypto::aead::{aead_open_xchacha, aead_seal_xchacha, AEAD_NONCE_LEN};
use sunrise_crypto::{
    identity_id_from_pub, DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair,
    IdentitySigningKeyPair, VaultRootKey,
};
use zeroize::Zeroize;

/// AAD prefix binding the wrapped device signing secret to its device id.
const LOCAL_IDENTITY_AAD_PREFIX: &[u8] = b"sunrise.local_identity.v1";
/// AAD prefix binding the wrapped device DH secret to its device id.
const LOCAL_DH_AAD_PREFIX: &[u8] = b"sunrise.local_identity.dh.v1";
/// AAD prefix binding the wrapped identity secrets to the identity id.
const IDENTITY_AAD_PREFIX: &[u8] = b"sunrise.local_identity.identity.v1";
/// Length of a wrapped 32-byte secret: nonce (24) + ciphertext (32) + tag (16).
const WRAPPED_SECRET_LEN: usize = AEAD_NONCE_LEN + 32 + 16;

pub(super) fn device_aad(device_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(LOCAL_IDENTITY_AAD_PREFIX, device_id)
}

pub(super) fn device_dh_aad(device_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(LOCAL_DH_AAD_PREFIX, device_id)
}

fn identity_aad(identity_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(IDENTITY_AAD_PREFIX, identity_id)
}

fn prefixed_aad(prefix: &[u8], id: &[u8; 16]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(prefix.len() + 16);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(id);
    aad
}

pub(super) fn wrap_secret(
    vault_root: &VaultRootKey,
    secret: &[u8; 32],
    aad: &[u8],
    rng: &dyn Rng,
) -> Vec<u8> {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    // A 32-byte plaintext never trips the only AEAD error path.
    let ct = aead_seal_xchacha(vault_root.as_bytes(), &nonce, secret, aad)
        .expect("aead seal of 32-byte secret");
    let mut out = Vec::with_capacity(AEAD_NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

pub(super) fn unwrap_secret(
    vault_root: &VaultRootKey,
    wrapped: &[u8],
    aad: &[u8],
) -> Result<[u8; 32], KeychainError> {
    if wrapped.len() != WRAPPED_SECRET_LEN {
        return Err(KeychainError::WrappedLen);
    }
    let (nonce, ct) = wrapped.split_at(AEAD_NONCE_LEN);
    let mut nonce_arr = [0u8; AEAD_NONCE_LEN];
    nonce_arr.copy_from_slice(nonce);
    let pt = aead_open_xchacha(vault_root.as_bytes(), &nonce_arr, ct, aad)
        .map_err(|_| KeychainError::VaultRootMismatch)?;
    pt.as_slice()
        .try_into()
        .map_err(|_| KeychainError::WrappedLen)
}

pub(super) fn wrap_device_secrets(
    vault_root: &VaultRootKey,
    signing: &DeviceSigningKeyPair,
    dh: &DeviceDhKeyPair,
    device_id: &[u8; 16],
    rng: &dyn Rng,
) -> (Vec<u8>, Vec<u8>) {
    let mut s = signing.secret_bytes();
    let wrapped_signing = wrap_secret(vault_root, &s, &device_aad(device_id), rng);
    s.zeroize();
    let mut d = dh.secret_bytes();
    let wrapped_dh = wrap_secret(vault_root, &d, &device_dh_aad(device_id), rng);
    d.zeroize();
    (wrapped_signing, wrapped_dh)
}

pub(super) fn wrap_identity(
    vault_root: &VaultRootKey,
    identity: &Identity,
    rng: &dyn Rng,
) -> (Vec<u8>, Vec<u8>) {
    let aad = identity_aad(&identity.identity_id);
    let mut s = identity.signing.secret_bytes();
    let wrapped_s = wrap_secret(vault_root, &s, &aad, rng);
    s.zeroize();
    // `None` stores a zero-length blob rather than a wrapped zero key: the
    // column has to distinguish "this device never had the secret" from "the
    // secret happens to be all zeros", and an empty blob cannot be mistaken
    // for a nonce-prefixed ciphertext by `unwrap_secret`.
    let wrapped_d = match identity.dh_secret.as_ref() {
        Some(dh) => {
            let mut d = dh.secret_bytes();
            let w = wrap_secret(vault_root, &d, &aad, rng);
            d.zeroize();
            w
        }
        None => Vec::new(),
    };
    (wrapped_s, wrapped_d)
}

/// Rebuild the account [`Identity`] from its stored row.
///
/// `this_device` is the device id of the vault doing the unwrapping, or `None`
/// when there is not one yet. `ID_D_priv` is loaded **only** when the row names
/// that same device as the identity's minter, and this is the runtime half of
/// what migration 0019 does at rest: the column and the guard have to agree,
/// because a row that survived the migration's `UPDATE` by some path nobody
/// anticipated must still not hand a paired device the account's unwrapping
/// key. A row whose blob is present but whose `minted_by_device_id` says
/// otherwise is treated exactly as an absent blob, which is the state a paired
/// device has had since #86.
pub(super) fn unwrap_identity(
    vault_root: &VaultRootKey,
    row: &AccountIdentityRow,
    this_device: Option<&[u8; 16]>,
) -> Result<Identity, KeychainError> {
    let aad = identity_aad(&row.identity_id);
    let mut s = unwrap_secret(vault_root, &row.id_s_priv_wrapped, &aad)?;
    let signing = IdentitySigningKeyPair::from_secret_bytes(&s);
    s.zeroize();
    let minted_here = match (row.minted_by_device_id.as_ref(), this_device) {
        (Some(minter), Some(me)) => minter == me,
        _ => false,
    };
    let dh_secret = if row.id_d_priv_wrapped.is_empty() || !minted_here {
        None
    } else {
        let mut d = unwrap_secret(vault_root, &row.id_d_priv_wrapped, &aad)?;
        let dh = IdentityDhKeyPair::from_secret_bytes(d);
        d.zeroize();
        // Only checkable when the secret is here. On a paired device the
        // stored `id_d_pub` is what the payload asserted and the Noise channel
        // is what stands behind it; see `sunrise_pairing::payload`.
        if dh.public_bytes() != row.id_d_pub {
            return Err(KeychainError::IdentityIdMismatch);
        }
        Some(dh)
    };
    let dh_pub: [u8; 32] = row
        .id_d_pub
        .as_slice()
        .try_into()
        .map_err(|_| KeychainError::IdentityIdMismatch)?;
    if signing.public_bytes() != row.id_s_pub
        || identity_id_from_pub(&signing.public_bytes()) != row.identity_id
    {
        return Err(KeychainError::IdentityIdMismatch);
    }
    Ok(Identity {
        identity_id: row.identity_id,
        signing,
        dh_pub,
        dh_secret,
        created_at_ms: row.created_at_ms,
    })
}
