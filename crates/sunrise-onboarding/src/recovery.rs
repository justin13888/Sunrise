//! Recovery flow: BIP-39 → seed → unseal recovery blob → restore identity.
//!
//! Per `docs/03-crypto/recovery.md`. The flow:
//!
//! 1. User logs in via OIDC on a fresh device.
//! 2. Client fetches the encrypted recovery blob from the server.
//! 3. User enters the 24-word BIP-39 recovery phrase.
//! 4. Phrase → 32-byte seed (BIP-39 entropy).
//! 5. Argon2id over seed + salt → recovery key.
//! 6. AEAD-open the recovery blob → identity private keys.
//!
//! Steps 3 and 4 are [`recover_identity_from_code`], which is the whole flow
//! from what a user types. This crate used to say the BIP-39 step was "left to
//! a higher-level UI/CLI surface that integrates with a chosen wordlist
//! library" and no such surface existed anywhere in the workspace, so a typed
//! recovery code had nothing to be handed to; `sunrise_crypto::bip39` is that
//! codec now. [`recover_identity`] remains for a caller that already holds the
//! 32-byte seed.
//!
//! # The account-level flow
//!
//! [`recover_account`] is everything after the user has signed in: §Recovery
//! flow steps 3 to 8, from the 24 words to a vault that has read its history.
//! It used to live in `sunrise-cli`, where no other client could call it; it
//! lives here so that `sunrise recover` and the UniFFI seam's
//! `SunriseCore::recover_account` run one implementation.
//!
//! ```text
//!  step 3   the account's ID_S_pub   -> identity_id (the AAD)
//!           the sealed recovery blob    (behind an OIDC step-up)
//!  steps 4-5 the 24 words open the blob -> ID_S_priv, ID_D_priv
//!  step 6   open a vault under a fresh root, re-keyed under the restored
//!           identity, and register this device with the relay
//!  step 8   sync until the replay has caught up
//! ```
//!
//! The order is the load-bearing part. #180 states the trap: *a recovery that
//! mints the root before it can decrypt anything produces a vault that opens
//! and is empty, which looks exactly like success.* So the vault is created
//! **from** the restored identity, and success is reported only once the sync
//! driver has caught up.
//!
//! Everything that talks to the relay goes through [`RecoveryRelay`], which a
//! caller supplies. That is what lets this crate sit below
//! `sunrise-relay-client` (which implements it) and what lets the flow be
//! tested against a relay that refuses, serves the wrong blob, or never
//! answers, with no network at all.

use std::future::Future;
use std::sync::Arc;

use sunrise_core::{Core, CoreConfig, CoreError, DomainEvent, Unlock};
use sunrise_crypto::bip39::{decode_recovery_code, Bip39Error};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_crypto::{unseal_recovery_blob, RecoveryError};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::account::{decode_public_key, decode_recovery_blob, PublicKeyError};

