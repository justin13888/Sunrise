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
//! It used to live in `sunrise-cli`, where no other client could call it, and
//! then in `sunrise-onboarding`, below the relay client; it lives here, in the
//! client layer, so that `sunrise recover` and the `UniFFI` seam's
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
//! Everything that talks to the relay goes through [`RecoveryRelay`]. This
//! module implements it over [`sunrise_relay_client::RelayRecovery`], which is
//! what both shipping callers hand in; the trait is what lets the flow be
//! tested against a relay that refuses, serves the wrong blob, or never
//! answers, with no network at all. The dependency runs one way: this crate
//! calls the REST client, and the REST client names no engine.

use std::future::Future;
use std::sync::Arc;

use sunrise_core::{Core, CoreConfig, CoreError, SyncStatus, Unlock};
use sunrise_crypto::bip39::{decode_recovery_code, Bip39Error};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_crypto::{unseal_recovery_blob, RecoveryError};
use thiserror::Error;

pub use sunrise_relay_client::RelayRefusal;
use sunrise_relay_client::{
    decode_public_key, decode_recovery_blob, DeviceIdentity, PublicKeyError, RelayRecovery,
};

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
/// Injected rather than called directly so the flow is testable without a
/// network. [`RelayRecovery`] is the implementation both shipping callers use;
/// every method carries the bearer the implementation was built with, which
/// for [`RecoveryRelay::recovery_blob`] must carry a completed OIDC step-up.
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

impl RecoveryRelay for RelayRecovery {
    async fn account_identity_key(&self) -> Result<Option<String>, RelayRefusal> {
        RelayRecovery::account_identity_key(self).await
    }

    async fn recovery_blob(&self) -> Result<String, RelayRefusal> {
        RelayRecovery::recovery_blob(self).await
    }

    async fn register_device(&self, device: RecoveringDevice) -> Result<String, RelayRefusal> {
        RelayRecovery::register_device(self, recovering_identity(device)).await
    }
}

/// What a recovering device registers as: its keys, the cert the restored
/// identity signed over them, and the vault id a sibling revokes it by.
fn recovering_identity(device: RecoveringDevice) -> DeviceIdentity {
    DeviceIdentity {
        device_pub_s: device.device_pub_s,
        device_pub_d: None,
        // Opaque to the server, and carried in the same alphabet as the blob.
        device_cert: Some(sunrise_relay_client::encode_recovery_blob(
            &device.device_cert,
        )),
        vault_device_id: Some(sunrise_id::crockford::encode_bytes(&device.vault_device_id)),
        nickname: device.nickname,
        platform: device.platform,
        app_version: device.app_version,
    }
}

