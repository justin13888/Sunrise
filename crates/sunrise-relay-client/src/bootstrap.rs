//! The account-and-device bootstrap: the flow ADR-0021 found had no caller.
//!
//! Two calls, in an order the crypto design fixes:
//!
//! 1. `POST /api/v1/accounts` publishes the identity keys and the recovery
//!    blob. It rides the bootstrap exemption — a device cannot sign a request
//!    before it exists — so it presents a bearer and no device binding.
//! 2. `POST /api/v1/devices` registers this device's Ed25519 signing key, which
//!    is what verifies its `X-Sunrise-Device-Sig` on every request afterwards.
//!
//! Both are idempotent at the server: `resolve_account` mints an account on
//! first sight of a `(iss, sub)` and returns the existing one afterwards, and a
//! device re-registering with the same key is a second row rather than an
//! error. So a client that crashes between the two, or repeats the pair, is
//! recoverable by running it again — which is the property that lets a caller
//! do this on every start rather than tracking whether it has run.

use crate::api;
use crate::api::SecretString;
use sunrise_onboarding::account::AccountCreateRequest;

/// What a device needs to introduce itself.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// The public half of the Ed25519 key that signs `header_sig_v2`
    /// (`D_S_pub`), as raw bytes.
    ///
    /// Bytes rather than the base64url-no-pad string the wire carries, and
    /// that is the whole point. The relay validates this field with
    /// `VerifyingKey::from_bytes` over a base64url decode and answers `400`
    /// otherwise, and the CLI — the only caller — passed
    /// `hex(core.device_id())` here: 32 hex characters of a 16-byte *id*,
    /// which decode to 24 bytes and can never be a key, so `sunrise login`
    /// against a real relay could not register a device at all. A field that
    /// takes the key itself has nowhere for that mistake to live, and
    /// [`bootstrap`] does the encoding once, with the crate that defines it.
    pub device_pub_s: [u8; 32],
    /// X25519 key, base64url no-pad, when the device has one.
    pub device_pub_d: Option<String>,
    /// Self-signed device certificate, opaque to the server.
    pub device_cert: Option<String>,
    /// The id this device answers to *inside the vault*, Crockford base-32 of
    /// 16 bytes.
    ///
    /// The relay mints its own id for a device and returns it here, and that id
    /// never travels back through the op stream — so a peer that later revokes
    /// this device holds only the vault id. Registering it is what makes
    /// `DELETE /api/v1/devices/by-vault-id/{id}` able to name this row; a
    /// device that registers `None` cannot be revoked at the relay at all.
    pub vault_device_id: Option<String>,
    /// User-visible name.
    pub nickname: String,
    /// One of `ios`, `android`, `macos`, `windows`, `linux`, `web`.
    pub platform: String,
    /// Reported app version.
    pub app_version: Option<String>,
}

/// What the bootstrap established.
#[derive(Debug, Clone)]
pub struct BootstrapOutcome {
    /// The account as the server now holds it.
    pub identity_id: String,
    /// The normalized email the server recorded.
    pub email: String,
    /// The id the server assigned this device. Every later request names it in
    /// `X-Sunrise-Device`.
    pub device_id: String,
}

/// Why a bootstrap did not complete.
///
/// Deliberately three arms rather than one: a caller retries a transport
/// failure, fixes a request the server refused, and reports a server fault. The
/// generated client's own taxonomy is richer — it distinguishes construction,
/// timeout, decode and protocol failures — and `Error::is_transient` is what
/// classifies them, so the detail is preserved in the message rather than
/// discarded.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// The account could not be created or read.
    #[error("account bootstrap failed: {0}")]
    Account(String),
    /// The device could not be registered.
    #[error("device registration failed: {0}")]
    Device(String),
}

/// Narrow the acceptance timestamp to what the description declares.
///
/// The document gives `terms_at_ms` a signed integer, so the generated model
/// takes one. A `u64` past `i64::MAX` is a clock that is wrong by three hundred
/// million years rather than a value to wrap, and saying so is cheaper than
/// discovering it as a negative timestamp on the server.
fn terms_at_ms(ms: u64) -> Result<i64, BootstrapError> {
    i64::try_from(ms).map_err(|_| {
        BootstrapError::Account("terms_at_ms is not a representable timestamp".to_owned())
    })
}

/// Create (or adopt) the account, then register this device.
///
/// `base_url` is the relay origin — `https://relay.example`, no path. `bearer`
/// is the OIDC access token; the relay resolves the account from its `(iss,
/// sub)` and never from anything in the body, so the request cannot name an
/// account it does not hold a token for.
///
/// # Errors
/// [`BootstrapError`] naming which of the two calls failed and what the
/// generated client reported.
pub async fn bootstrap(
    base_url: &str,
    bearer: &str,
    account: AccountCreateRequest,
    device: DeviceIdentity,
) -> Result<BootstrapOutcome, BootstrapError> {
    // `AccountToken` is the scheme's name in `components.securitySchemes`,
    // which the generated client matches each operation's `security`
    // requirement against. A missing required credential is a
    // request-construction error rather than a silent 401.
    let client = api::Client::new(base_url)
        .map_err(|e| BootstrapError::Account(e.to_string()))?
        .with_credential(
            "AccountToken",
            api::Credential::Bearer(SecretString::from(bearer.to_owned())),
        );

    // `None` for the per-operation parameters: this is the bootstrap
    // exemption, so the three device-binding headers are deliberately absent.
    let created = client
        .create_account(
            None,
            &api::types::AccountCreateRequest {
                email: account.email,
                identity_signing_pub: sunrise_onboarding::encode_public_key(
                    &account.identity_signing_pub,
                ),
                identity_dh_pub: sunrise_onboarding::encode_public_key(&account.identity_dh_pub),
                // The empty string is how the description says "no blob": the
                // handler filters it out before the store sees it, so a device
                // that cannot seal one leaves the column exactly as the
                // founding device wrote it.
                recovery_blob: account
                    .recovery_blob
                    .as_deref()
                    .map(sunrise_onboarding::encode_recovery_blob)
                    .unwrap_or_default(),
                terms_at_ms: terms_at_ms(account.terms_at_ms)?,
            },
        )
        .await
        .map_err(|e| BootstrapError::Account(e.to_string()))?
        .into_inner();

    let registered = client
        .register_device(
            None,
            &api::types::DeviceRegisterRequest {
                device_pub_s: sunrise_http_sig::device_pub_b64(&device.device_pub_s),
                device_pub_d: device.device_pub_d,
                device_cert: device.device_cert,
                vault_device_id: device.vault_device_id,
                nickname: device.nickname,
                platform: device.platform,
                app_version: device.app_version,
            },
        )
        .await
        .map_err(|e| BootstrapError::Device(e.to_string()))?
        .into_inner();

    Ok(BootstrapOutcome {
        identity_id: created.identity_id,
        email: created.email,
        device_id: registered.device_id,
    })
}
