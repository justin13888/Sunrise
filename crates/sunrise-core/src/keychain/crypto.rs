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
//!
//! The rule has one secret per domain, and that is checked rather than assumed:
//! `each_wrapped_secret_has_its_own_domain` below asserts every prefix in this
//! file is distinct, so adding a fifth wrapped secret that reuses a fourth's
//! AAD fails the build's tests rather than shipping.

use super::rows::AccountIdentityRow;
use super::{Identity, IdentitySigningKey, KeychainError};
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
/// AAD prefix binding the wrapped identity **signing** secret (`ID_S_priv`) to
/// the identity id.
///
/// `sign.v2`, not `v1`. One prefix covered both halves of the identity until
/// this constant existed, which is the rule at the top of this module broken in
/// the one place it matters most: `ID_S_priv` signs for the account and
/// `ID_D_priv` unwraps for it, they are different capabilities, and a single
/// AAD made the two blobs interchangeable to the AEAD. A storage bug, a swapped
/// `UPDATE`, or a hostile write that exchanged the two columns produced two
/// blobs that both opened. The `v2` suffix moves with the split so a blob
/// written under the shared domain cannot be opened by this build at all.
const IDENTITY_SIGNING_AAD_PREFIX: &[u8] = b"sunrise.local_identity.identity.sign.v2";
/// AAD prefix binding the wrapped identity **DH** secret (`ID_D_priv`) to the
/// identity id.
///
/// Distinct from [`LOCAL_DH_AAD_PREFIX`], which binds a *device*'s DH seed:
/// these are two different keys with two different owners and neither blob may
/// open under the other's domain.
const IDENTITY_DH_AAD_PREFIX: &[u8] = b"sunrise.local_identity.identity.dh.v2";
/// Length of a wrapped 32-byte secret: nonce (24) + ciphertext (32) + tag (16).
const WRAPPED_SECRET_LEN: usize = AEAD_NONCE_LEN + 32 + 16;

pub(super) fn device_aad(device_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(LOCAL_IDENTITY_AAD_PREFIX, device_id)
}

pub(super) fn device_dh_aad(device_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(LOCAL_DH_AAD_PREFIX, device_id)
}

fn identity_signing_aad(identity_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(IDENTITY_SIGNING_AAD_PREFIX, identity_id)
}

