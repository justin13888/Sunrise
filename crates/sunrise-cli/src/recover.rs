//! `sunrise recover` — spend a recovery code and come back with a vault.
//!
//! `docs/03-crypto/recovery.md` §Recovery flow is eight steps. Steps 2-5 were
//! already built and reachable (`sunrise_onboarding::recover_identity_from_code`
//! opens the blob and hands back the identity); what had no caller was
//! everything that turns an opened blob into something a user can use, and an
//! opened blob is not a vault.
//!
//! This module is steps 1 and 3 through 8, in the order they have to happen —
//! and the order is the load-bearing part. #180 states the trap outright: *a
//! recovery that mints the root before it can decrypt anything produces a vault
//! that opens and is empty, which looks exactly like success.* So the vault is
//! created **from** the restored identity rather than before it, and the
//! command does not claim success until the sync driver has actually caught up.
//!
//! ```text
//!  step 1   the user points this at an empty vault directory
//!  step 3   GET /accounts/me           -> ID_S_pub  -> identity_id (the AAD)
//!           GET /accounts/me/recovery_blob         (behind an OIDC step-up)
//!  steps 4-5 the 24 words open the blob -> ID_S_priv, ID_D_priv
//!  step 6   mint a fresh vault root; Core::open re-keys this device under the
//!           restored identity and publishes its DeviceCert
//!  step 8   sync: every identity-addressed `key_envelope` is replayed, and the
//!           ops that were parked waiting on those keys are drained
//!  step 7   tell the user to revoke the devices they lost and rotate their
//!           Stream keys — last, because it is advice about a vault that now
//!           exists
//! ```
//!
//! # Why the vault directory has to be empty
//!
//! The identity a vault belongs to is decided when it is created and cannot be
//! changed afterwards (`Keychain::open`'s case 4 loads what is on disk and
//! never replaces it). Recovering *into* an existing vault would therefore
//! either fail with `IdentityConflict` or, worse, quietly leave the old
//! identity in place — so this refuses up front, with the directory named.

use std::path::{Path, PathBuf};

