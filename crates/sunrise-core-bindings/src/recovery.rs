//! Restoring an account from its recovery code, across the seam.
//!
//! [`crate::SunriseCore::recover_account`] runs
//! `sunrise_client_core::recovery::recover_account`, the same flow `sunrise recover` runs,
//! over the same relay adapter. This module holds that constructor and what
//! the seam adds: the progress events in a shape Swift can switch on, the
//! listener they are delivered through, the per-word check a 24-word field
//! runs as the user types, and the mapping from the flow's errors onto
//! [`BindingError`] variants a client branches on.
//!
//! Nothing secret crosses back. The 24 words go in once and are dropped on
//! this side; the identity keys they open never leave Rust.

use std::path::PathBuf;
use std::sync::Arc;

use sunrise_client_core::recovery::{RecoveryFlowError, RecoveryProgress, RestoreError};
use sunrise_core::{Core, CoreConfig, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use zeroize::Zeroize as _;

use crate::account::device_label;
use crate::{background, sse_factory, vault_root_bytes, BindingError, SunriseCore};
#[cfg(doc)]
use crate::{recovery, SunriseLogin};

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Restore an account from its recovery code into `vault_dir`, a directory
    /// holding no vault, and return the vault open.
    ///
    /// `docs/03-crypto/recovery.md` §Recovery flow steps 3 to 8, by the same
    /// implementation `sunrise recover` runs
    /// (`sunrise_client_core::recovery::recover_account`): fetch the account's identity key and its sealed
    /// blob, open the blob with `code`, create the vault from the restored
    /// identity under `vault_root`, register this device with the relay as
    /// `nickname`, and wait up to `catch_up_ms` for the history to replay.
    /// It returns only once the replay has caught up. A vault that opens empty
    /// is not a recovery.
    ///
    /// `step_up_bearer` must carry a fresh sign-in, which
    /// [`SunriseLogin::begin_step_up`] produces. The blob route refuses any
    /// other, as [`BindingError::StepUpRequired`].
    ///
    /// `vault_root` is the caller's, for the reason [`Self::open`]'s is: it
    /// comes from the platform keychain, and this seam does not derive it. It
    /// is new, because a root is never in a recovery blob. Store it once this
    /// returns, or once it fails with [`BindingError::RecoveryIncomplete`],
    /// which is the one failure that has written the vault. After any other
    /// failure, keeping the root would let the next open mint a fresh, empty
    /// account in the directory.
    ///
    /// `listener` hears each [`recovery::RecoveryStep`]. The
    /// [`recovery::RecoveryStep::DeviceRegistered`] step carries the relay
    /// device id the caller must record before starting sync.
    ///
    /// The returned vault is not syncing. Start it with [`Self::start_sync`]
    /// as on any launch.
    ///
    /// # Errors
    ///
    /// [`BindingError::BadVaultRoot`], [`BindingError::RecoveryCode`],
    /// [`BindingError::StepUpRequired`], [`BindingError::RecoveryRefused`]
    /// (also for a directory that already holds a vault), and
    /// [`BindingError::RecoveryIncomplete`] (also for a restored vault that
    /// does not reopen).
    #[uniffi::constructor(default(catch_up_ms = 60000))]
    #[allow(clippy::too_many_arguments)]
    pub async fn recover_account(
        vault_dir: String,
        vault_root: Vec<u8>,
        app_version: String,
        relay_url: String,
        step_up_bearer: String,
        code: String,
        nickname: String,
        listener: Arc<dyn RecoveryListener>,
        catch_up_ms: u64,
    ) -> Result<Arc<Self>, BindingError> {
        recover_account_reporting(
            RecoveryRequest {
                vault_dir,
                vault_root,
                app_version,
                relay_url,
                step_up_bearer,
                code,
                nickname,
                catch_up_ms,
            },
            forward(listener, None),
        )
        .await
    }

    /// [`Self::recover_account`], with the replay's progress also reported
    /// to `replay` as n of N where the relay states N (#459).
    ///
    /// `listener` hears every step as it does from [`Self::recover_account`],
    /// [`recovery::RecoveryStep::Replaying`] included. `replay` hears each of
    /// those replay steps again as a [`recovery::ReplayProgress`], which
    /// carries the total. A constructor of its own, not a new field or
    /// listener method, because either of those would change the generated
    /// Swift the app already compiles against.
    ///
    /// # Errors
    ///
    /// As [`Self::recover_account`].
    #[uniffi::constructor(default(catch_up_ms = 60000))]
    #[allow(clippy::too_many_arguments)]
    pub async fn recover_account_with_replay(
        vault_dir: String,
        vault_root: Vec<u8>,
        app_version: String,
        relay_url: String,
        step_up_bearer: String,
        code: String,
        nickname: String,
        listener: Arc<dyn RecoveryListener>,
        replay: Arc<dyn ReplayListener>,
        catch_up_ms: u64,
    ) -> Result<Arc<Self>, BindingError> {
        recover_account_reporting(
            RecoveryRequest {
                vault_dir,
                vault_root,
                app_version,
                relay_url,
                step_up_bearer,
                code,
                nickname,
                catch_up_ms,
            },
            forward(listener, Some(replay)),
        )
        .await
    }
}

