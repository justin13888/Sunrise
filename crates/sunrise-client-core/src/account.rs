//! Publishing a vault to the relay: the account-and-device bootstrap, and the
//! registration a device makes on an account that already exists.
//!
//! `sunrise bootstrap` and the `UniFFI` seam's `bootstrap_account` used to
//! each carry this sequence, and the two copies were where #181 and #183 hid:
//! a vault created by the app that sealed no recovery blob, and a terms
//! timestamp decided on the caller's behalf. One implementation now, which
//! both call.
//!
//! The order is the guarantee. The seed is drawn here, sealed here, uploaded
//! here and dropped here, and the code is handed back only after the relay has
//! accepted the blob: `POST /accounts` answers `409 RECOVERY_BLOB_EXISTS` to a
//! second, differing blob rather than discarding it, so a code this returns is
//! always the code the stored blob opens.

use sunrise_core::{Core, CoreError, Rng};
use sunrise_crypto::bip39::{encode_recovery_code, RecoveryCode, RECOVERY_ENTROPY_LEN};
use sunrise_relay_client::{
    AccountCreateRequest, BootstrapError, BootstrapOutcome, DeviceIdentity,
};
use zeroize::Zeroize as _;

pub use crate::recovery::DeviceLabel;

/// What this device tells `POST /api/v1/devices` about itself.
///
/// One builder for every registration path, so a founding device and a paired
/// one cannot come to describe themselves differently.
#[must_use]
pub fn device_identity(core: &Core, label: DeviceLabel) -> DeviceIdentity {
    DeviceIdentity {
        // `D_S_pub`, the key the relay checks every `X-Sunrise-Device-Sig`
        // against — not the 16-byte device id, which decodes to no key.
        device_pub_s: core.device_signing_pub(),
        device_pub_d: None,
        device_cert: None,
        // The only name a sibling can revoke this device by: the relay mints
        // its own id and never sends it back through the op stream. A device
        // that omits it cannot be revoked at the relay at all.
        vault_device_id: Some(sunrise_id::crockford::encode_bytes(&core.device_id())),
        nickname: label.nickname,
        platform: label.platform,
        app_version: label.app_version,
    }
}

/// A recovery blob sealed under a freshly drawn seed, and the code that
/// opens it.
pub struct SealedRecovery {
    /// The sealed blob, for `POST /api/v1/accounts`.
    pub blob: Vec<u8>,
    /// The 24 words. Show them only once the relay has stored `blob`.
    pub code: RecoveryCode,
}

// Hand-written: the code is the account's one written-down secret.
impl std::fmt::Debug for SealedRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedRecovery")
            .field("blob_len", &self.blob.len())
            .field("code", &self.code)
            .finish()
    }
}

/// Draw a seed from `rng`, seal this account's identity under it, and drop
/// the seed.
///
/// `None` on a device admitted by pairing, which holds no `ID_D_priv`: only
/// the account's creator can seal a blob that restores an identity able to
/// open the identity-addressed `key_envelope` ops a recovery replays, and a
/// paired device that sealed one would hand its user a code that opens
/// nothing — worse than no code, because the user believes they are covered.
///
/// # Errors
/// What the core's seal raises.
pub fn seal_fresh_recovery(
    core: &Core,
    rng: &dyn Rng,
) -> Result<Option<SealedRecovery>, CoreError> {
    if !core.holds_identity_key() {
        return Ok(None);
    }
    let mut seed = [0u8; RECOVERY_ENTROPY_LEN];
    rng.fill_bytes(&mut seed);
    let sealed = core.seal_recovery_blob(&seed);
    let code = encode_recovery_code(&seed);
    seed.zeroize();
    Ok(Some(SealedRecovery {
        blob: sealed?,
        code,
    }))
}

/// What a completed [`bootstrap_account`] established.
#[derive(Debug)]
pub struct Bootstrapped {
    /// The account and this device, as the relay now holds them. The caller
    /// records `device_id`: the relay never sends it again.
    pub outcome: BootstrapOutcome,
    /// The recovery code, `None` on a device admitted by pairing.
    pub recovery_code: Option<RecoveryCode>,
}

