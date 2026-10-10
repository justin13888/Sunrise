//! `sunrise recover` — spend a recovery code and come back with a vault.
//!
//! `docs/03-crypto/recovery.md` §Recovery flow is eight steps. Steps 2-5 were
//! already built and reachable (`sunrise_onboarding::recover_identity_from_code`
//! opens the blob and hands back the identity); what had no caller was
//! everything that turns an opened blob into something a user can use, and an
//! opened blob is not a vault.
//!
//! Steps 3 to 8 are not here. They are `sunrise_onboarding::recovery`, which
//! the UniFFI seam's `recover_account` runs too, over the same relay adapter
//! (`sunrise_relay_client::RelayRecovery`). What stays in this module is what
//! only a command line has: where the words come from, which directory the
//! vault goes in, where its root is kept, the step-up sign-in, and the advice
//! printed at the end.
//!
//! ```text
//!  step 1   the user points this at an empty vault directory
//!  steps 3-5 sunrise_onboarding::restore_identity
//!  step 6   mint a fresh vault root in the keystore, then
//!  step 8   sunrise_onboarding::rejoin_account opens the vault from the
//!           restored identity, registers it, and waits for the replay
//!  step 7   tell the user to revoke the devices they lost and rotate their
//!           Stream keys — last, because it is advice about a vault that now
//!           exists
//! ```
//!
//! The root is minted only once the code has opened the blob, so a mistyped
//! code leaves nothing in the keystore.
//!
//! # Why the vault directory has to be empty
//!
//! The identity a vault belongs to is decided when it is created and cannot be
//! changed afterwards (`Keychain::open`'s case 4 loads what is on disk and
//! never replaces it). Recovering *into* an existing vault would therefore
//! either fail with `IdentityConflict` or, worse, quietly leave the old
//! identity in place — so this refuses up front, with the directory named.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sunrise_core::{Core, CoreConfig, SystemRng};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_onboarding::{RecoveryProgress, RestoreError};
use sunrise_relay_client::RelayRecovery;