/// How far a recovery has got. Delivered in order, each at most once except
/// [`RecoveryStep::Replaying`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryStep {
    /// The relay served the sealed recovery blob.
    BlobFetched,
    /// The 24 words opened it.
    IdentityOpened,
    /// The vault exists and the relay registered this device.
    ///
    /// The caller records `relay_device_id` now, wherever it keeps the relay
    /// device id: the relay never sends it again, and a recovery that ends in
    /// [`BindingError::RecoveryIncomplete`] still needs it to sync later.
    DeviceRegistered {
        /// The id the relay minted.
        relay_device_id: String,
    },
    /// The account's history is being replayed into the vault. `applied`
    /// counts the changes so far; the relay does not say how many there are
    /// before it has sent them, so there is no total.
    ///
    /// Since #459 each change counted is an op the replay delivered, and a
    /// relay that states its backlog does give a total. It is not on this
    /// variant, because a field here would change the generated Swift and the
    /// seam changes additively only: it reaches a [`ReplayListener`] given to
    /// [`crate::SunriseCore::recover_account_with_replay`].
    Replaying {
        /// Changes applied so far.
        applied: u64,
    },
    /// The replay has caught up.
    CaughtUp,
}

impl From<RecoveryProgress> for RecoveryStep {
    fn from(p: RecoveryProgress) -> Self {
        match p {
            RecoveryProgress::BlobFetched => Self::BlobFetched,
            RecoveryProgress::IdentityOpened => Self::IdentityOpened,
            RecoveryProgress::DeviceRegistered { relay_device_id } => {
                Self::DeviceRegistered { relay_device_id }
            }
            RecoveryProgress::Replaying { applied, .. } => Self::Replaying { applied },
            RecoveryProgress::CaughtUp => Self::CaughtUp,
        }
    }
}

/// The foreign side of a recovery's progress.
///
/// Called from a runtime worker thread, never the foreign main thread, so an
/// implementation hops to its UI thread itself.
#[uniffi::export(with_foreign)]
pub trait RecoveryListener: Send + Sync + 'static {
    /// One step happened.
    fn on_step(&self, step: RecoveryStep);
}

/// How far a recovery's replay has got (#459): n of N where the relay states
/// N, and n alone where it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ReplayProgress {
    /// Ops delivered so far.
    pub applied: u64,
    /// `applied` plus the ops the relay has stated it will still send.
    /// `None` against a relay that states no backlog. It can grow while the
    /// replay runs, as the ops it delivers name streams the new vault then
    /// subscribes to.
    pub total: Option<u64>,
}

/// The foreign side of a replay's progress, beside [`RecoveryListener`].
///
/// Called from a runtime worker thread, never the foreign main thread.
#[uniffi::export(with_foreign)]
pub trait ReplayListener: Send + Sync + 'static {
    /// The replay moved.
    fn on_replay(&self, progress: ReplayProgress);
}

/// Whether `word` is a word a recovery code can contain.
///
/// What a 24-word field checks as each word is typed, so a typo is marked at
/// the word holding it before anything is sent anywhere. Case and surrounding
/// whitespace are ignored. A field of valid words can still fail the checksum;
/// [`check_recovery_code`] says that.
#[uniffi::export]
#[must_use]
pub fn is_recovery_word(word: String) -> bool {
    sunrise_client_core::recovery::is_recovery_word(&word)
}

