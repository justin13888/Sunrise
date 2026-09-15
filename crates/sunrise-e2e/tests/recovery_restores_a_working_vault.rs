//! Spend a recovery code: seal a blob, throw the vault away, and come back to
//! a **working vault** from nothing but twenty-four words and what the relay
//! serves.
//!
//! [`recovery_blob_round_trip`] already asserts that the identity survives —
//! that the words open the blob and the `ID_D_priv` inside is the one the
//! account's `key_envelope` ops were sealed to. It stops there, and that is
//! exactly the gap #180 names: an opened blob is not a vault. `recovery.md`
//! §Recovery flow steps 6-8 are three further things — mint a fresh vault
//! root, re-key this device under the restored identity and publish its cert,
//! and replay the identity-addressed `key_envelope` ops — and until they are
//! built a user who wrote their code down has something that decrypts and
//! nothing that opens.
//!
//! So this test is deliberately shaped as the failure it is guarding:
//!
//! 1. A founding device creates an account, seals a blob, uploads it, and
//!    **writes a task**. The task matters: content written *before* the
//!    recovery is the only thing that can tell a restored vault from an empty
//!    one, and an empty vault "opens" perfectly well.
//! 2. The device and its vault directory are dropped. The vault root goes with
//!    them — it is in no blob and no backup.
//! 3. A second, unrelated vault directory is opened with
//!    [`Unlock::RecoveryCode`] and a root it mints itself, and syncs.
//! 4. The task has to be there.
//!
//! Step 4 is the assertion the whole recovery design exists for, and nothing
//! in the tree asserted it before.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, SystemClock};
use sunrise_domain::{StreamDraft, TaskDraft};
use sunrise_e2e::{
    canonical_tasks, open_core_offline, open_recovered_core, signed_ws_factory, spawn_relay_with,
};
use sunrise_server::auth::StepUp;
use sunrise_server::{ServerConfig, StaticVerifier, Subject};

const ISSUER: &str = "https://idp.example";
/// A bearer whose holder authenticated moments ago — a completed step-up, and
/// the only kind the blob route serves.
const FRESH: &str = "alice-just-authenticated";
/// The founding device's vault root. It is in no recovery blob, and the
/// recovering vault below never sees it — that is the point.
const FOUNDING_ROOT: [u8; 32] = [0x7c; 32];
/// The root the recovering device mints for itself. Deliberately a different
/// value: a recovery that only worked because the two agreed would be
/// asserting nothing.
const RECOVERED_ROOT: [u8; 32] = [0x2e; 32];

fn verifier(now_ms: u64) -> StaticVerifier {
    StaticVerifier::default().with_step_up(
        FRESH,
        Subject::new(ISSUER, "alice"),
        StepUp {
            acr: None,
            amr: vec!["pwd".into()],
            auth_time_secs: Some(now_ms / 1000),
        },
    )
}

/// Fetch the blob the way a recovering client does, through the generated
/// client rather than a hand-rolled request.
async fn fetch_blob(base_url: &str, bearer: &str) -> String {
    let client = sunrise_relay_client::api::Client::new(base_url)
        .expect("client")
        .with_credential(
            "AccountToken",
            sunrise_relay_client::api::Credential::Bearer(
                sunrise_relay_client::api::SecretString::from(bearer.to_owned()),
            ),
        );
    client
        .get_recovery_blob(None)
        .await
        .expect("the relay serves the blob to a stepped-up bearer")
        .into_inner()
        .recovery_blob
}

/// The one that matters.
#[tokio::test]
async fn a_recovery_code_restores_a_vault_that_reads_what_was_written_before_it() {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let now_ms = clock.now_ms();
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), move |s| {
        s.with_verifier(Arc::new(verifier(now_ms)))
    })
    .await;
    let base_url = format!("http://{addr}");

    // ---- 1-2. An account is founded, used, and lost. ---------------------
    let (identity_id, typed) = found_and_lose_an_account(&base_url, addr, &clock, now_ms).await;

    // ---- 3. Nothing but the words and the relay. -------------------------
    let served = fetch_blob(&base_url, FRESH).await;
    let restored = sunrise_onboarding::recover_identity_from_code(
        &sunrise_onboarding::decode_recovery_blob(&served).expect("base64url"),
        &typed,
        &identity_id,
    )
    .expect("the code shown at creation opens the blob");

    let recovered_dir = tempfile::tempdir().expect("a vault dir");
    // Offline first, because registration needs a `D_S_pub` that does not
    // exist until the vault is open, and the signed transport needs the relay
    // device id that does not exist until registration. That is the same order
    // `sunrise_relay_client::bootstrap` imposes on every other client.
    let recovered = open_recovered_core(
        recovered_dir.path(),
        RECOVERED_ROOT,
        restored.clone(),
        addr,
        Arc::clone(&clock),
        None,
    )
    .await;

    // Step 6, second half: the fresh device joins the account at the relay.
    // `POST /accounts` is idempotent and re-sends no blob — the column is
    // write-once and already holds the one this recovery just spent.
    let recovered_outcome = tokio::time::timeout(
        Duration::from_secs(30),
        sunrise_relay_client::bootstrap(
            &base_url,
            FRESH,
            sunrise_onboarding::AccountCreateRequest {
                email: "alice@example.com".into(),
                identity_signing_pub: recovered.identity_signing_pub(),
                identity_dh_pub: recovered.identity_dh_pub(),
                recovery_blob: None,
                terms_at_ms: now_ms,
            },
            sunrise_relay_client::DeviceIdentity {
                device_pub_s: recovered.device_signing_pub(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some(sunrise_id::crockford::encode_bytes(&recovered.device_id())),
                nickname: "recovered".into(),
                platform: "linux".into(),
                app_version: None,
            },
        ),
    )
    .await
    .expect("bootstrap must not hang")
    .expect("the recovered device registers into the account it recovered");
    recovered
        .start_sync(signed_ws_factory(
            addr,
            Some(FRESH.to_owned()),
            recovered.device_signer(recovered_outcome.device_id),
        ))
        .expect("start sync");

    // Step 6 of `recovery.md` §Recovery flow: the recovered vault is the same
    // account, and is a *different device* within it — fresh `D_S`/`D_D` and a
    // cert signed by the restored `ID_S_priv`, which `Core::open` publishes.
    assert_eq!(
        recovered.identity_id(),
        identity_id,
        "a recovery rejoins the account it recovered, not a new one"
    );
    assert_eq!(
        recovered.identity_dh_pub(),
        restored.id_d_pub,
        "the vault's `ID_D_pub` is the blob's, so the envelopes sealed to it are this vault's to open"
    );
    assert!(
        recovered.holds_identity_key(),
        "a recovered vault holds `ID_D_priv` — without it every identity-addressed \
         `key_envelope` stays shut and this vault reads nothing. \
         docs/03-crypto/key-rotation.md §Revocation states the rule outright."
    );
    assert!(
        !recovered.device_cert().is_empty(),
        "the recovering device issues itself a cert under the restored identity"
    );

    // ---- 4. Step 8: the history is readable. -----------------------------
    wait_for_task(&recovered, "Renew passport", Duration::from_secs(60)).await;

    recovered.shutdown().await;
    relay.abort();
}