/// Recovery flow errors.
#[derive(Debug, Error)]
pub enum RecoveryFlowError {
    /// The typed recovery code is not a well-formed 24-word BIP-39 mnemonic.
    ///
    /// Kept apart from [`RecoveryFlowError::Crypto`] because it is the failure
    /// a user can *fix by retyping*, and it is decided before Argon2id runs —
    /// which on the slowest supported device is several seconds of work that a
    /// mistyped word should not have to wait for.
    #[error(transparent)]
    Code(#[from] Bip39Error),
    /// Wrapped crypto-layer error from the recovery blob unseal.
    #[error(transparent)]
    Crypto(#[from] RecoveryError),
}

/// Restore identity keys from a recovery blob using the BIP-39 derived seed.
///
/// `expected_identity_id` MUST come from the OIDC account record; binding
/// it as AAD prevents replay across accounts.
pub fn recover_identity(
    blob: &[u8],
    seed: &[u8; 32],
    expected_identity_id: &[u8; 16],
) -> Result<sunrise_crypto::recovery::RecoveryPayload, RecoveryFlowError> {
    let payload = unseal_recovery_blob(blob, seed, expected_identity_id)?;
    Ok(payload)
}

/// Restore identity keys from a recovery blob and the 24 words a user typed.
///
/// The whole of steps 2 to 5 of `docs/03-crypto/recovery.md` §Recovery flow:
/// the code's checksum is verified first, so a typo surfaces immediately
/// rather than after an Argon2id run, and the blob is then opened under the
/// seed those words carry.
///
/// `expected_identity_id` MUST come from the OIDC account record, for the same
/// reason [`recover_identity`] says so: it is the AAD, and it is what stops a
/// blob from one account being replayed against another.
///
/// # Errors
/// [`RecoveryFlowError::Code`] for a mistyped or wrong-length code, and
/// [`RecoveryFlowError::Crypto`] for a code that is well formed and simply not
/// this account's — the two are distinguishable because the advice differs.
pub fn recover_identity_from_code(
    blob: &[u8],
    recovery_code: &str,
    expected_identity_id: &[u8; 16],
) -> Result<sunrise_crypto::recovery::RecoveryPayload, RecoveryFlowError> {
    let seed = decode_recovery_code(recovery_code)?;
    recover_identity(blob, &seed, expected_identity_id)
}

/// Whether `word` is on the BIP-39 English wordlist.
///
/// What a 24-word field checks as each word is typed, so a typo is marked at
/// the word that holds it rather than after the whole code has been entered.
/// Case and surrounding whitespace are ignored, as the decoder ignores them.
/// A list of valid words can still fail the checksum, which only
/// [`check_recovery_code`] can say.
#[must_use]
pub fn is_recovery_word(word: &str) -> bool {
    sunrise_crypto::bip39::is_english_word(word)
}

/// Check a whole typed code without spending it: the word count, every word,
/// and the checksum.
///
/// This is the same decode [`recover_account`] runs first, exposed so a client
/// can refuse a code before it starts a sign-in the code would then waste.
///
/// # Errors
/// [`Bip39Error`] naming what is wrong.
pub fn check_recovery_code(recovery_code: &str) -> Result<(), Bip39Error> {
    decode_recovery_code(recovery_code).map(|_| ())
}

/// Why the relay did not give a recovery what it asked for.
///
/// Split where the caller does something different. A missing step-up is
/// fixed by signing in again, a missing blob by nothing on this device, and
/// everything else is a network or a relay to retry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RelayRefusal {
    /// `403`: the bearer carries no fresh sign-in, and the blob route requires
    /// one (`docs/06-server/auth.md` §The step-up in front of step 3).
    #[error("{0}")]
    StepUpRequired(String),
    /// `404`: the account has no recovery blob stored.
    #[error("{0}")]
    NoBlob(String),
    /// Any other refusal or transport failure, in the relay client's words.
    #[error("{0}")]
    Other(String),
}

/// What a recovering device tells `POST /api/v1/devices` about itself.
///
/// Read off the vault [`rejoin_account`] has just opened: the keys are the
/// ones `Keychain::create` minted, and `device_cert` is the cert the
/// **restored** `ID_S_priv` signed over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveringDevice {
    /// This device's Ed25519 public key (`D_S_pub`).
    pub device_pub_s: [u8; 32],
    /// The `DeviceCert` this device issued itself under the restored identity
    /// (canonical CBOR). Sent because a recovering device joins an account
    /// that may still have siblings, and this is the copy they can be shown.
    pub device_cert: Vec<u8>,
    /// The vault's own 16-byte device id: the name a sibling revokes it by.
    pub vault_device_id: [u8; 16],
    /// What the device list calls this device.
    pub nickname: String,
    /// The platform tag, as `POST /api/v1/devices` names them.
    pub platform: String,
    /// The build that registered.
    pub app_version: Option<String>,
}

/// The relay, as a recovery needs it.
///
/// Injected rather than built here so the flow is testable without a network,
/// and so this crate does not depend on the generated client that depends on
/// it. `sunrise-relay-client` provides the implementation both shipping
/// callers use; every method carries the bearer the implementation was built
/// with, which for [`RecoveryRelay::recovery_blob`] must carry a completed
/// OIDC step-up.
pub trait RecoveryRelay: Send + Sync {
    /// The account record's `identity_signing_pub`, or `None` where the
    /// account never published one.
    fn account_identity_key(
        &self,
    ) -> impl Future<Output = Result<Option<String>, RelayRefusal>> + Send;

    /// The sealed recovery blob, base64url as the relay serves it.
    fn recovery_blob(&self) -> impl Future<Output = Result<String, RelayRefusal>> + Send;

    /// Register this device on the account and return the id the relay
    /// minted for it.
    fn register_device(
        &self,
        device: RecoveringDevice,
    ) -> impl Future<Output = Result<String, RelayRefusal>> + Send;
}

/// Start the sync driver on a restored vault, bound to the relay device id the
/// registration returned.
///
/// A callback rather than a [`RecoveryRelay`] method because the transport is
/// the caller's: the CLI and the UniFFI seam each build their own SSE factory,
/// and this crate stays out of transport selection.
pub type SyncStarter<'a> = &'a (dyn Fn(&Arc<Core>, &str) -> Result<(), String> + Send + Sync);

