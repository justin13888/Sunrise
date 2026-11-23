//! [`BindingError`], everything that can go wrong across the seam, and the
//! conversions into it that belong to no one concern.
//!
//! A conversion that does belong to one sits with it: the attachment errors in
//! [`crate::attachments`], the login errors in [`crate::login`], and the
//! recovery errors in [`crate::recovery`].

use sunrise_core::CoreError;
use thiserror::Error;

use crate::dto::hex16;

#[cfg(doc)]
use crate::{pairing_relay, vault_repair, SunriseCore, SunriseLogin};

/// Everything that can go wrong across the seam.
///
/// `#[uniffi(flat_error)]` because [`sunrise_core::CoreError`] is a tree of
/// `#[error(transparent)]` wrappers around six other crates' error types, and
/// UniFFI cannot model that structurally. Flattening keeps the *variant* —
/// which is what a client branches on — and carries the detail as the message,
/// which is what a client shows.
#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
#[non_exhaustive]
pub enum BindingError {
    /// The vault could not be opened, read or written.
    #[error("core: {0}")]
    Core(String),
    /// An id string was not a valid `EntityRef`.
    #[error("not an id: {id} ({cause})")]
    BadId {
        /// What was passed.
        id: String,
        /// Why it was rejected.
        cause: String,
    },
    /// A timestamp, date or time was not readable.
    #[error("not a time: {value} ({cause})")]
    BadTime {
        /// What was passed.
        value: String,
        /// Why it was rejected.
        cause: String,
    },
    /// A vault root key was not 32 bytes.
    #[error("vault root must be 32 bytes, got {len}")]
    BadVaultRoot {
        /// How many bytes arrived.
        len: u32,
    },
    /// The OIDC login flow failed, for any reason but
    /// [`BindingError::LoginRefused`]'s.
    #[error("login: {0}")]
    Login(String),
    /// The issuer refused the grant in OAuth's own words
    /// (`sunrise_auth::LoginError::Rejected`): a revoked or expired refresh
    /// token, or a login the user declined.
    ///
    /// Its own variant because a client does something different with it. A
    /// refused refresh token will be refused again, so the client drops it and
    /// asks the user to sign in; any other [`BindingError::Login`], an
    /// unreachable issuer above all, says nothing about the token, and dropping
    /// it there turns every offline launch into a browser sign-in. The message
    /// reads the same as [`BindingError::Login`]'s.
    #[error("login: {0}")]
    LoginRefused(String),
    /// A recurrence phrase could not be read.
    ///
    /// Carries the phrase back so a routine editor can leave what was typed in
    /// the field. Never guessed at: a routine that silently fires on the wrong
    /// cadence is discovered weeks later, by which point it has generated the
    /// wrong tasks.
    #[error("not a recurrence: {text} ({cause})")]
    BadRecurrence {
        /// What was typed.
        text: String,
        /// Why it was rejected, in the domain's own words.
        cause: String,
    },
    /// A fixed-width field arrived at the wrong length, or was not hex.
    ///
    /// UniFFI cannot express `[u8; 32]`, so a key crosses as a `Vec<u8>` and a
    /// digest as a hex string. Their width is a contract the seam has to check
    /// somewhere, and here is the last place it can be checked cheaply.
    #[error("{field} must be {expected} bytes")]
    BadFixedBytes {
        /// Which field.
        field: String,
        /// How many bytes it must carry.
        expected: u32,
    },
    /// The relay refused a request, or could not be reached.
    ///
    /// Kept apart from [`BindingError::Core`] because the two need different
    /// things from the user: a core failure is a bug or a broken vault, and
    /// this is a network, a token, or a relay saying no. Raised by
    /// `bootstrap_account`, `register_relay_device` and
    /// [`pairing_relay::RelayPairing`].
    #[error("relay: {0}")]
    Relay(String),
    /// An attachment's metadata is on this device and its bytes are not.
    ///
    /// Its own variant rather than a `Core` message because it is the one
    /// attachment failure that is **not** an error in the usual sense: the
    /// vault is fine and the row is real. A client shows "not downloaded", not
    /// "something went wrong", and it can only tell the two apart if the seam
    /// keeps them apart.
    #[error("attachment {id} has no bytes on this device")]
    AttachmentNotHere {
        /// The attachment.
        id: String,
    },
    /// A client called [`SunriseCore::cancel_attachment_fetch`] while the
    /// download was running.
    ///
    /// Its own variant for the same reason [`BindingError::AttachmentNotHere`]
    /// is: it is not a failure. The user pressed Cancel and the app should go
    /// quietly back to the placeholder — a client that showed this in an error
    /// banner would be reporting the user's own decision back to them.
    #[error("the download of attachment {id} was cancelled")]
    AttachmentDownloadCancelled {
        /// The attachment.
        id: String,
    },
    /// The attachment byte path failed for any other reason.
    ///
    /// Carries the core's own wording, which is written to be shown: an
    /// attachment nothing ever uploaded, a vault with no sync session, and a
    /// relay that could not serve the blob are three different things to tell
    /// the user and each says so in its message.
    #[error("attachment: {0}")]
    Attachment(String),
    /// An `.ics` import or export failed.
    ///
    /// Its own variant rather than a `Core` message: a malformed calendar file
    /// is the user's file being wrong, not the vault being wrong, and a client
    /// says something quite different about the two.
    #[error("calendar: {0}")]
    Calendar(String),
    /// A pairing step failed, was taken out of order, or was refused.
    ///
    /// Includes the SAS rejection, which is an error rather than a value on
    /// purpose: "the codes did not match" is the one pairing outcome a UI must
    /// never let fall through to the next screen.
    #[error("pairing: {0}")]
    Pairing(String),
    /// The vault requires `feature`, this build does not have it, and the
    /// command would have edited what it covers (ADR-0045 §8,
    /// `DOC_FEATURE_MISSING`).
    ///
    /// Its own variant because it is not a failure of the vault or of the
    /// request: nothing was written, reads and sync carry on, and the answer
    /// is "Update Sunrise to edit". A client that disables edit actions from
    /// [`SunriseCore::edit_gate`] should never see it; one that races a sync
    /// can.
    #[error("this vault uses `{feature}`, which this version of Sunrise does not have")]
    FeatureMissing {
        /// The feature id.
        feature: String,
    },
    /// The command needs the vault to require `feature`, and these devices,
    /// which are not revoked, have not said they support it, so nothing was
    /// written (ADR-0045 §7, ADR-0056 §7).
    ///
    /// Its own variant because the answer is "update these devices", which a
    /// client says in its own words and with the devices named. A flat error
    /// carries only its message across the seam, so the message ends with the
    /// ids and [`feature_refusal`] reads them back; a client never parses the
    /// wording itself.
    #[error(
        "enabling `{feature}` needs {} other device(s) to update first{}{}",
        devices.len(),
        FEATURE_REFUSAL_IDS,
        devices.join(FEATURE_REFUSAL_ID_SEPARATOR)
    )]
    FeatureUnsupportedByDevices {
        /// The feature id.
        feature: String,
        /// The devices to update, as lowercase hex ids.
        devices: Vec<String>,
    },
    /// A recovery code that is mistyped, or is not this account's, or that an
    /// identity rotation retired (ADR-0070).
    ///
    /// Raised before anything is written. The client puts the user back in
    /// the 24-word field. The message names a bad word's position and never
    /// the word; for a retired code it says the code was retired and when,
    /// which is what tells the user to look for the newer one.
    #[error("recovery code: {0}")]
    RecoveryCode(String),
    /// The relay will release the recovery blob only to a fresh sign-in, and
    /// the bearer did not carry one.
    ///
    /// Raised before anything is written. The client signs the user in again
    /// with [`SunriseLogin::begin_step_up`] and retries; the code was not the
    /// problem.
    #[error("recovery: {0}")]
    StepUpRequired(String),
    /// The recovery did not produce a vault: no relay, no account identity, no
    /// blob, a relay that would not answer, a directory that already holds a
    /// vault, or a vault that would not open.
    ///
    /// No vault under the restored account exists, so the client must not
    /// keep the root it handed in: opening the directory under it would mint
    /// a fresh, empty account. A directory holds a vault when it has `vault.db`
    /// or a non-empty `vault-id` marker, and a vault that would not open may
    /// have left either behind, which makes the directory refuse the next
    /// recovery until it is cleared.
    #[error("recovery: {0}")]
    RecoveryRefused(String),
    /// The recovery wrote the vault and did not finish: the relay would not
    /// register the device, or the history did not finish replaying.
    ///
    /// The directory now holds a vault under the restored account, opened
    /// under the root the client handed in. The client keeps that root, or the
    /// vault is unreadable. A replay that did not finish resumes the next time
    /// the vault syncs.
    #[error("recovery: {0}")]
    RecoveryIncomplete(String),
    /// The vault's integrity check failed: the file is damaged, and was not
    /// opened or migrated (`STORAGE_INTEGRITY_CHECK_FAILED`).
    ///
    /// This and the five variants after it are what [`SunriseCore::open`]
    /// and [`SunriseCore::open_repairing`] raise for a vault that exists and
    /// will not open, one per [`sunrise_client_core::vault_session::OpenProblem`] but `Other`, which
    /// stays [`BindingError::Core`]. Each reads as the `Core` message it
    /// replaces. The client offers what [`vault_repair::open_remedies`]
    /// lists for it.
    #[error("core: {0}")]
    VaultDamaged(String),
    /// The copy a migration or rebuild runs under could not be written, so
    /// neither ran and the vault is as it was (`STORAGE_BACKUP_FAILED`).
    #[error("core: {0}")]
    VaultBackupFailed(String),
    /// A migration failed and rolled back (`STORAGE_MIGRATION_FAILED`).
    #[error("core: {0}")]
    VaultMigrationFailed(String),
    /// A newer build wrote the vault (`STORAGE_V_TOO_NEW`).
    #[error("core: {0}")]
    VaultTooNew(String),
    /// The vault predates the storage baseline (`STORAGE_V_TOO_OLD`).
    #[error("core: {0}")]
    VaultTooOld(String),
    /// Something else has the vault open (`STORAGE_VAULT_LOCKED`).
    #[error("core: {0}")]
    VaultBusy(String),
    /// `PLAN_STALE`: an input the previewed diff was solved from changed
    /// before the commit. Nothing was written; preview again.
    #[error("PLAN_STALE: the plan's inputs changed since it was previewed")]
    PlanStale,
    /// `PLAN_UNKNOWN`: the diff is not remembered (evicted, cancelled,
    /// already committed, or the vault reopened). Nothing was written;
    /// preview again.
    #[error("PLAN_UNKNOWN: no such plan is remembered")]
    PlanUnknown,
    /// The planner refused the gesture: a diff with a Blocking violation, an
    /// intent naming nothing the vault holds, or a block move that would
    /// leave a recurring block's exception applying to nothing. Carries the
    /// core's words.
    #[error("plan: {0}")]
    PlanRefused(String),
}

