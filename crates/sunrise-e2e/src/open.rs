//! Opening cores: [`Core`] instances on a vault directory, keyed as a paired
//! device or a recovered one, with their sync driver wired to the relay.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::relay::ws_factory;
use sunrise_core::{Clock, Core, CoreConfig, SyncConfig, SystemRng, TransportFactory, Unlock};
use sunrise_crypto::keys::VaultRootKey;

/// App identity string the harness cores advertise in their sync `Hello`.
const APP_ID: &str = "0.1.0+e2e";

/// Anti-entropy resync period for harness cores.
///
/// The driver's inbound-loss recovery is a periodic re-`Subscribe` with current
/// cursors (`sunrise_core::sync_driver`), at 30 s in production. A chaos
/// scenario has to watch that mechanism actually recover, inside a test, so the
/// harness clocks it fast — the same knob a backoff constant gets, not a
/// different code path.
const HARNESS_RESYNC_INTERVAL: Duration = Duration::from_millis(200);

/// Open (or reopen) a [`Core`] on `vault_dir` keyed by the shared paired-device
/// `root`, then start its sync driver against the relay at `addr`.
///
/// The paired-device model gives both replicas the **same** vault root (so
/// their per-stream keys match and envelopes decrypt) but distinct random
/// device identities (the keychain seeds a fresh id per vault).
pub async fn open_synced_core(
    vault_dir: &Path,
    root: [u8; 32],
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
) -> Arc<Core> {
    open_core_with_factory(vault_dir, root, addr, clock, ws_factory(addr)).await
}

/// Like [`open_synced_core`] but starts the driver against a caller-supplied
/// [`TransportFactory`] — e.g. a [`crate::toxic_ws_factory`] for chaos
/// scenarios. The vault is still keyed by the shared paired-device `root`;
/// `addr` only seeds the [`CoreConfig`] sync URL (the actual transport comes
/// from `factory`).
pub async fn open_core_with_factory(
    vault_dir: &Path,
    root: [u8; 32],
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
    factory: TransportFactory,
) -> Arc<Core> {
    // Chaos scenarios have to observe autonomous recovery inside a test run, so
    // the anti-entropy backstop is clocked in hundreds of milliseconds rather
    // than the production 30 s. The mechanism is the same one; only its period
    // is tuned, exactly like a backoff constant.
    open_core_paired(vault_dir, root, addr, clock, Some(factory), None).await
}

/// Open a **second device on the same account** as `existing`.
///
/// This is what replaced `trust_each_other`. Two vaults that merely share a
/// vault root are two separate accounts under ADR-0024: each mints its own
/// identity, and Stream keys are random rather than derived from the root, so
/// neither could read the other's ops however many certs they exchanged.
/// Joining an account means being handed its `PairingPayload` — the identity
/// keys and every Stream key — which can only happen at `Core::open`, because
/// the identity a vault belongs to is decided when it is created.
///
/// The payload travels in-process here rather than through the Noise channel,
/// which `paired_devices_converge` exercises separately. What this harness
/// reproduces is the *state* a completed pairing leaves behind.
pub async fn open_paired_core(
    vault_dir: &Path,
    existing: &Core,
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
) -> Arc<Core> {
    open_paired_core_with_factory(vault_dir, existing, addr, clock, ws_factory(addr)).await
}

/// [`open_paired_core`] against a caller-supplied transport factory.
pub async fn open_paired_core_with_factory(
    vault_dir: &Path,
    existing: &Core,
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
    factory: TransportFactory,
) -> Arc<Core> {
    let payload = pair_with(existing);
    let root = payload.vault_root;
    open_core_paired(
        vault_dir,
        root,
        addr,
        clock,
        Some(factory),
        Some(Box::new(payload)),
    )
    .await
}