/// How far a recovery has got. Reported in order, each at most once except
/// [`RecoveryProgress::Replaying`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryProgress {
    /// The relay served the sealed blob.
    BlobFetched,
    /// The 24 words opened it; the account identity is in hand.
    IdentityOpened,
    /// The vault exists and the relay knows this device by this id.
    DeviceRegistered {
        /// The id the relay minted. The caller records it: the relay never
        /// sends it again, and every later sync request names it.
        relay_device_id: String,
    },
    /// The replay of §Recovery flow step 8 is running. `applied` counts the
    /// changes it has applied to this vault so far.
    ///
    /// There is no total beside it: the relay does not say how much history
    /// an account holds before it has sent it.
    Replaying {
        /// Changes applied so far.
        applied: u64,
    },
    /// The sync driver is live with nothing pending: the history is read.
    CaughtUp,
}

/// Why a recovery did not complete.
///
/// One arm per thing a user can do about it. A recovery is run by somebody who
/// has already lost every device, so "it failed" is not an acceptable message.
#[derive(Debug, Error)]
pub enum RestoreError {
    /// The account record carries no identity key.
    #[error(
        "this account has published no identity key, so there is nothing to recover into. \
         An account is only recoverable once a device has registered it with the relay."
    )]
    NoIdentityKey,
    /// The account record's identity key is not a key.
    #[error("the account's identity key is unreadable: {0}")]
    IdentityKey(#[from] PublicKeyError),
    /// The relay refused the blob to a session without a fresh sign-in.
    #[error("the relay will release the recovery blob only after a fresh sign-in: {0}")]
    StepUpRequired(String),
    /// The account has no recovery blob.
    #[error(
        "this account has no recovery blob on the relay, so no recovery code can open it: {0}"
    )]
    NoBlob(String),
    /// The relay could not be reached or refused for another reason.
    #[error("{0}")]
    Relay(String),
    /// The relay served something that is not a sealed blob.
    #[error("the relay served a malformed recovery blob: {0}")]
    MalformedBlob(String),
    /// The words are mistyped, or are not this account's code.
    #[error(transparent)]
    Code(#[from] RecoveryFlowError),
    /// The vault could not be opened from the restored identity.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The vault exists and the relay would not register this device.
    #[error("the vault was restored but the relay would not register this device: {0}")]
    Register(String),
    /// The vault exists, the device is registered, and the replay did not
    /// finish within the budget.
    #[error("the vault was restored but never finished reading its history ({detail})")]
    NeverCaughtUp {
        /// What the sync driver last reported.
        detail: String,
        /// The id the relay minted, which the caller still has to record.
        relay_device_id: String,
    },
}

impl RestoreError {
    /// Whether the failure happened after the vault was written to disk.
    ///
    /// A client decides from this what to keep. Before the vault exists,
    /// nothing was written and the directory is still empty, so a retry starts
    /// clean. After, the directory holds a vault under the restored identity
    /// and the root it was opened with: keep the root, or the vault is
    /// unreadable.
    #[must_use]
    pub fn vault_written(&self) -> bool {
        matches!(
            self,
            Self::Core(_) | Self::Register(_) | Self::NeverCaughtUp { .. }
        )
    }
}

/// What a completed recovery produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    /// The account's 16-byte identity id, lower-case hex.
    pub identity_id: String,
    /// The relay's id for this device.
    pub relay_device_id: String,
}

/// How this device describes itself when it registers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceLabel {
    /// What the device list calls it.
    pub nickname: String,
    /// The platform tag.
    pub platform: String,
    /// The build.
    pub app_version: Option<String>,
}

/// Derive the vault's `identity_id` from the `ID_S_pub` the account record
/// carries.
///
/// This is the value the recovery blob's AAD binds, so it has to be known
/// before the blob can be opened, and on a fresh device the relay is the only
/// place it can come from. Nothing is trusted on the strength of that answer:
/// a relay that served another account's key produces an `identity_id` the
/// blob was not sealed under, and the AEAD refuses it, which is the refusal a
/// mistyped code gets.
///
/// # Errors
/// [`RestoreError::NoIdentityKey`] when the account never published one, and
/// [`RestoreError::IdentityKey`] when what it published is not a key.
pub fn identity_id_from_account(
    identity_signing_pub: Option<&str>,
) -> Result<[u8; 16], RestoreError> {
    let encoded = identity_signing_pub
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(RestoreError::NoIdentityKey)?;
    let key = decode_public_key(encoded)?;
    Ok(sunrise_crypto::identity_id_from_pub(&key))
}