impl From<sunrise_integrations::IntegrationError> for BindingError {
    fn from(e: sunrise_integrations::IntegrationError) -> Self {
        Self::Calendar(e.to_string())
    }
}

impl From<sunrise_pairing::PairingError> for BindingError {
    fn from(e: sunrise_pairing::PairingError) -> Self {
        Self::Pairing(e.to_string())
    }
}

impl From<CoreError> for BindingError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Engine(sunrise_core::EngineError::FeatureMissing { feature }) => {
                Self::FeatureMissing { feature }
            }
            CoreError::Engine(sunrise_core::EngineError::FeatureUnsupportedByDevices {
                feature,
                devices,
            }) => Self::FeatureUnsupportedByDevices {
                feature,
                devices: devices.iter().map(hex16).collect(),
            },
            other => Self::Core(other.to_string()),
        }
    }
}

/// Where [`BindingError::FeatureUnsupportedByDevices`]'s message turns from
/// the sentence to the device ids.
const FEATURE_REFUSAL_IDS: &str = ": ";
/// Between two device ids in that message.
const FEATURE_REFUSAL_ID_SEPARATOR: &str = ", ";

/// What a [`BindingError::FeatureUnsupportedByDevices`] refused on.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FeatureRefusal {
    /// The feature the command needed the vault to require.
    pub feature: String,
    /// The devices to update before it can, as lowercase hex ids.
    pub devices: Vec<String>,
}

/// The feature and devices a [`BindingError::FeatureUnsupportedByDevices`]
/// names, read back from the message it crossed the seam with. `None` for
/// any other message.
///
/// The error is flat, so its fields do not cross and the message is all a
/// client receives. This reads the message the seam itself wrote, so the
/// wording and the reading of it change together, in one file, under one
/// test; a client passes the variant's message here instead of parsing it.
#[uniffi::export]
#[must_use]
pub fn feature_refusal(message: String) -> Option<FeatureRefusal> {
    let rest = message.strip_prefix("enabling `")?;
    let (feature, rest) = rest.split_once('`')?;
    let (_, ids) = rest.split_once(" to update first")?;
    let ids = ids.strip_prefix(FEATURE_REFUSAL_IDS).unwrap_or(ids);
    let devices: Vec<String> = ids
        .split(FEATURE_REFUSAL_ID_SEPARATOR)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    if feature.is_empty()
        || devices
            .iter()
            .any(|id| !id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(FeatureRefusal {
        feature: feature.to_owned(),
        devices,
    })
}
