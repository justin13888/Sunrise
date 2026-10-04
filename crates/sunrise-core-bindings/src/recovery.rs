//! Restoring an account from its recovery code, across the seam.
//!
//! [`crate::SunriseCore::recover_account`] runs
//! `sunrise_onboarding::recover_account`, the same flow `sunrise recover` runs,
//! over the same relay adapter. This module holds what the seam adds: the
//! progress events in a shape Swift can switch on, the listener they are
//! delivered through, the per-word check a 24-word field runs as the user
//! types, and the mapping from the flow's errors onto [`BindingError`]
//! variants a client branches on.
//!
//! Nothing secret crosses back. The 24 words go in once and are dropped on
//! this side; the identity keys they open never leave Rust.

use std::sync::Arc;

use sunrise_onboarding::{RecoveryFlowError, RecoveryProgress, RestoreError};

use crate::BindingError;

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
            RecoveryProgress::Replaying { applied } => Self::Replaying { applied },
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

/// Whether `word` is a word a recovery code can contain.
///
/// What a 24-word field checks as each word is typed, so a typo is marked at
/// the word holding it before anything is sent anywhere. Case and surrounding
/// whitespace are ignored. A field of valid words can still fail the checksum;
/// [`check_recovery_code`] says that.
#[uniffi::export]
#[must_use]
pub fn is_recovery_word(word: String) -> bool {
    sunrise_onboarding::is_recovery_word(&word)
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
    sunrise_onboarding::check_recovery_code(&code)
        .map_err(|e| BindingError::RecoveryCode(e.to_string()))
}

/// Deliver the flow's progress to the foreign listener.
pub(crate) fn forward(listener: Arc<dyn RecoveryListener>) -> impl FnMut(RecoveryProgress) + Send {
    move |p| listener.on_step(RecoveryStep::from(p))
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
            RestoreError::Code(RecoveryFlowError::Code(_) | RecoveryFlowError::Crypto(_)) => {
                Self::RecoveryCode(message)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

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