/// Stand up the account this test then recovers, and lose it.
///
/// Returns the only two things that survive the loss: the account's identity
/// id, which the recovering client reads back from `GET /accounts/me`, and the
/// twenty-four words, as a user retypes them off paper.
async fn found_and_lose_an_account(
    base_url: &str,
    addr: SocketAddr,
    clock: &Arc<dyn Clock>,
    now_ms: u64,
) -> ([u8; 16], String) {
    // ---- 1. The founding device: an account, a blob, and a task. ----------
    let founding_dir = tempfile::tempdir().expect("a vault dir");
    let founder =
        open_core_offline(founding_dir.path(), FOUNDING_ROOT, addr, Arc::clone(clock)).await;
    assert!(
        founder.holds_identity_key(),
        "the founding device is the one that can seal a blob"
    );

    let seed = [0x51u8; sunrise_crypto::bip39::RECOVERY_ENTROPY_LEN];
    let blob = founder
        .seal_recovery_blob(&seed)
        .expect("the creator seals");
    let code = sunrise_crypto::bip39::encode_recovery_code(&seed);

    let founder_outcome = tokio::time::timeout(
        Duration::from_secs(30),
        sunrise_relay_client::bootstrap(
            base_url,
            FRESH,
            sunrise_onboarding::AccountCreateRequest {
                email: "alice@example.com".into(),
                identity_signing_pub: founder.identity_signing_pub(),
                identity_dh_pub: founder.identity_dh_pub(),
                recovery_blob: Some(blob),
                terms_at_ms: now_ms,
            },
            sunrise_relay_client::DeviceIdentity {
                device_pub_s: founder.device_signing_pub(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some(sunrise_id::crockford::encode_bytes(&founder.device_id())),
                nickname: "founder".into(),
                platform: "linux".into(),
                app_version: None,
            },
        ),
    )
    .await
    .expect("bootstrap must not hang")
    .expect("the account and the device register");

    // Written *before* the recovery, which is what makes its presence
    // afterwards mean something. An empty vault would pass every other
    // assertion in this file.
    //
    // In its own Stream rather than straight into the Inbox, because that is
    // the harder half: a Stream mints an epoch of its own, and its key reaches
    // a recovering device only through the identity-addressed `key_envelope`
    // that ADR-0024 decision 4 exists to emit.
    let stream = founder
        .submit(Command::CreateStream(StreamDraft {
            name: "Travel".into(),
            ..Default::default()
        }))
        .await
        .expect("the founding device makes a stream")
        .entity;
    founder
        .submit(Command::CreateTask(TaskDraft {
            title: "Renew passport".into(),
            stream_id: Some(stream),
            ..Default::default()
        }))
        .await
        .expect("the founding device writes a task");

    // Push it to the relay and wait for the outbox to drain, so the op is on
    // the relay before the only device that holds it goes away.
    founder
        .start_sync(signed_ws_factory(
            addr,
            Some(FRESH.to_owned()),
            founder.device_signer(founder_outcome.device_id),
        ))
        .expect("start sync");
    sunrise_e2e::wait_pending_zero(&founder, Duration::from_secs(30)).await;

    let identity_id = founder.identity_id();
    // The words, as a user reads them off paper and retypes them. Upper-cased
    // for the same reason the round-trip test does it: what is written down is
    // not necessarily what the encoder emitted.
    let typed = code.reveal().to_uppercase();

    // ---- 2. Every device is gone. ---------------------------------------
    founder.shutdown().await;
    drop(founder);
    drop(founding_dir);

    (identity_id, typed)
}

/// Poll until `title` materializes, or fail saying what step 8 is.
///
/// `tokio::time::Instant` rather than `std::time::Instant`, which the
/// workspace's determinism lint disallows outright.
async fn wait_for_task(core: &Core, title: &str, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tasks = canonical_tasks(core).await;
        if tasks.iter().any(|t| t.title == title) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recovered vault never read the task written before the recovery. \
             This is `recovery.md` §Recovery flow step 8: every `(stream_id, epoch)` \
             key is sealed to `ID_D_pub` as well as to each device, so a restored \
             identity opens the history. Saw {tasks:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