/// §Recovery flow steps 3 to 5: fetch the account record and the sealed blob,
/// and open the blob with `code`.
///
/// The code is decoded **before** the relay is asked anything, so a typo
/// never costs a round trip, and a step-up sign-in is not spent on words that
/// could never have worked.
///
/// # Errors
/// [`RestoreError::Code`] for a mistyped code or one that is not this
/// account's, and the relay arms for what the relay said.
pub async fn restore_identity<R: RecoveryRelay>(
    relay: &R,
    code: &str,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<RecoveryPayload, RestoreError> {
    let seed = decode_recovery_code(code).map_err(RecoveryFlowError::from)?;

    let key = relay.account_identity_key().await.map_err(relay_error)?;
    let identity_id = identity_id_from_account(key.as_deref())?;

    let served = relay.recovery_blob().await.map_err(relay_error)?;
    let blob =
        decode_recovery_blob(&served).map_err(|e| RestoreError::MalformedBlob(e.to_string()))?;
    progress(RecoveryProgress::BlobFetched);

    let identity = recover_identity(&blob, &seed, &identity_id)?;
    progress(RecoveryProgress::IdentityOpened);
    Ok(identity)
}

/// §Recovery flow steps 6 and 8: open a vault from the restored identity,
/// register this device, and wait for the replay.
///
/// `root` is minted by the caller, because where a root is kept is the
/// platform's business: a keystore file for the CLI, the Keychain for the
/// Apple clients. It is not in the blob and could not be (`recovery.md`
/// §Device backups do not carry the vault root).
///
/// `Core::open` does the cryptographic half of step 6 without being asked:
/// `Keychain::create` mints fresh `D_S`/`D_D` and issues a `DeviceCert` signed
/// by the restored `ID_S_priv`, and the engine publishes it as an op. The
/// replay needs no walker of its own: every identity-addressed `key_envelope`
/// arrives as an ordinary op and opens under the restored `ID_D_priv`. What
/// this adds is the **waiting**: reporting success before the log has been
/// read would hand the user an empty vault and call it a recovery.
///
/// The core is shut down before this returns, on every path, so the caller
/// can open the vault again in the same process.
///
/// # Errors
/// [`RestoreError::Core`], [`RestoreError::Register`], and
/// [`RestoreError::NeverCaughtUp`]; every one of them has written the vault.
#[allow(clippy::too_many_arguments)]
pub async fn rejoin_account<R: RecoveryRelay>(
    relay: &R,
    cfg: CoreConfig,
    root: VaultRootKey,
    identity: RecoveryPayload,
    device: DeviceLabel,
    start_sync: SyncStarter<'_>,
    catch_up_ms: u64,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<Recovered, RestoreError> {
    let identity_id = hex_16(&identity.identity_id);
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::RecoveryCode {
                root,
                identity: Box::new(identity),
            },
        )
        .await?,
    );

    // Step 6, second half: publish this device to the account. A registration
    // and not an account creation: the account exists, its identity keys are
    // the ones this vault just restored, and the blob column is write-once and
    // already holds the blob this recovery spent.
    let registered = relay
        .register_device(RecoveringDevice {
            device_pub_s: core.device_signing_pub(),
            device_cert: core.device_cert(),
            vault_device_id: core.device_id(),
            nickname: device.nickname,
            platform: device.platform,
            app_version: device.app_version,
        })
        .await;
    let relay_device_id = match registered {
        Ok(id) => id,
        Err(e) => {
            core.shutdown().await;
            return Err(RestoreError::Register(e.to_string()));
        }
    };
    progress(RecoveryProgress::DeviceRegistered {
        relay_device_id: relay_device_id.clone(),
    });

    // Subscribed before the driver starts, so not one applied change is
    // missed from the count.
    let changes = core.changes();
    let caught_up = match start_sync(&core, &relay_device_id) {
        Ok(()) => wait_until_live(&core, changes, catch_up_ms, progress).await,
        Err(e) => Err(format!("sync did not start: {e}")),
    };
    core.shutdown().await;
    caught_up.map_err(|detail| RestoreError::NeverCaughtUp {
        detail,
        relay_device_id: relay_device_id.clone(),
    })?;
    progress(RecoveryProgress::CaughtUp);

    Ok(Recovered {
        identity_id,
        relay_device_id,
    })
}