/// Start the sync driver on a restored vault, bound to the relay device id the
/// registration returned.
///
/// A callback rather than a [`RecoveryRelay`] method because the transport is
/// the caller's: the CLI and the `UniFFI` seam each build their own SSE factory,
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
    /// The replay of §Recovery flow step 8 is running.
    ///
    /// Both numbers count ops (#459): `applied` is how many the replay has
    /// delivered to this vault, and `total` is `applied` plus what the relay
    /// has stated it will still send. A relay that predates that statement
    /// gives no total, and a client shows `applied` alone.
    ///
    /// `total` can grow while the replay runs. The ops it delivers name
    /// streams this new vault did not know it had, and a stream's backlog is
    /// stated only once the vault has subscribed to it.
    Replaying {
        /// Ops delivered so far.
        applied: u64,
        /// Ops delivered plus the ops the relay still has to send, where the
        /// relay states its backlog.
        total: Option<u64>,
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
    ///
    /// Not [`RestoreError::vault_written`], even though the open may have
    /// left files behind: there is no vault under the restored identity, and
    /// a client that kept the root and opened the directory anyway would mint
    /// a fresh, empty account in it. The leftovers make the directory refuse
    /// the next recovery until it is cleared, which is the safe failure.
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
    /// Whether the failure happened after a vault under the restored identity
    /// was written to disk.
    ///
    /// A client decides from this what to keep. When `true`, the directory
    /// holds that vault, opened under the root the client handed in: keep the
    /// root, or the vault is unreadable. When `false`, there is no such vault,
    /// and the root must not be kept, because opening the directory under it
    /// would create a fresh, empty account, which looks exactly like success.
    #[must_use]
    pub fn vault_written(&self) -> bool {
        matches!(self, Self::Register(_) | Self::NeverCaughtUp { .. })
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
/// [`RestoreError::Core`] when the vault would not open, and
/// [`RestoreError::Register`] and [`RestoreError::NeverCaughtUp`], both of
/// which have written it.
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
    let identity_id = crate::ids::hex_id(&identity.identity_id);
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

    let caught_up = match start_sync(&core, &relay_device_id) {
        Ok(()) => wait_until_live(&core, catch_up_ms, progress).await,
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
/// What the `UniFFI` seam's `recover_account` runs. The CLI runs the two halves
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

/// How far a replay has got, from one sync snapshot: the ops delivered, and
/// the total where the relay has stated what is left.
#[must_use]
pub fn replay_progress(status: &SyncStatus) -> RecoveryProgress {
    let applied = status.replay_applied;
    RecoveryProgress::Replaying {
        applied,
        total: status
            .replay_remaining
            .map(|left| applied.saturating_add(left)),
    }
}

/// Wait for the sync driver to report `Live` with nothing pending, reporting
/// how far the replay has got as it goes.
///
/// [`crate::sync::wait_until_caught_up`], timed against the core's injected
/// clock, with each snapshot read through [`replay_progress`] and reported
/// only when it moved.
async fn wait_until_live(
    core: &Core,
    budget_ms: u64,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<(), String> {
    let mut reported: Option<RecoveryProgress> = None;
    let waited = crate::sync::wait_until_caught_up(core, budget_ms, &mut |status| {
        let now = replay_progress(status);
        let started = matches!(now, RecoveryProgress::Replaying { applied, total } if applied > 0 || total.is_some());
        if started && reported.as_ref() != Some(&now) {
            progress(now.clone());
            reported = Some(now);
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    waited.map_err(|last| {
        format!(
            "still {:?}, {} pending after {}s",
            last.state,
            last.outbox_pending,
            budget_ms / 1000
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;
    use sunrise_crypto::seal_recovery_blob;

    /// The registration a recovery sends names the vault id in Crockford
    /// base-32 and carries the cert, so a sibling can revoke and inspect it.
    #[test]
    fn a_recovering_device_registers_its_vault_id_and_cert() {
        let id = recovering_identity(RecoveringDevice {
            device_pub_s: [7u8; 32],
            device_cert: vec![1, 2, 3],
            vault_device_id: [0u8; 16],
            nickname: "n".into(),
            platform: "linux".into(),
            app_version: Some("1".into()),
        });
        assert_eq!(id.device_pub_s, [7u8; 32]);
        assert_eq!(id.device_cert.as_deref(), Some("AQID"));
        assert_eq!(
            id.vault_device_id.as_deref(),
            Some("00000000000000000000000000")
        );
        assert!(id.device_pub_d.is_none());
        assert_eq!((id.nickname.as_str(), id.platform.as_str()), ("n", "linux"));
    }

    fn status(applied: u64, remaining: Option<u64>) -> SyncStatus {
        SyncStatus {
            state: sunrise_sync::SyncState::CatchingUp,
            outbox_pending: 0,
            peer_devices: 0,
            last_sync_ms: None,
            replay_applied: applied,
            replay_remaining: remaining,
        }
    }

    /// #459: n of N where the relay states what is left, n alone where it
    /// does not.
    #[test]
    fn a_replay_reports_a_total_only_where_the_relay_states_one() {
        assert_eq!(
            replay_progress(&status(3, Some(7))),
            RecoveryProgress::Replaying {
                applied: 3,
                total: Some(10)
            }
        );
        assert_eq!(
            replay_progress(&status(10, Some(0))),
            RecoveryProgress::Replaying {
                applied: 10,
                total: Some(10)
            }
        );
        assert_eq!(
            replay_progress(&status(3, None)),
            RecoveryProgress::Replaying {
                applied: 3,
                total: None
            }
        );
    }

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