/// Run a whole pairing against `existing` and return what the joiner installs.
///
/// Three messages, not one, since #105: the sponsor offers its account's public
/// identity, the joiner mints `D_S`/`D_D` and asks to be certified, and the
/// sponsor — which is the only device in the account holding `ID_S_priv` —
/// issues the cert and hands over the root and the Stream keys. A harness that
/// short-circuited that would be testing a pairing no client performs.
///
/// The device seeds are drawn fresh per call so two devices paired from the
/// same sponsor in one test get different `device_id`s. A fixed seed here would
/// give them the same id, and the second would silently overwrite the first's
/// `devices` row.
///
/// Panics rather than returning an error: a sponsor that cannot sponsor is a
/// broken fixture, not a scenario any of these tests is about.
#[must_use]
pub fn pair_with(existing: &Core) -> sunrise_pairing::PairingPayload {
    use sunrise_core::Rng as _;
    let rng = sunrise_core::SystemRng;
    let mut seed_s = [0u8; 32];
    let mut seed_d = [0u8; 32];
    rng.fill_bytes(&mut seed_s);
    rng.fill_bytes(&mut seed_d);
    existing
        .pair_device_in_process("e2e-joiner".into(), "test".into(), seed_s, seed_d)
        .expect("the sponsor holds ID_S_priv and can issue a cert")
}

/// Open a core with **no sync driver running**, so the caller can register its
/// device at the relay before deciding what transport to give it.
///
/// The device binding needs both halves of a fact the ordinary helpers cannot
/// have at once: the relay's id for this device, which only exists after
/// registration, and the vault's signing key, which only exists after the open.
/// Splitting the open from the `start_sync` is what lets a test hold both —
/// and it is the same order a real client runs in, since
/// `sunrise_relay_client::bootstrap` also registers before any signed request.
pub async fn open_core_offline(
    vault_dir: &Path,
    root: [u8; 32],
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
) -> Arc<Core> {
    open_core_paired(vault_dir, root, addr, clock, None, None).await
}

/// [`open_core_offline`], as a second device on `existing`'s account.
pub async fn open_paired_core_offline(
    vault_dir: &Path,
    existing: &Core,
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
) -> Arc<Core> {
    let payload = pair_with(existing);
    let root = payload.vault_root;
    open_core_paired(vault_dir, root, addr, clock, None, Some(Box::new(payload))).await
}

/// Open a vault from an **opened recovery blob** — the state a device is in
/// after `sunrise recover` has fetched the blob and spent the user's 24 words.
///
/// `root` is a fresh vault root this "device" mints for itself: the blob
/// carries the account identity and never the vault root
/// (`docs/03-crypto/recovery.md` §Recovery flow step 8), so there is nothing to
/// be handed one by and nothing to share with the vault that was lost. That is
/// the difference between this and [`open_paired_core`], and it is the whole
/// reason a recovery has to read its Stream keys out of the op log.
pub async fn open_recovered_core(
    vault_dir: &Path,
    root: [u8; 32],
    identity: sunrise_crypto::recovery::RecoveryPayload,
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
    factory: Option<TransportFactory>,
) -> Arc<Core> {
    open_core_unlocked(
        vault_dir,
        addr,
        clock,
        factory,
        Unlock::RecoveryCode {
            root: VaultRootKey::from_bytes(root),
            identity: Box::new(identity),
        },
    )
    .await
}

pub(crate) async fn open_core_paired(
    vault_dir: &Path,
    root: [u8; 32],
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
    factory: Option<TransportFactory>,
    paired: Option<Box<sunrise_pairing::PairingPayload>>,
) -> Arc<Core> {
    open_core_unlocked(
        vault_dir,
        addr,
        clock,
        factory,
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(root),
            paired,
        },
    )
    .await
}

async fn open_core_unlocked(
    vault_dir: &Path,
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
    factory: Option<TransportFactory>,
    unlock: Unlock,
) -> Arc<Core> {
    try_open_core_unlocked(vault_dir, addr, clock, factory, unlock)
        .await
        .expect("open core")
}

/// [`open_core_unlocked`], handing back the open's refusal instead of
/// panicking on it.
///
/// The cross-version harness needs the refusal itself: `HEAD` declining to
/// open a vault an older build wrote is a finding about the invariant, not a
/// broken fixture.
pub(crate) async fn try_open_core_unlocked(
    vault_dir: &Path,
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
    factory: Option<TransportFactory>,
    unlock: Unlock,
) -> Result<Arc<Core>, sunrise_core::CoreError> {
    let cfg = CoreConfig {
        sync: Some(
            SyncConfig::new(format!("http://{addr}")).with_resync_interval(HARNESS_RESYNC_INTERVAL),
        ),
        ..CoreConfig::with_clock(vault_dir.to_path_buf(), APP_ID, clock, Arc::new(SystemRng))
    };
    let core = Arc::new(Core::open(cfg, unlock).await?);
    if let Some(factory) = factory {
        core.start_sync(factory).expect("start sync");
    }
    Ok(core)
}