/// Check a whole typed code without spending it: the count, every word, and
/// the checksum.
///
/// Run it before the step-up sign-in, so a code that could never open the blob
/// does not cost the user a sign-in first.
///
/// # Errors
/// [`BindingError::RecoveryCode`], whose message names the position of a bad
/// word and never the word itself.
#[uniffi::export]
pub fn check_recovery_code(code: String) -> Result<(), BindingError> {
    sunrise_client_core::recovery::check_recovery_code(&code)
        .map_err(|e| BindingError::RecoveryCode(e.to_string()))
}

/// Deliver the flow's progress to the foreign listeners: every step to
/// `listener`, and each replay step's count and total to `replay` too.
pub(crate) fn forward(
    listener: Arc<dyn RecoveryListener>,
    replay: Option<Arc<dyn ReplayListener>>,
) -> impl FnMut(RecoveryProgress) + Send {
    move |p| {
        if let (Some(replay), RecoveryProgress::Replaying { applied, total }) = (&replay, &p) {
            replay.on_replay(ReplayProgress {
                applied: *applied,
                total: *total,
            });
        }
        listener.on_step(RecoveryStep::from(p));
    }
}

/// Map the flow's errors onto the variants a client branches on.
///
/// The split is by what the client does next, and by whether the vault
/// directory was written, which is what decides whether the client keeps the
/// root it handed in.
impl From<RestoreError> for BindingError {
    fn from(e: RestoreError) -> Self {
        let message = e.to_string();
        if e.vault_written() {
            return Self::RecoveryIncomplete(message);
        }
        match e {
            RestoreError::Code(RecoveryFlowError::Code(_) | RecoveryFlowError::Crypto(_))
            | RestoreError::CodeRetired { .. } => Self::RecoveryCode(message),
            RestoreError::StepUpRequired(_) => Self::StepUpRequired(message),
            _ => Self::RecoveryRefused(message),
        }
    }
}

/// The reopen after a recovery that succeeded did not open.
///
/// [`BindingError::RecoveryIncomplete`] and never a plain core error: the
/// vault under the restored identity is on disk and the device is registered,
/// so the caller keeps the root it handed in. Discarding it would leave the
/// restored vault unreadable.
pub(crate) fn reopen_failed(e: sunrise_core::CoreError) -> BindingError {
    BindingError::RecoveryIncomplete(format!("the vault was restored but did not reopen: {e}"))
}

/// What [`SunriseCore::recover_account`] and
/// [`SunriseCore::recover_account_with_replay`] take, less the listeners.
struct RecoveryRequest {
    vault_dir: String,
    vault_root: Vec<u8>,
    app_version: String,
    relay_url: String,
    step_up_bearer: String,
    code: String,
    nickname: String,
    catch_up_ms: u64,
}