/// Why a [`bootstrap_account`] did not complete.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// The recovery blob could not be sealed. Nothing was sent.
    #[error(transparent)]
    Seal(#[from] CoreError),
    /// The relay refused or could not be reached.
    #[error(transparent)]
    Relay(#[from] BootstrapError),
}

/// Publish this vault to the relay at `base_url` under `bearer`, register
/// this device as `device`, and come back with the recovery code.
///
/// `email` is a fallback the relay reads only when the identity provider emits
/// no `email` claim; pass the empty string rather than inventing one.
/// `terms_accepted_at_ms` is the moment the account's holder accepted the
/// relay operator's terms (`docs/06-server/api.md` §Terms acceptance) — a
/// product fact the caller supplies, not a clock reading this decides on its
/// behalf. The relay keeps the first value it is sent.
///
/// # Errors
/// [`AccountError::Seal`] before anything is sent, and
/// [`AccountError::Relay`] with what the relay said.
pub async fn bootstrap_account(
    core: &Core,
    rng: &dyn Rng,
    base_url: &str,
    bearer: &str,
    email: String,
    terms_accepted_at_ms: u64,
    device: DeviceLabel,
) -> Result<Bootstrapped, AccountError> {
    let sealed = seal_fresh_recovery(core, rng)?;
    let (recovery_blob, recovery_code) = match sealed {
        Some(SealedRecovery { blob, code }) => (Some(blob), Some(code)),
        None => (None, None),
    };
    let outcome = sunrise_relay_client::bootstrap(
        base_url,
        bearer,
        AccountCreateRequest {
            email,
            identity_signing_pub: core.identity_signing_pub(),
            identity_dh_pub: core.identity_dh_pub(),
            recovery_blob,
            terms_at_ms: terms_accepted_at_ms,
        },
        device_identity(core, device),
    )
    .await?;
    // Handed back only now, after the relay accepted the blob.
    Ok(Bootstrapped {
        outcome,
        recovery_code,
    })
}

/// Register this device on an account that already exists, and return the id
/// the relay minted for it.
///
/// The second half of [`bootstrap_account`] on its own: what a device admitted
/// by pairing needs, which holds no `ID_D_priv` to seal and has no business
/// asserting a terms acceptance on the account holder's behalf, and what a
/// founding device whose id was never recorded runs again. The relay treats a
/// second registration of the same key as a second row rather than an error.
///
/// # Errors
/// [`BootstrapError::Device`] with what the relay said.
pub async fn register_relay_device(
    core: &Core,
    base_url: &str,
    bearer: &str,
    device: DeviceLabel,
) -> Result<String, BootstrapError> {
    sunrise_relay_client::register_device(base_url, bearer, device_identity(core, device)).await
}

/// Why a [`reissue_recovery_code`] did not complete.
#[derive(Debug, thiserror::Error)]
pub enum ReissueError {
    /// This device holds no `ID_D_priv`, so no blob it sealed could open the
    /// account. Nothing was sent.
    #[error(
        "this device does not hold the account's recovery key, so it cannot issue a recovery \
         code; run this on the device the account was created on or restored to"
    )]
    NoRecoveryKey,
    /// This device no longer signs under the identity in force: a blob it
    /// sealed would carry a retired identity. Nothing was sent.
    #[error(
        "this device no longer speaks for the account's current identity, so a recovery code it \
         issued would open a retired one; run this on a device that does"
    )]
    NotCurrent,
    /// The blob could not be sealed. Nothing was sent.
    #[error(transparent)]
    Seal(#[from] CoreError),
    /// The relay refused the blob or could not be reached. The code was not
    /// shown and is gone; the blob in place, if any, is unchanged.
    #[error("the relay did not take the new recovery blob: {0}")]
    Relay(sunrise_sync::TransportError),
}

