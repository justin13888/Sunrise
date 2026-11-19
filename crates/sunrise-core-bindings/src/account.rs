//! This device and its account on the relay: the device's identity, whether it
//! holds the account's identity key, the recovery blob, and registration.

use zeroize::Zeroize as _;

#[cfg(doc)]
use crate::SunriseLogin;
use crate::{dto, BindingError, SunriseCore};

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// This device's stable id, hex-encoded — what [`SunriseLogin::begin`]
    /// binds the token to.
    #[must_use]
    pub fn device_id(&self) -> String {
        sunrise_client_core::ids::hex_id(&self.inner.device_id())
    }

    /// This device's certificate (canonical CBOR), for pairing.
    #[must_use]
    pub fn device_cert(&self) -> Vec<u8> {
        self.inner.device_cert()
    }

    /// Whether this vault holds `ID_D_priv` — the account identity's
    /// unwrapping key.
    ///
    /// True on the device that created the account and on one restored from a
    /// recovery code; false on every device admitted by pairing, which since
    /// `#76` is handed only `ID_D_pub`.
    ///
    /// **This is the signal a client is meant to surface rather than act on,
    /// and it had reached none.** Where no recovery blob has been sealed, a
    /// `true` here means *this vault is the only place `ID_D_priv` exists, and
    /// losing it destroys the key permanently*: every `Recipient::Identity`
    /// copy in the op log becomes unopenable forever, and no recovery feature
    /// shipped afterwards can retrieve it, because sealing a blob needs the key
    /// it would carry. `docs/03-crypto/key-rotation.md` §Revocation has said so
    /// for some time; [#144](https://github.com/justin13888/Sunrise/issues/144)
    /// is that it was said nowhere a user could see.
    ///
    /// It is also the precondition on [`Self::bootstrap_account`] producing a
    /// recovery code at all: a paired device cannot seal a blob, and one that
    /// tried would hand its user a code decrypting to a key that opens nothing.
    #[must_use]
    pub fn holds_identity_key(&self) -> bool {
        self.inner.holds_identity_key()
    }

    /// Whether a revocation of `device` is still owed to the relay.
    ///
    /// The other half of what a client must say after `RevokeDevice`, and the
    /// half a user actually asked for. The vault half is committed the moment
    /// the command returns; the relay half is a durable intent that needs a
    /// session, so a client that reported only the first would let someone with
    /// no network believe a stolen laptop had been cut off from the server
    /// ([#160](https://github.com/justin13888/Sunrise/issues/160)).
    ///
    /// `sunrise device revoke` has printed both since #160. This is what lets
    /// the Apple device list do the same rather than reintroducing the
    /// disclosure failure on a second client.
    ///
    /// # Errors
    /// [`BindingError::BadId`] when `device` is not a `dev_` reference, and
    /// [`BindingError::Core`] on a storage failure.
    pub fn relay_revocation_pending(
        &self,
        device: sunrise_id::EntityRef,
    ) -> Result<bool, BindingError> {
        if device.kind() != sunrise_id::EntityKind::Device {
            return Err(BindingError::BadId {
                id: device.to_str(),
                cause: "expected a device reference".into(),
            });
        }
        Ok(self.inner.relay_revocation_pending(device.bytes())?)
    }

    /// Seal this account's identity keys into a recovery blob under a
    /// caller-supplied 32-byte seed.
    ///
    /// **Prefer [`Self::bootstrap_account`].** This is the raw operation, and
    /// the raw operation puts two things on the caller that the caller should
    /// not be carrying: drawing 32 bytes from a CSPRNG, and destroying them
    /// afterwards. A seed that crosses this seam lives in a Swift `Data` for as
    /// long as ARC decides, and a recovery code is the one secret whose whole
    /// value is that it exists in exactly two places — on the user's paper and
    /// inside the blob. `bootstrap_account` draws the seed on this side of the
    /// seam, uses it twice, and drops it.
    ///
    /// It is exposed because a caller that has a seed from somewhere else — a
    /// test vector, or a future social-recovery split that reassembles one from
    /// shares — has no other way in.
    ///
    /// # Errors
    ///
    /// [`BindingError::BadFixedBytes`] if `seed` is not exactly 32 bytes, and
    /// [`BindingError::Core`] on a device admitted by pairing, which holds no
    /// `ID_D_priv` to seal.
    pub fn seal_recovery_blob(&self, seed: Vec<u8>) -> Result<Vec<u8>, BindingError> {
        let mut seed: [u8; 32] = seed.try_into().map_err(|_| BindingError::BadFixedBytes {
            field: "recovery seed".to_owned(),
            expected: 32,
        })?;
        let out = self.inner.seal_recovery_blob(&seed);
        seed.zeroize();
        Ok(out?)
    }

    /// Publish this vault to the relay and come back with the recovery code —
    /// the whole of what `sunrise bootstrap` does, for the Apple clients.
    ///
    /// Before this existed, `apps/apple` could reach neither
    /// `Core::seal_recovery_blob` nor `POST /api/v1/accounts`, so **a vault
    /// created by the app had no recovery blob at all** and was therefore the
    /// only place `ID_D_priv` existed
    /// ([#181](https://github.com/justin13888/Sunrise/issues/181)).
    ///
    /// One call rather than three, and deliberately: the seed is drawn here,
    /// sealed here, uploaded here, and dropped here, so the code handed back is
    /// always the code the stored blob opens. `POST /accounts` answers `409
    /// RECOVERY_BLOB_EXISTS` to a second, differing blob rather than discarding
    /// it, which is what makes that guarantee hold across a retry — but only if
    /// the code is not shown until the upload has been accepted, and a caller
    /// holding three separate calls is a caller that can get that order wrong.
    ///
    /// [`dto::AccountBootstrap::recovery_code`] is `None` on a device admitted by
    /// pairing, which holds no `ID_D_priv`. That is not an error and the app
    /// must not present it as one: the device that created the account is the
    /// one that can produce a code, and this device's account already has one.
    /// A paired device that only needs its relay device id should call
    /// [`Self::register_relay_device`] instead, which publishes nothing about
    /// the account and asserts no terms.
    ///
    /// `terms_accepted_at_ms` is the `terms_at_ms` the relay records, and it is
    /// the **caller's** to supply because it is a product fact, not a clock
    /// reading: the moment this account's holder accepted the relay operator's
    /// terms. `docs/06-server/api.md` §Terms acceptance defines it. This method
    /// used to fill it with its own `now_ms()`, which decided on every caller's
    /// behalf that an acceptance had happened
    /// ([#183](https://github.com/justin13888/Sunrise/issues/183)). The relay
    /// keeps the first value it is sent, so a retry does not move it.
    ///
    /// The relay device id comes back in [`dto::AccountBootstrap::device_id`],
    /// and the caller must record it: the relay never sends it again, and it is
    /// what [`Self::start_sync`]'s `relay_device_id` names.
    ///
    /// # Errors
    ///
    /// [`BindingError::Core`] if the blob cannot be sealed, and
    /// [`BindingError::Relay`] with what the relay said.
    pub async fn bootstrap_account(
        &self,
        relay_url: String,
        bearer: String,
        email: String,
        nickname: String,
        terms_accepted_at_ms: u64,
    ) -> Result<dto::AccountBootstrap, BindingError> {
        // Exactly `sunrise bootstrap`'s sequence — draw, seal, encode, upload —
        // because it is the same function.
        let done = sunrise_client_core::account::bootstrap_account(
            &self.inner,
            &sunrise_core::SystemRng,
            &relay_url,
            &bearer,
            email,
            terms_accepted_at_ms,
            device_label(nickname),
        )
        .await
        .map_err(|e| match e {
            sunrise_client_core::account::AccountError::Seal(e) => BindingError::from(e),
            sunrise_client_core::account::AccountError::Relay(e) => {
                BindingError::Relay(e.to_string())
            }
        })?;

        Ok(dto::AccountBootstrap {
            identity_id: done.outcome.identity_id,
            email: done.outcome.email,
            device_id: done.outcome.device_id,
            // Read out only after the relay accepted the blob, so a code this
            // ever returns is a code the stored blob opens.
            recovery_code: done.recovery_code.map(|c| c.reveal().to_owned()),
        })
    }

    /// Register this device on an account that already exists, and return the
    /// id the relay minted for it.
    ///
    /// The second half of [`Self::bootstrap_account`] on its own — exactly what
    /// `sunrise recover` does with `sunrise_relay_client::register_device`, and
    /// for the same reason. A device admitted by pairing joins an account its
    /// sponsor already published: it holds no `ID_D_priv` to seal, has no
    /// identity keys to add that the relay does not already hold, and has no
    /// business asserting a terms acceptance on the account holder's behalf.
    /// Before this existed such a device could reach the relay only through
    /// `bootstrap_account`, which the Apple app never called for it, so it held
    /// no relay device id and a relay with `require_device_sig` refused it
    /// ([#183](https://github.com/justin13888/Sunrise/issues/183)).
    ///
    /// Also the path a founding device takes when its id was never recorded:
    /// the relay treats a second registration of the same key as a second row
    /// rather than an error, so this cannot fail for having run before.
    ///
    /// The returned id is the caller's to record, for the reason
    /// [`dto::AccountBootstrap::device_id`] gives.
    ///
    /// # Errors
    ///
    /// [`BindingError::Relay`] with what the relay said.
    pub async fn register_relay_device(
        &self,
        relay_url: String,
        bearer: String,
        nickname: String,
    ) -> Result<String, BindingError> {
        sunrise_client_core::account::register_relay_device(
            &self.inner,
            &relay_url,
            &bearer,
            device_label(nickname),
        )
        .await
        .map_err(|e| BindingError::Relay(e.to_string()))
    }
}

/// How this build describes itself to `POST /api/v1/devices`: the caller's
/// nickname, this build's platform tag and version.
pub(crate) fn device_label(nickname: String) -> sunrise_client_core::account::DeviceLabel {
    sunrise_client_core::account::DeviceLabel {
        nickname,
        platform: platform_tag(),
        app_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    }
}

/// This build's platform tag, as `POST /api/v1/devices` names them. One
/// source for every registration path, so a founding, a paired and a
/// recovered device cannot describe their platform differently.
fn platform_tag() -> String {
    if cfg!(target_os = "ios") {
        "ios".to_owned()
    } else {
        "macos".to_owned()
    }
}
