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
use sunrise_crypto::{
    retired_check_matches, unseal_recovery_blob, CarryWalk, IdentityChain, RecoveryError,
};
use sunrise_sync::SyncState;
use thiserror::Error;
use zeroize::Zeroize as _;

use sunrise_relay_client::{
    decode_public_key, decode_recovery_blob, DeviceIdentity, PublicKeyError, RelayRecovery,
};
pub use sunrise_relay_client::{RecoveryMaterial, RelayRefusal, RetiredCode};

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

    /// The sealed recovery blob, base64url as the relay serves it, with the
    /// account's identity chain record and its retired codes (ADR-0070).
    fn recovery_blob(&self) -> impl Future<Output = Result<RecoveryMaterial, RelayRefusal>> + Send;

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

    async fn recovery_blob(&self) -> Result<RecoveryMaterial, RelayRefusal> {
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
    /// The words were this account's code, and an identity rotation retired
    /// them (ADR-0070): either the account was given a new code, or a
    /// rotation it went through could not carry the old one forward.
    ///
    /// Distinct from [`RestoreError::Code`] because the advice is: these
    /// words were right once, there is a newer code, and nothing about them
    /// can be fixed by retyping. Nothing was restored.
    #[error(
        "this recovery code was retired{}: the account's identity was rotated without carrying \
         it forward, so these words no longer open the account. Use the newer code that rotation \
         issued, or have a device that still holds the account issue one \
         (`sunrise identity new-code`)",
        retired_on(*.retired_at_ms)
    )]
    CodeRetired {
        /// When the relay replaced the blob this code opened, ms since the
        /// epoch; `None` when the code still opens a blob and a rotation
        /// after it carried nothing forward.
        retired_at_ms: Option<u64>,
    },
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

/// " on 2026-10-24", or nothing, for [`RestoreError::CodeRetired`].
fn retired_on(at_ms: Option<u64>) -> String {
    at_ms
        .and_then(|ms| jiff::Timestamp::from_millisecond(i64::try_from(ms).ok()?).ok())
        .map(|t| format!(" on {}", t.strftime("%Y-%m-%d")))
        .unwrap_or_default()
}

/// What the 24 words opened, and what the relay keeps that takes it to the
/// identity in force.
pub struct RestoredIdentity {
    /// The identity the blob carried, `ID_D_priv` included.
    pub payload: RecoveryPayload,
    /// The account's identity chain record, when the relay holds one that
    /// verifies from this account's genesis (ADR-0070). [`rejoin_account`]
    /// seeds the vault with it before anything else reads the relay.
    pub identity_chain: Option<Vec<u8>>,
}

impl std::fmt::Debug for RestoredIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestoredIdentity")
            .field("payload", &self.payload)
            .field(
                "identity_chain",
                &self.identity_chain.as_ref().map(Vec::len),
            )
            .finish()
    }
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
) -> Result<RestoredIdentity, RestoreError> {
    let mut seed = decode_recovery_code(code).map_err(RecoveryFlowError::from)?;

    let key = relay.account_identity_key().await.map_err(relay_error)?;
    let identity_id = identity_id_from_account(key.as_deref())?;

    let served = relay.recovery_blob().await.map_err(relay_error)?;
    let blob = decode_recovery_blob(&served.blob)
        .map_err(|e| RestoreError::MalformedBlob(e.to_string()))?;
    progress(RecoveryProgress::BlobFetched);

    let opened = recover_identity(&blob, &seed, &identity_id);
    // A code that does not open the blob in place may be one a rotation
    // replaced. The relay kept a few bytes of each blob it replaced, enough to
    // say so and too few to open anything.
    let retired = match &opened {
        Err(RecoveryFlowError::Crypto(RecoveryError::AuthFailed)) => {
            retired_at(&served.retired, &seed)
        }
        _ => None,
    };
    seed.zeroize();
    if let Some(at) = retired {
        return Err(RestoreError::CodeRetired {
            retired_at_ms: Some(at),
        });
    }
    let mut payload = opened?;
    let identity_chain = served
        .identity_chain
        .as_deref()
        .and_then(|c| decode_recovery_blob(c).ok())
        .filter(|c| IdentityChain::decode(c).is_ok_and(|chain| chain.verify(&identity_id).is_ok()));
    // The words opened the blob. Whether they still reach the account is the
    // chain's to say: a rotation after the blob's identity that carried no
    // share retired them, and a vault restored from them would be a vault of
    // an identity nothing the account writes is sealed to.
    if let Some(chain) = identity_chain
        .as_deref()
        .and_then(|c| IdentityChain::decode(c).ok())
    {
        if let CarryWalk::Retired { .. } =
            chain.carry_walk(&payload.identity_id, &payload.id_d_priv)
        {
            payload.zeroize();
            return Err(RestoreError::CodeRetired {
                retired_at_ms: None,
            });
        }
    }
    progress(RecoveryProgress::IdentityOpened);
    Ok(RestoredIdentity {
        payload,
        identity_chain,
    })
}

