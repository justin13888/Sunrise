//! An account founded through the Apple seam comes back on a new device from
//! its recovery code, through the Apple seam, with its history readable.
//!
//! #349: until `SunriseCore::recover_account` existed the recovery code could
//! be spent only by `sunrise recover`, so a user whose every Mac and iPhone was
//! gone could not restore on a Mac or an iPhone. This drives the whole loop
//! the app drives, with no CLI code in it: found a vault, seal and upload its
//! blob, write a task, push it, lose the device, then restore from the words
//! alone and read the task back.
//!
//! The last assertion is the one #180 is about. A recovery that opened an
//! empty vault would pass every other check here, and would look exactly
//! like success.
//!
//! The relay runs the default self-host `NullVerifier`, the one configuration
//! exempt from the recovery step-up; `recovery_blob_round_trip.rs` covers the
//! gate against a real verifier.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sunrise_core_bindings::{
    BindingError, CoreQuery, CoreQueryResult, RecoveryListener, RecoveryStep, SunriseCore,
};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::ServerConfig;

const BEARER: &str = "self-host";
const FOUNDING_ROOT: [u8; 32] = [0x4e; 32];
const RECOVERED_ROOT: [u8; 32] = [0x4f; 32];

/// Every step the seam reported, in order.
#[derive(Default)]
struct Steps(Mutex<Vec<RecoveryStep>>);

impl RecoveryListener for Steps {
    fn on_step(&self, step: RecoveryStep) {
        if let Ok(mut g) = self.0.lock() {
            g.push(step);
        }
    }
}

impl Steps {
    fn seen(&self) -> Vec<RecoveryStep> {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Wait for the driver to push everything it holds.
async fn drained(core: &SunriseCore) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(CoreQueryResult::SyncStatus { status }) =
                core.query(CoreQuery::SyncStatus).await
            {
                if status.outbox_pending == 0 && status.state == sunrise_sync::SyncState::Live {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the founding device never pushed its ops");
}

/// Found an account through the seam, write one task, push it, and return the
/// recovery code. The vault is shut down; the caller drops its directory.
async fn found(dir: &std::path::Path, base_url: &str) -> String {
    let core = SunriseCore::open(
        dir.to_string_lossy().into_owned(),
        FOUNDING_ROOT.to_vec(),
        "e2e".into(),
        None,
    )
    .await
    .expect("the app opens a vault");
    let outcome = core
        .bootstrap_account(
            base_url.to_owned(),
            BEARER.to_owned(),
            "alice@example.com".to_owned(),
            "Alice's Mac".to_owned(),
            1,
        )
        .await
        .expect("the app publishes its vault");
    core.capture("Renew passport".into(), "UTC".into())
        .await
        .expect("capture");
    core.start_sync(
        base_url.to_owned(),
        Some(BEARER.to_owned()),
        Some(outcome.device_id),
    )
    .expect("sync starts");
    drained(&core).await;
    core.shutdown().await;
    outcome
        .recovery_code
        .expect("a founding device produces a code")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recovery_code_restores_the_account_and_its_history_through_the_seam() {
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| s).await;
    let base_url = format!("http://{addr}");

    let founding = tempfile::tempdir().expect("a vault dir");
    let code = found(founding.path(), &base_url).await;
    // The device is gone. The words are all that is left.
    drop(founding);

    let restored = tempfile::tempdir().expect("a vault dir");
    let steps = Arc::new(Steps::default());
    let core = tokio::time::timeout(
        Duration::from_secs(90),
        SunriseCore::recover_account(
            restored.path().to_string_lossy().into_owned(),
            RECOVERED_ROOT.to_vec(),
            "e2e".into(),
            base_url.clone(),
            BEARER.to_owned(),
            code,
            "Alice's new iPhone".into(),
            Arc::clone(&steps) as Arc<dyn RecoveryListener>,
            60_000,
        ),
    )
    .await
    .expect("the recovery must not hang")
    .expect("the words restore the account");

    let seen = steps.seen();
    assert_eq!(seen.first(), Some(&RecoveryStep::BlobFetched), "{seen:?}");
    assert!(seen.contains(&RecoveryStep::IdentityOpened), "{seen:?}");
    assert!(
        seen.iter()
            .any(|s| matches!(s, RecoveryStep::DeviceRegistered { relay_device_id } if !relay_device_id.is_empty())),
        "the caller has to be handed the relay device id: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&RecoveryStep::CaughtUp), "{seen:?}");

    assert!(
        core.holds_identity_key(),
        "a restored device holds ID_D_priv, so it can seal a new code"
    );

    // The assertion that matters: what was written before the device was lost
    // is readable after the recovery. Its Stream key reached this device only
    // through an identity-addressed `key_envelope`.
    let CoreQueryResult::Tasks { tasks } = core.query(CoreQuery::Inbox).await.expect("inbox")
    else {
        panic!("Inbox must return tasks");
    };
    assert!(
        tasks.iter().any(|t| t.title == "Renew passport"),
        "the restored vault never read the task written before the device was lost: {:?}",
        tasks.iter().map(|t| &t.title).collect::<Vec<_>>()
    );

    core.shutdown().await;
    relay.abort();
}

/// A recovery into a directory that already holds a vault is refused before
/// the relay is asked anything, and leaves the vault there untouched.
#[tokio::test(flavor = "multi_thread")]
async fn a_recovery_into_an_existing_vault_is_refused_before_anything_is_written() {
    let dir = tempfile::tempdir().expect("a vault dir");
    std::fs::write(dir.path().join("vault.db"), b"somebody's vault").expect("write");

    let err = SunriseCore::recover_account(
        dir.path().to_string_lossy().into_owned(),
        RECOVERED_ROOT.to_vec(),
        "e2e".into(),
        // Nothing listens here: a refusal that reached the network would fail
        // differently.
        "http://127.0.0.1:9".into(),
        BEARER.to_owned(),
        "abandon".into(),
        "phone".into(),
        Arc::new(Steps::default()),
        1_000,
    )
    .await
    .expect_err("refused");
    assert!(
        matches!(&err, BindingError::RecoveryRefused(m) if m.contains("already holds a vault")),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("vault.db")).expect("still there"),
        b"somebody's vault"
    );
}

/// A mistyped code is refused as a code, before the relay is asked anything.
#[tokio::test(flavor = "multi_thread")]
async fn a_mistyped_code_is_refused_as_a_code_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("a vault dir");
    let err = SunriseCore::recover_account(
        dir.path().to_string_lossy().into_owned(),
        RECOVERED_ROOT.to_vec(),
        "e2e".into(),
        "http://127.0.0.1:9".into(),
        BEARER.to_owned(),
        "abandon ability able".into(),
        "phone".into(),
        Arc::new(Steps::default()),
        1_000,
    )
    .await
    .expect_err("three words are not a code");
    assert!(matches!(err, BindingError::RecoveryCode(_)), "{err:?}");
    assert!(
        std::fs::read_dir(dir.path())
            .expect("the dir")
            .next()
            .is_none(),
        "nothing may be written for a code that could never open the blob"
    );
}