use crate::i18n::strings;
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
/// acceptable message: each of these names the next action. The shared flow's
/// arms arrive as [`RecoverError::Restore`], worded for this command line.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// The target directory already holds a vault.
    #[error("{}", strings::recover::not_empty(&.0.display().to_string()))]
    NotEmpty(PathBuf),
    /// No relay origin.
    #[error("{}", strings::bootstrap::needs_relay(livesync::ENV_SYNC_URL))]
    NoRelay,
    /// No recovery code on the command line and nothing on stdin.
    #[error("{}", strings::recover::needs_code())]
    NoCode,
    /// The step-up sign-in could not be run, or the relay origin is unusable.
    #[error("{0}")]
    Relay(String),
    /// The vault root could not be minted or stored.
    #[error(transparent)]
    Vault(#[from] vault::VaultError),
    /// The shared flow refused: a code, a relay, or a vault failure.
    #[error("{}", cli_wording(.0))]
    Restore(#[from] RestoreError),
}

/// The shared flow's errors, with the next step this command line offers.
///
/// Only two need it. A step-up refusal here means the bearer came from
/// `SUNRISE_SYNC_TOKEN`, because when this command signs in itself it asks for
/// the step-up; and a replay that did not finish is resumed by a command the
/// shared flow cannot name.
fn cli_wording(e: &RestoreError) -> String {
    match e {
        RestoreError::StepUpRequired(detail) => {
            strings::recover::step_up_required(detail, livesync::ENV_SYNC_TOKEN)
        }
        RestoreError::NeverCaughtUp { .. } => strings::recover::never_caught_up(&e.to_string()),
        other => other.to_string(),
    }
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

/// The relay at `base_url`, as the shared flow reads it.
fn relay(base_url: &str, bearer: &str) -> Result<RelayRecovery, RecoverError> {
    RelayRecovery::new(base_url, bearer).map_err(|e| RecoverError::Relay(e.to_string()))
}

/// Fetch the account record and the sealed blob, and open it with `code`:
/// steps 3 to 5, through [`sunrise_onboarding::restore_identity`].
///
/// `bearer` must carry a completed OIDC step-up: the blob route refuses an
/// ordinary session with `403 AUTH_STEP_UP_REQUIRED`, which is the whole reason
/// `login::step_up_login` exists.
///
/// # Errors
/// [`RecoverError::Restore`] for a code that does not open the blob and for
/// every relay refusal, the step-up `403` included, which is worded with what
/// to do about it.
pub async fn restore_identity(
    base_url: &str,
    bearer: &str,
    code: &str,
) -> Result<RecoveryPayload, RecoverError> {
    let relay = relay(base_url, bearer)?;
    Ok(sunrise_onboarding::restore_identity(&relay, code, &mut |_| {}).await?)
}

/// What [`rejoin_account`]'s two callbacks share while the flow runs.
#[derive(Default)]
struct Rejoining {
    /// Why the relay's id for this device was not recorded, if it was not.
    unrecorded: Option<String>,
    /// Lines the sync starter produced that are not printed yet.
    lines: Vec<String>,
}

/// Steps 6 and 8: mint the vault root, then build the vault from the restored
/// identity, register it, and replay the log, through
/// [`sunrise_onboarding::rejoin_account`].
///
/// Named for what it does to the account rather than `rebuild_vault`, which
/// is what rebuilding a vault's projection from its own op log is called
/// (#327).
///
/// The vault root is minted here and nowhere earlier. It is not in the blob and
/// could not be — `docs/03-crypto/recovery.md` §Device backups do not carry the
/// vault root is the guarantee that makes that true — so a recovering device
/// makes its own, and `vault::resolve` registers it in the keystore like any
/// other, which is what puts the restored account in `sunrise vaults`.
///
/// The relay's id for this device is recorded as soon as the relay mints it,
/// before the replay, so a replay that does not finish leaves a vault that
/// `sunrise sync --once` can resume. An id that could not be recorded stops
/// the command before sync starts, since there would be nothing to resume.
/// What starting sync says ("sync driver started", or why it did not) is
/// printed, as every command prints it.
///
/// # Errors
/// [`RecoverError::Vault`] for the root, and [`RecoverError::Restore`] for a
/// vault that would not open, a registration the relay refused, and a replay
/// that did not finish. [`RecoverError::Relay`] when the relay's id for this
/// device could not be recorded.
pub async fn rejoin_account(
    vault_dir: &Path,
    identity: RecoveryPayload,
    base_url: &str,
    bearer: &str,
    app: &str,
    announce: &mut (dyn FnMut(&str) + Send),
) -> Result<Recovered, RecoverError> {
    let relay = relay(base_url, bearer)?;
    let root = vault::resolve(vault_dir, &SystemRng)?;

    let mut cfg = CoreConfig::production(vault_dir.to_path_buf(), app.to_owned());
    cfg.sync = Some(sunrise_core::SyncConfig::new(base_url.to_owned()));

    // Shared by the two callbacks: the progress callback records whether the
    // relay's id was saved, and the sync starter reads that and leaves the
    // lines `apply_plan` says for the progress callback to print in order.
    let shared = std::sync::Mutex::new(Rejoining::default());
    let shared_ref = &shared;
    let plan_url = base_url.to_owned();
    let plan_bearer = bearer.to_owned();
    let start_sync = move |core: &Arc<Core>, relay_device_id: &str| {
        let mut state = shared_ref
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An id that was not recorded would leave a vault `sunrise sync
        // --once` cannot resume, so the replay is not worth waiting for.
        if let Some(detail) = &state.unrecorded {
            return Err(detail.clone());
        }
        let plan = livesync::SyncPlan {
            sync: Some(
                sunrise_core::SyncConfig::new(plan_url.clone())
                    .with_credential(sunrise_core::TokenSource::new(Some(plan_bearer.clone()))),
            ),
            device_id: Some(relay_device_id.to_owned()),
        };
        // "sync driver started", or why it did not: what every command prints
        // after `apply_plan`. A driver that did not start also shows up as a
        // replay that never catches up, which is reported.
        state.lines.extend(livesync::apply_plan(core, &plan));
        Ok(())
    };
    let drain = |announce: &mut (dyn FnMut(&str) + Send)| {
        let lines = std::mem::take(
            &mut shared_ref
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .lines,
        );
        for line in &lines {
            announce(line);
        }
    };

    let outcome = sunrise_onboarding::rejoin_account(
        &relay,
        cfg,
        VaultRootKey::from_bytes(root),
        identity,
        sunrise_onboarding::DeviceLabel {
            nickname: "sunrise-cli".to_owned(),
            platform: platform(),
            app_version: Some(app.to_owned()),
        },
        &start_sync,
        CATCH_UP_MS,
        &mut |step| {
            drain(&mut *announce);
            match step {
                RecoveryProgress::DeviceRegistered { relay_device_id } => {
                    announce(&strings::recover::vault_rebuilt());
                    match livesync::save_relay_device_id(vault_dir, &relay_device_id) {
                        Ok(()) => announce(&strings::recover::rekeyed(&relay_device_id)),
                        Err(e) => {
                            shared_ref
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .unrecorded = Some(strings::recover::unrecorded(
                                &relay_device_id,
                                &e.to_string(),
                            ));
                        }
                    }
                }
                RecoveryProgress::Replaying { applied } => {
                    announce(&strings::recover::replaying(
                        i64::try_from(applied).unwrap_or(i64::MAX),
                    ));
                }
                _ => {}
            }
        },
    )
    .await;
    drain(&mut *announce);
    let unrecorded = shared
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unrecorded;
    if let Some(detail) = unrecorded {
        return Err(RecoverError::Relay(detail));
    }
    let done = outcome?;

    Ok(Recovered {
        identity_id: done.identity_id,
        relay_device_id: done.relay_device_id,
        vault_dir: vault_dir.to_path_buf(),
    })
}

/// What §Recovery flow step 7 asks the client to say, said.
///
/// Advice and not an action: a recovery is by definition an unknown-state
/// environment, and only the user knows which of the devices on this account
/// they still have. It is printed last, because it is about a vault that now
/// exists.
#[must_use]
pub fn aftercare(identity_id: &str) -> String {
    strings::recover::aftercare(identity_id)
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
            announce(&strings::recover::using_bearer(livesync::ENV_SYNC_TOKEN));
            return Ok(token);
        }
    }
    let cfg = login::LoginConfig::from_env().map_err(|e| {
        RecoverError::Relay(strings::recover::needs_config(
            &e,
            login::ENV_ISSUER,
            login::ENV_CLIENT_ID,
            livesync::ENV_SYNC_TOKEN,
        ))
    })?;
    announce(&strings::recover::signing_in());
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

    /// The two shared-flow errors this command words for itself carry the
    /// next step only a command line can name.
    #[test]
    fn a_step_up_refusal_and_an_unfinished_replay_name_the_next_command() {
        let step_up = RecoverError::from(RestoreError::StepUpRequired("403".to_owned()));
        assert!(
            step_up.to_string().contains(livesync::ENV_SYNC_TOKEN),
            "{step_up}"
        );
        let replay = RecoverError::from(RestoreError::NeverCaughtUp {
            detail: "still CatchingUp".to_owned(),
            relay_device_id: "01X".to_owned(),
        });
        assert!(
            replay.to_string().contains("sunrise sync --once"),
            "{replay}"
        );
        let other = RecoverError::from(RestoreError::NoIdentityKey);
        assert_eq!(other.to_string(), RestoreError::NoIdentityKey.to_string());
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