/// The recovery both constructors run, reporting through `progress`.
async fn recover_account_reporting(
    request: RecoveryRequest,
    mut progress: impl FnMut(RecoveryProgress) + Send,
) -> Result<Arc<SunriseCore>, BindingError> {
    let RecoveryRequest {
        vault_dir,
        vault_root,
        app_version,
        relay_url,
        step_up_bearer,
        code,
        nickname,
        catch_up_ms,
    } = request;
    let root = vault_root_bytes(&vault_root)?;
    let dir = PathBuf::from(vault_dir);
    sunrise_client_core::recovery::check_recovery_directory(&dir)
        .map_err(|e| BindingError::RecoveryRefused(e.to_string()))?;
    let relay = sunrise_relay_client::RelayRecovery::new(&relay_url, &step_up_bearer)
        .map_err(|e| BindingError::RecoveryRefused(e.to_string()))?;

    let mut cfg = CoreConfig::production(dir.clone(), app_version.clone());
    cfg.sync = Some(sunrise_core::SyncConfig::new(relay_url.clone()));
    let bearer = step_up_bearer.clone();
    let url = relay_url.clone();
    let start_sync = move |core: &Arc<Core>, relay_device_id: &str| {
        core.sync_credential().set(Some(bearer.clone()));
        let signer = core.device_signer(relay_device_id);
        // A link of its own: this core is shut down before the handle the
        // caller gets is opened, and nothing ever kicks it.
        core.start_sync(sse_factory(&url, Some(signer), background::SyncLink::new()))
            .map_err(|e| e.to_string())
    };

    let mut code = code;
    let outcome = sunrise_client_core::recovery::recover_account(
        &relay,
        &code,
        cfg,
        VaultRootKey::from_bytes(root),
        device_label(nickname),
        &start_sync,
        catch_up_ms,
        &mut progress,
    )
    .await;
    code.zeroize();
    outcome?;

    // The flow shut its core down, so the vault lock is free: open it
    // again the way every later launch will, which is also the proof that
    // the root opens what the recovery wrote. The vault is written and
    // the device registered by now, so a failure here is still one whose
    // root the caller must keep.
    let core = Core::open(
        CoreConfig::production(dir, app_version),
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(root),
            paired: None,
        },
    )
    .await
    .map_err(reopen_failed)?;
    Ok(SunriseCore::wrap(core))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recovery into `dir` with a code that fails its checksum, which is the
    /// first thing the flow checks once the directory has passed.
    async fn recover_into(dir: &std::path::Path) -> BindingError {
        let request = RecoveryRequest {
            vault_dir: dir.to_string_lossy().into_owned(),
            vault_root: vec![7; 32],
            app_version: "test".into(),
            // Nothing listens here, and neither path below reaches it.
            relay_url: "http://127.0.0.1:9".into(),
            step_up_bearer: "bearer".into(),
            code: "abandon".into(),
            nickname: "phone".into(),
            catch_up_ms: 1_000,
        };
        match recover_account_reporting(request, |_| {}).await {
            Ok(_) => panic!("a one-word code cannot recover anything"),
            Err(e) => e,
        }
    }

    /// The seam runs the shared rule: a stray file gets as far as the code,
    /// and a written marker is refused before it.
    #[tokio::test]
    async fn a_stray_file_passes_and_a_vault_marker_is_refused() {
        let dir = tempfile::tempdir().expect("a vault dir");
        std::fs::write(dir.path().join(".DS_Store"), b"stray").expect("write");
        let err = recover_into(dir.path()).await;
        assert!(matches!(err, BindingError::RecoveryCode(_)), "{err:?}");

        std::fs::write(dir.path().join("vault-id"), b"anything\n").expect("write");
        let err = recover_into(dir.path()).await;
        assert!(
            matches!(&err, BindingError::RecoveryRefused(m) if m.contains("already holds a vault")),
            "{err:?}"
        );
    }

    #[test]
    fn a_reopen_that_failed_keeps_the_root() {
        let crossed = reopen_failed(sunrise_core::CoreError::Closed);
        assert!(
            matches!(crossed, BindingError::RecoveryIncomplete(_)),
            "{crossed:?}"
        );
    }

    #[test]
    fn each_error_crosses_as_what_the_client_does_next() {
        let code = BindingError::from(RestoreError::Code(RecoveryFlowError::Code(
            sunrise_crypto::bip39::Bip39Error::Checksum,
        )));
        assert!(matches!(code, BindingError::RecoveryCode(_)), "{code:?}");

        // Words that were right once: still the code's problem, not a refusal.
        for retired_at_ms in [Some(1_793_000_000_000), None] {
            let retired = BindingError::from(RestoreError::CodeRetired { retired_at_ms });
            assert!(
                matches!(&retired, BindingError::RecoveryCode(m) if m.contains("retired")),
                "{retired:?}"
            );
        }

        let step_up = BindingError::from(RestoreError::StepUpRequired("403".into()));
        assert!(
            matches!(step_up, BindingError::StepUpRequired(_)),
            "{step_up:?}"
        );

        let refused = BindingError::from(RestoreError::NoIdentityKey);
        assert!(
            matches!(refused, BindingError::RecoveryRefused(_)),
            "{refused:?}"
        );

        // Written, so the client must keep the root: never a refusal.
        for written in [
            RestoreError::Register("503".into()),
            RestoreError::NeverCaughtUp {
                detail: "still CatchingUp".into(),
                relay_device_id: "01X".into(),
            },
        ] {
            let crossed = BindingError::from(written);
            assert!(
                matches!(crossed, BindingError::RecoveryIncomplete(_)),
                "{crossed:?}"
            );
        }
    }

    #[test]
    fn a_typed_code_is_checked_without_quoting_it() {
        assert!(is_recovery_word("Abandon ".into()));
        assert!(!is_recovery_word("abandonn".into()));
        let err = check_recovery_code("abandon zebra".into()).expect_err("two words");
        assert!(!err.to_string().contains("zebra"), "{err}");
        let seed = [0x11u8; 32];
        let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
        check_recovery_code(code.reveal().to_owned()).expect("a real code");
    }
}