/// Seal this account's identity under a freshly drawn code, replace the
/// relay's recovery blob with it, and hand back the code (ADR-0070 §2).
///
/// What a rotation that retires the code runs next, and what
/// `sunrise identity new-code` runs alone. The order is the bootstrap's: the
/// code is returned only once the relay holds the blob it opens, so a code a
/// caller shows is always one the relay can serve. The relay keeps a few
/// bytes of the blob it replaces, so the code that opened that one fails a
/// recovery with a reason rather than as a mistyped code.
///
/// `transport` must carry this device's binding: the relay replaces a blob
/// only for a registered device of the account.
///
/// # Errors
/// [`ReissueError`].
pub async fn reissue_recovery_code(
    core: &Core,
    rng: &dyn Rng,
    transport: &mut dyn sunrise_sync::Transport,
) -> Result<RecoveryCode, ReissueError> {
    // A device a rotation left behind still holds the identity it was left
    // at, and a blob of that identity is a code that opens nothing current.
    match core.query(sunrise_core::Query::IdentityStatus).await? {
        sunrise_core::QueryResult::Identity(st) if st.this_device_is_current => {}
        _ => return Err(ReissueError::NotCurrent),
    }
    let Some(SealedRecovery { blob, code }) = seal_fresh_recovery(core, rng)? else {
        return Err(ReissueError::NoRecoveryKey);
    };
    transport
        .recovery_blob_replace(&blob)
        .await
        .map_err(ReissueError::Relay)?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use sunrise_core::{CoreConfig, SystemRng, Unlock};
    use sunrise_crypto::keys::VaultRootKey;

    /// A seed of one repeated byte, counting how often it was asked.
    #[derive(Debug, Default)]
    struct Fixed(AtomicUsize);

    impl Rng for Fixed {
        fn fill_bytes(&self, dest: &mut [u8]) {
            self.0.fetch_add(1, Ordering::SeqCst);
            dest.fill(0x5a);
        }
    }

    /// 2026-01-01T00:00:00Z in UTC: no test here reads the host clock.
    #[derive(Debug)]
    struct Midnight;

    impl sunrise_core::Clock for Midnight {
        fn now_ms(&self) -> u64 {
            1_767_225_600_000
        }
        fn timezone(&self) -> String {
            "UTC".to_owned()
        }
    }

    async fn founding(dir: &std::path::Path) -> Core {
        let cfg = CoreConfig::with_clock(
            dir.to_path_buf(),
            "0.1.0+test",
            Arc::new(Midnight),
            Arc::new(SystemRng),
        );

        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes([3; 32]),
                paired: None,
            },
        )
        .await
        .expect("open a founding vault")
    }

    fn label() -> DeviceLabel {
        DeviceLabel {
            nickname: "laptop".into(),
            platform: "linux".into(),
            app_version: Some("1.2.3".into()),
        }
    }

    #[tokio::test]
    async fn a_device_introduces_itself_by_its_signing_key_and_vault_id() {
        let dir = tempfile::tempdir().unwrap();
        let core = founding(dir.path()).await;
        let id = device_identity(&core, label());
        assert_eq!(id.device_pub_s, core.device_signing_pub());
        assert_eq!(
            id.vault_device_id,
            Some(sunrise_id::crockford::encode_bytes(&core.device_id()))
        );
        assert_eq!(
            (
                id.nickname.as_str(),
                id.platform.as_str(),
                id.app_version.as_deref()
            ),
            ("laptop", "linux", Some("1.2.3"))
        );
        assert!(id.device_pub_d.is_none() && id.device_cert.is_none());
        core.shutdown().await;
    }

    /// The code handed back opens the blob sealed beside it, under the seed
    /// the injected rng drew.
    #[tokio::test]
    async fn the_founding_device_seals_a_blob_its_code_opens() {
        let dir = tempfile::tempdir().unwrap();
        let core = founding(dir.path()).await;
        let rng = Fixed::default();
        let sealed = seal_fresh_recovery(&core, &rng)
            .unwrap()
            .expect("the creator holds ID_D_priv");
        assert_eq!(rng.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            sealed.code.reveal(),
            encode_recovery_code(&[0x5a; RECOVERY_ENTROPY_LEN]).reveal()
        );
        let identity_id = sunrise_crypto::identity_id_from_pub(&core.identity_signing_pub());
        let opened = crate::recovery::recover_identity_from_code(
            &sealed.blob,
            sealed.code.reveal(),
            &identity_id,
        )
        .expect("the code opens the blob");
        assert_eq!(opened.id_s_pub, core.identity_signing_pub());
        assert!(!format!("{sealed:?}").contains(sealed.code.reveal()));
        core.shutdown().await;
    }
}