fn identity_dh_aad(identity_id: &[u8; 16]) -> Vec<u8> {
    prefixed_aad(IDENTITY_DH_AAD_PREFIX, identity_id)
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

/// Wrap the account identity's two secret halves, **each under its own AAD**.
///
/// `(id_s_priv_wrapped, id_d_priv_wrapped)`, the two columns of the `identity`
/// row. The AADs differ because the secrets differ: see
/// [`IDENTITY_SIGNING_AAD_PREFIX`].
pub(super) fn wrap_identity(
    vault_root: &VaultRootKey,
    identity: &Identity,
    rng: &dyn Rng,
) -> (Vec<u8>, Vec<u8>) {
    let id = identity.identity_id;
    // An empty blob when the secret is not here, on the same rule and for the
    // same reason as `ID_D_priv` below. Since `#105` a device admitted by
    // pairing holds `ID_S_pub` and nothing else, so its `identity` row has to
    // be able to say so; wrapping a placeholder would make a row that unwraps
    // to a key the account does not have.
    let wrapped_s = match identity.signing {
        IdentitySigningKey::Held(ref kp) => {
            let mut s = kp.secret_bytes();
            let w = wrap_secret(vault_root, &s, &identity_signing_aad(&id), rng);
            s.zeroize();
            w
        }
        IdentitySigningKey::PublicOnly(_) => Vec::new(),
    };
    // `None` stores a zero-length blob rather than a wrapped zero key: the
    // column has to distinguish "this device never had the secret" from "the
    // secret happens to be all zeros", and an empty blob cannot be mistaken
    // for a nonce-prefixed ciphertext by `unwrap_secret`.
    let wrapped_d = match identity.dh_secret.as_ref() {
        Some(dh) => {
            let mut d = dh.secret_bytes();
            let w = wrap_secret(vault_root, &d, &identity_dh_aad(&id), rng);
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
    // An empty `id_s_priv_wrapped` is a device admitted by pairing: it holds
    // `ID_S_pub` and cannot issue a `DeviceCert` for anything (`#105`). The
    // column is the at-rest half of what `IdentitySigningKey` is at runtime,
    // and the two have to agree or a restart would hand the device a capability
    // its pairing withheld.
    let signing = if row.id_s_priv_wrapped.is_empty() {
        IdentitySigningKey::PublicOnly(row.id_s_pub)
    } else {
        let mut s = unwrap_secret(
            vault_root,
            &row.id_s_priv_wrapped,
            &identity_signing_aad(&row.identity_id),
        )?;
        let kp = IdentitySigningKeyPair::from_secret_bytes(&s);
        s.zeroize();
        IdentitySigningKey::Held(kp)
    };
    let minted_here = match (row.minted_by_device_id.as_ref(), this_device) {
        (Some(minter), Some(me)) => minter == me,
        _ => false,
    };
    let dh_secret = if row.id_d_priv_wrapped.is_empty() || !minted_here {
        None
    } else {
        let mut d = unwrap_secret(
            vault_root,
            &row.id_d_priv_wrapped,
            &identity_dh_aad(&row.identity_id),
        )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::VaultRootKey;

    /// A deterministic stand-in for the injected RNG. The nonce's *value* is
    /// irrelevant to every assertion here — what is under test is the AAD — and
    /// a fixed one keeps the failure message stable.
    #[derive(Debug)]
    struct FixedRng(u8);

    impl Rng for FixedRng {
        fn fill_bytes(&self, dest: &mut [u8]) {
            dest.fill(self.0);
        }
    }

    fn root() -> VaultRootKey {
        VaultRootKey::from_bytes([0x5au8; 32])
    }

    const ID: [u8; 16] = [0x11; 16];
    const DEVICE: [u8; 16] = [0x11; 16];

    /// The four AADs, anchored to frozen literals.
    ///
    /// [`each_wrapped_secret_has_its_own_domain`] above proves the four differ
    /// from *each other*, which is a property this file can satisfy while
    /// every one of them has drifted away from what is on disk. An AAD is not
    /// stored beside the ciphertext it authenticates, so a build that renamed
    /// a prefix — or swapped `prefix || id` for `id || prefix` — wraps and
    /// unwraps its own vault perfectly and cannot open anybody else's.
    ///
    /// The expectations are literals in `sunrise-crypto-test-vectors`, a crate
    /// with no dependencies at all, so a coordinated edit to a constant here
    /// and to the test beside it cannot pass.
    ///
    /// `WRAPPED_SECRET_LEN` is asserted here for the same reason and in the
    /// same place: it is the length of every `local_identity` and `identity`
    /// blob ever written, [`unwrap_secret`] refuses anything else, and nothing
    /// else in the tree wrote the number down.
    #[test]
    fn the_wrapped_secret_aads_are_byte_exact() {
        use sunrise_crypto_test_vectors::at_rest::keychain_aad as k;

        assert_eq!(
            device_aad(&k::DEVICE_ID),
            k::DEVICE_SIGNING,
            "the sunrise.local_identity.v1 AAD drifted"
        );
        assert_eq!(
            device_dh_aad(&k::DEVICE_ID),
            k::DEVICE_DH,
            "the sunrise.local_identity.dh.v1 AAD drifted"
        );
        assert_eq!(
            identity_signing_aad(&k::IDENTITY_ID),
            k::IDENTITY_SIGNING,
            "the sunrise.local_identity.identity.sign.v2 AAD drifted"
        );
        assert_eq!(
            identity_dh_aad(&k::IDENTITY_ID),
            k::IDENTITY_DH,
            "the sunrise.local_identity.identity.dh.v2 AAD drifted"
        );
        assert_eq!(
            WRAPPED_SECRET_LEN,
            k::WRAPPED_SECRET_LEN,
            "WRAPPED_SECRET_LEN moved away from the length every wrapped \
             secret on disk already has"
        );
    }

    /// The rule this module's header states, asserted rather than described.
    ///
    /// `device_id` and `identity_id` are both 16 bytes, so the id half of the
    /// AAD cannot separate a device secret from an identity secret — only the
    /// prefix can. Deliberately compares the *whole* AAD at one shared id to
    /// make that concrete.
    #[test]
    fn each_wrapped_secret_has_its_own_domain() {
        let aads = [
            device_aad(&DEVICE),
            device_dh_aad(&DEVICE),
            identity_signing_aad(&ID),
            identity_dh_aad(&ID),
        ];
        for (i, a) in aads.iter().enumerate() {
            for (j, b) in aads.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "AAD {i} and AAD {j} share a domain");
                }
            }
        }
    }

    /// `ID_S_priv` and `ID_D_priv` were wrapped under one AAD, so either blob
    /// opened under the other's domain. This is the finding, stated as the
    /// property that must hold: a blob wrapped for one role does not open as
    /// the other.
    ///
    /// Checked at the wrap layer rather than through `unwrap_identity`, because
    /// `unwrap_identity` has a second guard — it re-derives `identity_id` from
    /// the signing key it recovered — which would refuse a swap for a reason
    /// that has nothing to do with the AAD and would go on passing with this
    /// fix reverted.
    #[test]
    fn an_identity_blob_does_not_open_under_the_other_half_s_domain() {
        let vault_root = root();
        let rng = FixedRng(7);
        let signing = IdentitySigningKeyPair::from_secret_bytes(&[0x31u8; 32]);
        let dh = IdentityDhKeyPair::from_secret_bytes([0x32u8; 32]);
        let identity = Identity {
            identity_id: ID,
            dh_pub: dh.public_bytes(),
            dh_secret: Some(dh),
            // `Held`, because this asserts on the wrapped blob and a
            // `PublicOnly` identity wraps nothing to assert on. The optional
            // arm arrived with `#105`; the domain split it is threaded through
            // is the finding this test pins.
            signing: IdentitySigningKey::Held(signing),
            created_at_ms: 0,
        };
        let (wrapped_s, wrapped_d) = wrap_identity(&vault_root, &identity, &rng);

        // Each opens under its own domain.
        assert_eq!(
            unwrap_secret(&vault_root, &wrapped_s, &identity_signing_aad(&ID)).expect("sign opens"),
            [0x31u8; 32]
        );
        assert_eq!(
            unwrap_secret(&vault_root, &wrapped_d, &identity_dh_aad(&ID)).expect("dh opens"),
            [0x32u8; 32]
        );

        // Neither opens under the other's.
        assert!(
            matches!(
                unwrap_secret(&vault_root, &wrapped_s, &identity_dh_aad(&ID)),
                Err(KeychainError::VaultRootMismatch)
            ),
            "ID_S_priv opened as ID_D_priv: the two share an AAD"
        );
        assert!(
            matches!(
                unwrap_secret(&vault_root, &wrapped_d, &identity_signing_aad(&ID)),
                Err(KeychainError::VaultRootMismatch)
            ),
            "ID_D_priv opened as ID_S_priv: the two share an AAD"
        );
    }

    /// The same separation across the device/identity line: an identity secret
    /// must not open under a device's domain even when the two ids collide,
    /// which they can, because both are 16 opaque bytes from different KDFs.
    #[test]
    fn an_identity_blob_does_not_open_under_a_device_domain() {
        let vault_root = root();
        let rng = FixedRng(9);
        let wrapped = wrap_secret(&vault_root, &[0x41u8; 32], &identity_signing_aad(&ID), &rng);
        assert!(matches!(
            unwrap_secret(&vault_root, &wrapped, &device_aad(&DEVICE)),
            Err(KeychainError::VaultRootMismatch)
        ));
        assert!(matches!(
            unwrap_secret(&vault_root, &wrapped, &device_dh_aad(&DEVICE)),
            Err(KeychainError::VaultRootMismatch)
        ));
    }
}