use sunrise_core::{Core, CoreConfig, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_crypto::recovery::RecoveryPayload;

use crate::{livesync, login, vault};

/// The file whose presence means "there is already a vault here".
const VAULT_DB_FILE: &str = "vault.db";

/// How long to wait for the replay of §Recovery flow step 8 to reach the
/// vault before reporting that it did not.
const CATCH_UP_MS: u64 = 60_000;

/// Why a recovery did not complete.
///
/// Deliberately one arm per thing the *user* can do about it. A recovery is
/// run by somebody who has already lost every device, so "it failed" is not an
/// acceptable message: each of these names the next action.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// The target directory already holds a vault.
    #[error(
        "{} already holds a vault; a recovery creates one and cannot merge into another. \
         Point SUNRISE_VAULT at an empty directory and run this again.",
        .0.display()
    )]
    NotEmpty(PathBuf),
    /// No relay origin.
    #[error("set {} to the relay origin", livesync::ENV_SYNC_URL)]
    NoRelay,
    /// No recovery code on the command line and nothing on stdin.
    #[error("a recovery needs the 24-word code: `sunrise recover <word>...`, or pipe it in")]
    NoCode,
    /// The account record carries no identity key.
    #[error(
        "this account has published no identity key, so there is nothing to recover into. \
         An account is only recoverable once a device has run `sunrise bootstrap`."
    )]
    NoIdentityKey,
    /// The relay refused or could not be reached.
    #[error("{0}")]
    Relay(String),
    /// The blob came back and the words did not open it.
    #[error(transparent)]
    Code(#[from] sunrise_onboarding::RecoveryFlowError),
    /// A malformed identity key on the account record.
    #[error("the account's identity key is unreadable: {0}")]
    IdentityKey(#[from] sunrise_onboarding::PublicKeyError),
    /// The vault root could not be minted or stored.
    #[error(transparent)]
    Vault(#[from] vault::VaultError),
    /// The vault could not be opened from the restored identity.
    #[error(transparent)]
    Core(#[from] sunrise_core::CoreError),
    /// Sync never caught up, so the history was not replayed.
    #[error(
        "the vault was restored but never finished reading its history ({0}). \
         Nothing is lost: run `sunrise sync --once` when the network is better."
    )]
    NeverCaughtUp(String),
}

/// What a completed recovery produced, for the caller to report.
#[derive(Debug, Clone)]
pub struct Recovered {
    /// The account's 16-byte identity id, hex.
    pub identity_id: String,
    /// The relay's id for the freshly re-keyed device.
    pub relay_device_id: String,
    /// The directory the restored vault was written to.
    pub vault_dir: PathBuf,
}

/// Derive the vault's `identity_id` from the `ID_S_pub` the account record
/// carries.
///
/// This is the value the recovery blob's AAD binds
/// (`"sunrise.recovery_blob.v1" ‖ identity_id`), so it has to be known
/// *before* the blob can be opened — which on a fresh device means it can come
/// from exactly one place, the relay. `recover_identity_from_code` says it
/// "MUST come from the OIDC account record"; `AccountInfo::identity_id` is the
/// relay's own account id and not this, so `identity_signing_pub` is what that
/// sentence has to mean.
///
/// Nothing is trusted on the strength of the relay's answer. A relay that
/// served somebody else's key would produce an `identity_id` the blob was not
/// sealed under, and the AEAD refuses it — which is the same refusal a
/// mistyped code gets, and is why this is a derivation rather than a check.
///
/// # Errors
/// [`RecoverError::NoIdentityKey`] when the account never published one,
/// [`RecoverError::IdentityKey`] when what it published is not a key.
pub fn identity_id_from_account(
    identity_signing_pub: Option<&str>,
) -> Result<[u8; 16], RecoverError> {
    let encoded = identity_signing_pub
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(RecoverError::NoIdentityKey)?;
    let key = sunrise_onboarding::decode_public_key(encoded)?;
    Ok(sunrise_crypto::identity_id_from_pub(&key))
}

/// The 24 words, from the command line or from stdin.
///
/// Both, because a recovery code is the one thing a user is most likely to
/// have in a password manager rather than in their head — and a shell history
/// full of somebody's recovery code is a worse outcome than a prompt.
///
/// Joining on single spaces rather than passing the words through is what
/// makes `sunrise recover a b  c` and a pasted line with a trailing newline
/// the same input; the BIP-39 decoder's own normalisation does the rest.
#[must_use]
pub fn code_from(args: &[String], piped: &str) -> Option<String> {
    let joined = if args.is_empty() {
        piped.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        args.iter()
            .flat_map(|a| a.split_whitespace())
            .collect::<Vec<_>>()
            .join(" ")
    };
    (!joined.is_empty()).then_some(joined)
}

/// Refuse a directory that already holds a vault, per §Why the vault directory
/// has to be empty.
///
/// # Errors
/// [`RecoverError::NotEmpty`].
pub fn require_empty(vault_dir: &Path) -> Result<(), RecoverError> {
    if vault_dir.join(VAULT_DB_FILE).exists() {
        return Err(RecoverError::NotEmpty(vault_dir.to_path_buf()));
    }
    Ok(())
}

/// Fetch the account record and the sealed blob, and open it with `code`.
///
/// Steps 3 to 5, and the only part of a recovery that talks to the relay
/// before a vault exists. `bearer` must carry a completed OIDC step-up: the
/// blob route refuses an ordinary session with `403 AUTH_STEP_UP_REQUIRED`,
/// which is the whole reason `login::step_up_login` exists.
///
/// # Errors
/// [`RecoverError::Relay`] for a refusal or a transport failure — the step-up
/// `403` included, which is reported with what to do about it —
/// [`RecoverError::Code`] for a code that does not open the blob.
pub async fn restore_identity(
    base_url: &str,
    bearer: &str,
    code: &str,
) -> Result<RecoveryPayload, RecoverError> {
    let client = sunrise_relay_client::api::Client::new(base_url)
        .map_err(|e| RecoverError::Relay(e.to_string()))?
        .with_credential(
            "AccountToken",
            sunrise_relay_client::api::Credential::Bearer(
                sunrise_relay_client::api::SecretString::from(bearer.to_owned()),
            ),
        );

    let account = client
        .get_account(None)
        .await
        .map_err(|e| RecoverError::Relay(format!("could not read the account: {e}")))?
        .into_inner();
    let identity_id = identity_id_from_account(account.identity_signing_pub.as_deref())?;

    let served = client
        .get_recovery_blob(None)
        .await
        .map_err(|e| {
            let detail = e.to_string();
            if detail.contains("403") {
                RecoverError::Relay(format!(
                    "the relay will not serve the recovery blob to this session: {detail}. \
                     It requires a fresh sign-in (an OIDC step-up), which is what \
                     `sunrise recover` performs when it runs the login itself — so this \
                     means the token in {} is an ordinary one. Unset it and let this \
                     command sign you in.",
                    livesync::ENV_SYNC_TOKEN
                ))
            } else {
                RecoverError::Relay(format!("could not fetch the recovery blob: {detail}"))
            }
        })?
        .into_inner()
        .recovery_blob;

    let blob = sunrise_onboarding::decode_recovery_blob(&served)
        .map_err(|e| RecoverError::Relay(format!("the relay served a malformed blob: {e}")))?;

    Ok(sunrise_onboarding::recover_identity_from_code(
        &blob,
        code,
        &identity_id,
    )?)
}

/// Steps 6 and 8: build the vault, join the account, and replay the log.
///
/// The vault root is minted here and nowhere earlier. It is not in the blob and
/// could not be — `docs/03-crypto/recovery.md` §Device backups do not carry the
/// vault root is the guarantee that makes that true — so a recovering device
/// makes its own, and `vault::resolve` registers it in the keystore like any
/// other, which is what puts the restored account in `sunrise vaults`.
///
/// `Core::open` does the rest of step 6 without being asked: `Keychain::create`
/// mints fresh `D_S`/`D_D` and issues a `DeviceCert` signed by the **restored**
/// `ID_S_priv`, and `Engine::publish_device_cert` emits it as an op so every
/// replica learns this device.
///
/// # Errors
/// [`RecoverError::Vault`], [`RecoverError::Core`], [`RecoverError::Relay`] if
/// the relay refuses the registration, and [`RecoverError::NeverCaughtUp`] if
/// the replay does not finish.
pub async fn rebuild_vault(
    vault_dir: &Path,
    identity: RecoveryPayload,
    base_url: &str,
    bearer: &str,
    app: &str,
    announce: &mut dyn FnMut(&str),
) -> Result<Recovered, RecoverError> {
    let identity_id = hex_16(&identity.identity_id);
    let root = vault::resolve(vault_dir, &SystemRng)?;

    let mut cfg = CoreConfig::production(vault_dir.to_path_buf(), app.to_owned());
    cfg.sync = Some(sunrise_core::SyncConfig::new(base_url.to_owned()));
    let core = std::sync::Arc::new(
        Core::open(
            cfg,
            Unlock::RecoveryCode {
                root: VaultRootKey::from_bytes(root),
                identity: Box::new(identity),
            },
        )
        .await?,
    );
    announce("vault rebuilt from the recovery code");

    // Step 6, second half: publish this device to the account so its siblings
    // learn it. `register_device` and not `bootstrap`: the account already
    // exists and its identity keys are the ones this vault just restored, the
    // recovery-blob column is write-once and already holds the blob this
    // recovery spent, and `POST /accounts` wants an email a recovering client
    // has no business inventing.
    let relay_device_id = sunrise_relay_client::register_device(
        base_url,
        bearer,
        sunrise_relay_client::DeviceIdentity {
            device_pub_s: core.device_signing_pub(),
            device_pub_d: None,
            // The cert this device just issued itself under the restored
            // identity. `sunrise bootstrap` sends `None` because a founding
            // device's cert tells a relay nothing it does not already hold; a
            // recovering one is joining an account that may still have
            // siblings, and this is the copy they can be shown.
            device_cert: Some(sunrise_onboarding::encode_recovery_blob(
                &core.device_cert(),
            )),
            vault_device_id: Some(sunrise_id::crockford::encode_bytes(&core.device_id())),
            nickname: "sunrise-cli".to_owned(),
            platform: platform(),
            app_version: Some(app.to_owned()),
        },
    )
    .await
    .map_err(|e| RecoverError::Relay(e.to_string()))?;

    livesync::save_relay_device_id(vault_dir, &relay_device_id).map_err(|e| {
        RecoverError::Relay(format!(
            "registered as {relay_device_id} but could not record it: {e}"
        ))
    })?;
    announce(&format!("re-keyed as device {relay_device_id}"));

    // Step 8. The replay needs no walker of its own: every identity-addressed
    // `key_envelope` arrives as an ordinary op, `Engine::apply_control_op`
    // opens it with the restored `ID_D_priv`, and each absorbed key drains the
    // ops that were parked in `deferred_ops` waiting for it. What this command
    // adds is the *waiting* — reporting success before the log has been read
    // would hand the user an empty vault and call it a recovery.
    let plan = livesync::SyncPlan {
        sync: Some(
            sunrise_core::SyncConfig::new(base_url.to_owned())
                .with_credential(sunrise_core::TokenSource::new(Some(bearer.to_owned()))),
        ),
        export_pairing: None,
        adopt_pairing: None,
        device_id: Some(relay_device_id.clone()),
    };
    for line in livesync::apply_plan(&core, &plan) {
        announce(&line);
    }
    let caught_up = wait_until_live(&core, CATCH_UP_MS).await;
    core.shutdown().await;
    caught_up?;

    Ok(Recovered {
        identity_id,
        relay_device_id,
        vault_dir: vault_dir.to_path_buf(),
    })
}

/// Wait for the sync driver to report `Live` with nothing pending.
///
/// Timed against the core's injected clock rather than `Instant`, which the
/// workspace lint bans so that time is never read from two sources — the same
/// rule `sync --once` follows.
async fn wait_until_live(core: &Core, budget_ms: u64) -> Result<(), RecoverError> {
    /// How often to re-read the status.
    const POLL: std::time::Duration = std::time::Duration::from_millis(200);
    let deadline = core.now_ms().saturating_add(budget_ms);
    let mut last = String::new();
    while core.now_ms() < deadline {
        let sunrise_core::QueryResult::SyncStatus(s) = core
            .query(sunrise_core::Query::SyncStatus)
            .await
            .map_err(|e| RecoverError::NeverCaughtUp(e.to_string()))?
        else {
            return Err(RecoverError::NeverCaughtUp(
                "unexpected query result".into(),
            ));
        };
        if s.state == sunrise_sync::SyncState::Live && s.outbox_pending == 0 {
            return Ok(());
        }
        last = format!("{:?}, {} pending", s.state, s.outbox_pending);
        tokio::time::sleep(POLL).await;
    }
    Err(RecoverError::NeverCaughtUp(format!(
        "still {last} after {}s",
        budget_ms / 1000
    )))
}

/// What §Recovery flow step 7 asks the client to say, said.
///
/// Advice and not an action: a recovery is by definition an unknown-state
/// environment, and only the user knows which of the devices on this account
/// they still have. It is printed last, because it is about a vault that now
/// exists.
#[must_use]
pub fn aftercare(identity_id: &str) -> String {
    format!(
        "Account {identity_id} is restored on this device.\n\
         \n\
         Two things are worth doing now, and neither is automatic:\n\
         \n\
           * Revoke the devices you lost. Until you do, anything still holding\n\
            them can read what this account writes.\n\
         \n\
         * Rotate your Stream keys. A recovery means an unknown-state\n\
            environment; rotation is what bounds what a lost device keeps\n\
            reading. See docs/03-crypto/key-rotation.md.\n\
         \n\
         Your recovery code still works and has not changed. This device now\n\
         holds ID_D_priv, so it can seal a new one if you ever rotate it."
    )
}

/// Lower-case hex of a 16-byte id.
fn hex_16(id: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    id.iter().fold(String::with_capacity(32), |mut out, b| {
        // Infallible: writing to a `String` cannot fail.
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// This build's platform tag, as `POST /api/v1/devices` names them.
fn platform() -> String {
    if cfg!(target_os = "macos") {
        "macos".to_owned()
    } else if cfg!(target_os = "windows") {
        "windows".to_owned()
    } else {
        "linux".to_owned()
    }
}

/// Obtain a bearer that carries a completed step-up.
///
/// `SUNRISE_SYNC_TOKEN` wins where it is set, because that is the CI and
/// self-host override every other command honours and a self-host relay is
/// exempt from the step-up entirely (`NullVerifier` is single-tenant, so there
/// is no second account for a stolen session to reach). Otherwise this signs
/// the user in *now*, with `max_age=0`, because a stored login's `auth_time` is
/// whenever it happened and the blob route reads exactly that.
///
/// # Errors
/// [`RecoverError::Relay`] carrying what the IdP or the configuration said.
pub async fn step_up_bearer(
    vault_dir: &Path,
    now_ms: u64,
    announce: &mut dyn FnMut(&str),
) -> Result<String, RecoverError> {
    if let Ok(token) = std::env::var(livesync::ENV_SYNC_TOKEN) {
        let token = token.trim().to_owned();
        if !token.is_empty() {
            announce(&format!("using the bearer in {}", livesync::ENV_SYNC_TOKEN));
            return Ok(token);
        }
    }
    let cfg = login::LoginConfig::from_env().map_err(|e| {
        RecoverError::Relay(format!(
            "{e}; set {} and {}, or set {} to a token that carries a fresh sign-in",
            login::ENV_ISSUER,
            login::ENV_CLIENT_ID,
            livesync::ENV_SYNC_TOKEN
        ))
    })?;
    announce("signing in — your provider will ask you to authenticate again, which is what lets the relay release the recovery blob");
    // The device id claim is empty: this device does not exist yet, and the
    // vault that will name it is not created until the blob has been opened.
    let creds = login::step_up_login(&cfg, "", &login::store_for(vault_dir), now_ms, announce)
        .await
        .map_err(|e| RecoverError::Relay(e.to_string()))?;
    Ok(creds.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_id_is_derived_from_the_accounts_published_key() {
        // The founding device's key, through the encoding the wire uses.
        let key = [0x11u8; 32];
        let encoded = sunrise_onboarding::encode_public_key(&key);
        assert_eq!(
            identity_id_from_account(Some(&encoded)).unwrap(),
            sunrise_crypto::identity_id_from_pub(&key),
            "the AAD a recovering client computes must be the one the founder sealed under"
        );
    }

    /// The two ways an account can fail to be recoverable, kept apart because
    /// the advice differs: one is "nobody has bootstrapped this account", the
    /// other is "the relay answered with something that is not a key".
    #[test]
    fn an_account_with_no_usable_identity_key_is_refused_as_such() {
        assert!(matches!(
            identity_id_from_account(None),
            Err(RecoverError::NoIdentityKey)
        ));
        assert!(matches!(
            identity_id_from_account(Some("   ")),
            Err(RecoverError::NoIdentityKey)
        ));
        assert!(matches!(
            identity_id_from_account(Some("not base64url!!")),
            Err(RecoverError::IdentityKey(_))
        ));
        // Right alphabet, wrong length — the case a length check that padded
        // would turn into a silently wrong AAD.
        let short = sunrise_onboarding::encode_recovery_blob(&[0u8; 16]);
        assert!(matches!(
            identity_id_from_account(Some(&short)),
            Err(RecoverError::IdentityKey(
                sunrise_onboarding::PublicKeyError::Length(16)
            ))
        ));
    }

    #[test]
    fn a_code_is_taken_from_the_arguments_or_from_stdin() {
        let args: Vec<String> = "abandon ability able"
            .split(' ')
            .map(str::to_owned)
            .collect();
        assert_eq!(
            code_from(&args, "").as_deref(),
            Some("abandon ability able")
        );
        // One quoted argument is the same as three bare ones.
        assert_eq!(
            code_from(&["abandon ability able".to_owned()], "").as_deref(),
            Some("abandon ability able")
        );
        // Piped, with the newline and the ragged spacing a paste carries.
        assert_eq!(
            code_from(&[], "  abandon   ability\nable \n").as_deref(),
            Some("abandon ability able")
        );
        // Arguments win, so a stray pipe cannot silently replace what was typed.
        assert_eq!(
            code_from(&["abandon".to_owned()], "zoo zoo").as_deref(),
            Some("abandon")
        );
        assert_eq!(code_from(&[], "   \n "), None);
    }

    #[test]
    fn a_directory_that_already_holds_a_vault_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        require_empty(dir.path()).expect("an empty directory is fine");
        std::fs::write(dir.path().join(VAULT_DB_FILE), b"not really a vault").unwrap();
        let err = require_empty(dir.path()).unwrap_err();
        assert!(matches!(err, RecoverError::NotEmpty(_)));
        assert!(
            err.to_string().contains(&dir.path().display().to_string()),
            "the message has to name the directory the user must change: {err}"
        );
    }

    #[test]
    fn the_aftercare_says_both_halves_of_step_seven() {
        let text = aftercare("abc");
        assert!(text.contains("Revoke"), "{text}");
        assert!(text.contains("Rotate"), "{text}");
        assert!(text.contains("abc"));
    }
}