/// When the blob `seed` opened was retired, if one of `retired` is its.
fn retired_at(retired: &[RetiredCode], seed: &[u8; 32]) -> Option<u64> {
    retired.iter().find_map(|r| {
        let check = decode_recovery_blob(&r.check).ok()?;
        retired_check_matches(&check, seed)
            .ok()?
            .then_some(r.retired_at_ms)
    })
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
/// replay needs no walker of its own. Where the relay still holds them, every
/// identity-addressed `key_envelope` arrives as an ordinary op and opens under
/// the restored `ID_D_priv`. Where it has trimmed them, the relay reports a
/// cursor gap, the stream's snapshot record needs a key this vault lacks, and
/// the sync driver fetches the account's key bundle, opens it with the same
/// `ID_D_priv` and applies the record (ADR-0069, #580). What this adds is the
/// **waiting**: reporting success before the log has been read would hand the
/// user an empty vault and call it a recovery.
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
    identity: RestoredIdentity,
    device: DeviceLabel,
    start_sync: SyncStarter<'_>,
    catch_up_ms: u64,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<Recovered, RestoreError> {
    let RestoredIdentity {
        payload,
        identity_chain,
    } = identity;
    let identity_id = crate::ids::hex_id(&payload.account_identity_id());
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::RecoveryCode {
                root,
                identity: Box::new(payload),
            },
        )
        .await?,
    );
    // Before the registration and before sync: the device this vault registers
    // and publishes is the one certified under the identity the chain ends at,
    // and every cert, record and bundle the replay brings verifies only under
    // an identity this vault can reach (ADR-0070). The record already
    // verified from this account's genesis, so a refusal here is storage.
    if let Some(chain) = identity_chain {
        if let Err(e) = core.seed_identity_chain(&chain) {
            core.shutdown().await;
            return Err(RestoreError::Core(e));
        }
    }

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
///
/// A `Stopped` session ends the wait: the relay refused this device, and
/// nothing in this call replaces the credential. A `Degraded` one does not.
/// A restored device's replay passes through it on purpose: a stream whose
/// snapshot record is sealed under a key the relay has trimmed is deferred,
/// which latches `Degraded`, until the account's key bundle supplies the
/// key and the record applies (ADR-0069). So the wait is resumed until the
/// budget passes, as it always was for that state.
async fn wait_until_live(
    core: &Core,
    budget_ms: u64,
    progress: &mut (dyn FnMut(RecoveryProgress) + Send),
) -> Result<(), String> {
    let deadline = core.now_ms().saturating_add(budget_ms);
    let mut reported: Option<RecoveryProgress> = None;
    let mut on_status = |status: &SyncStatus| {
        let now = replay_progress(status);
        let started = matches!(now, RecoveryProgress::Replaying { applied, total } if applied > 0 || total.is_some());
        if started && reported.as_ref() != Some(&now) {
            progress(now.clone());
            reported = Some(now);
        }
    };
    let waited = loop {
        let left = deadline.saturating_sub(core.now_ms());
        let waited = crate::sync::wait_until_caught_up(core, left, &mut on_status)
            .await
            .map_err(|e| e.to_string())?;
        match waited {
            Err(last) if last.state == SyncState::Degraded && core.now_ms() < deadline => {
                tokio::time::sleep(crate::sync::STATUS_POLL).await;
            }
            other => break other,
        }
    };
    waited.map_err(|last| {
        if last.state == SyncState::Stopped {
            format!("ended {:?}, {} pending", last.state, last.outbox_pending)
        } else {
            format!(
                "still {:?}, {} pending after {}s",
                last.state,
                last.outbox_pending,
                budget_ms / 1000
            )
        }
    })
}

#[cfg(test)]
mod tests;