/// The whole of §Recovery flow steps 3 to 8, from the typed code to a vault
/// that has read its history: [`restore_identity`], then [`rejoin_account`].
///
/// What the UniFFI seam's `recover_account` runs. The CLI runs the two halves
/// itself, because it mints its root only once the code has opened the blob.
///
/// # Errors
/// Every [`RestoreError`]; [`RestoreError::vault_written`] says whether the
/// directory was touched.
#[allow(clippy::too_many_arguments)]
pub async fn recover_account<R: RecoveryRelay>(
    relay: &R,
    code: &str,
    cfg: CoreConfig,
    root: VaultRootKey,
    device: DeviceLabel,
    start_sync: SyncStarter<'_>,
    catch_up_ms: u64,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<Recovered, RestoreError> {
    let identity = restore_identity(relay, code, progress).await?;
    rejoin_account(
        relay,
        cfg,
        root,
        identity,
        device,
        start_sync,
        catch_up_ms,
        progress,
    )
    .await
}

/// Map a relay refusal onto the error a recovery reports.
fn relay_error(e: RelayRefusal) -> RestoreError {
    match e {
        RelayRefusal::StepUpRequired(m) => RestoreError::StepUpRequired(m),
        RelayRefusal::NoBlob(m) => RestoreError::NoBlob(m),
        RelayRefusal::Other(m) => RestoreError::Relay(m),
    }
}

/// Wait for the sync driver to report `Live` with nothing pending, reporting
/// how many changes the replay has applied as it goes.
///
/// Timed against the core's injected clock rather than `Instant`, which the
/// workspace lint bans so that time is never read from two sources.
async fn wait_until_live(
    core: &Core,
    mut changes: broadcast::Receiver<DomainEvent>,
    budget_ms: u64,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<(), String> {
    /// How often to re-read the status.
    const POLL: std::time::Duration = std::time::Duration::from_millis(200);
    let deadline = core.now_ms().saturating_add(budget_ms);
    let mut applied = 0u64;
    let mut reported = 0u64;
    let last = loop {
        loop {
            match changes.try_recv() {
                Ok(_) => applied += 1,
                Err(broadcast::error::TryRecvError::Lagged(n)) => applied += n,
                Err(_) => break,
            }
        }
        if applied != reported {
            reported = applied;
            progress(RecoveryProgress::Replaying { applied });
        }
        let sunrise_core::QueryResult::SyncStatus(s) = core
            .query(sunrise_core::Query::SyncStatus)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Err("unexpected query result".into());
        };
        if s.state == sunrise_sync::SyncState::Live && s.outbox_pending == 0 {
            return Ok(());
        }
        if core.now_ms() >= deadline {
            break format!("{:?}, {} pending", s.state, s.outbox_pending);
        }
        tokio::time::sleep(POLL).await;
    };
    Err(format!("still {last} after {}s", budget_ms / 1000))
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;
    use sunrise_crypto::seal_recovery_blob;

    #[test]
    fn round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let seed = [9u8; 32];
        let payload = RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [5u8; 16],
            created_at_ms: 1,
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
        let back = recover_identity(&blob, &seed, &payload.identity_id).unwrap();
        assert_eq!(back, payload);
    }

    /// The flow a user actually runs: twenty-four words in, identity out.
    #[test]
    fn a_typed_recovery_code_restores_the_identity() {
        let mut rng = ChaCha20Rng::seed_from_u64(11);
        let seed = [0x2bu8; 32];
        let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
        let payload = RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [5u8; 16],
            created_at_ms: 1,
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();

        let back = recover_identity_from_code(&blob, code.reveal(), &payload.identity_id).unwrap();
        assert_eq!(back, payload);
    }

    /// A mistyped word is refused before Argon2id runs, and is reported as a
    /// code failure rather than as a decryption failure — the user's next
    /// action is "check what you typed", not "find another code".
    #[test]
    fn a_mistyped_code_is_refused_as_a_code() {
        let mut rng = ChaCha20Rng::seed_from_u64(12);
        let seed = [0x2cu8; 32];
        let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
        let payload = RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [6u8; 16],
            created_at_ms: 1,
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();

        let mut words: Vec<&str> = code.reveal().split(' ').collect();
        words.pop();
        assert!(matches!(
            recover_identity_from_code(&blob, &words.join(" "), &payload.identity_id),
            Err(RecoveryFlowError::Code(_))
        ));

        // And a well-formed code for another account fails the AEAD, which is
        // the other half: the two errors must not collapse.
        let other = sunrise_crypto::bip39::encode_recovery_code(&[0x2du8; 32]);
        assert!(matches!(
            recover_identity_from_code(&blob, other.reveal(), &payload.identity_id),
            Err(RecoveryFlowError::Crypto(_))
        ));
    }
}
