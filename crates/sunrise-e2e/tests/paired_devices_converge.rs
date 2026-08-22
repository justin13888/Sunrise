//! Pairing end to end: a second device gets its vault root over Noise, and the
//! two replicas then sync for real.
//!
//! Every other test in this suite hands both replicas the same `[0x42; 32]`
//! literal. That proves the sync stack converges, but it quietly assumes away
//! the hardest part of a multi-device product: **how the second device ever
//! obtains the key.** Until `sunrise-pairing` was implemented, the honest
//! answer was that it could not — encryption was real, key distribution was
//! bypassed.
//!
//! This test closes that gap. There is deliberately **no shared key constant
//! in this file**. Device A generates a root; device B learns it only by
//! completing a Noise XX handshake, confirming the SAS, and reading it off the
//! encrypted channel. Then the two exchange device certs and converge over the
//! real relay.

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::TaskDraft;
use sunrise_e2e::{
    open_synced_core, spawn_relay, trust_each_other, wait_live, wait_tasks_converge,
};
use sunrise_pairing::{PairingSession, Role};

const TIMEOUT: Duration = Duration::from_secs(20);

/// Run the three-message XX transcript between two in-process sessions,
/// standing in for the relay that would carry them in production. The relay
/// only routes these bytes — it cannot read them — so passing them directly is
/// faithful to what the server would do.
fn complete_handshake() -> (PairingSession, PairingSession) {
    let a_key = PairingSession::generate_static_key().expect("static key");
    let b_key = PairingSession::generate_static_key().expect("static key");
    // The *new* device initiates: it generated the QR the existing device
    // scanned.
    let mut new_device = PairingSession::new(Role::NewDevice, &b_key).expect("new device session");
    let mut existing = PairingSession::new(Role::ExistingDevice, &a_key).expect("existing session");

    let m1 = new_device.write_message(&[]).expect("msg 1");
    existing.read_message(&m1).expect("read msg 1");
    let m2 = existing.write_message(&[]).expect("msg 2");
    new_device.read_message(&m2).expect("read msg 2");
    let m3 = new_device.write_message(&[]).expect("msg 3");
    existing.read_message(&m3).expect("read msg 3");

    (new_device, existing)
}

async fn create_task(core: &Core, title: &str) -> sunrise_id::EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

async fn inbox_titles(core: &Core) -> Vec<String> {
    match core.query(Query::Inbox).await.expect("inbox") {
        QueryResult::Tasks(t) | QueryResult::StreamTasks(t) => {
            t.iter().map(|x| x.title.clone()).collect()
        }
        other => panic!("expected Tasks, got {other:?}"),
    }
}

/// The whole point: pair, transfer, converge — with no shared key literal.
#[tokio::test]
async fn a_paired_device_receives_the_vault_root_and_syncs() {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("tmp a");
    let dir_b = tempfile::tempdir().expect("tmp b");
    let clock = Arc::new(SystemClock);

    // ---- Device A: an existing vault with a root nobody else knows ----
    // Generated here rather than hardcoded, so nothing downstream can succeed
    // by accident of a shared constant.
    let root_a: [u8; 32] = {
        let mut seed = [0u8; 32];
        // Derive a per-run root from the pairing crate's CSPRNG-backed key
        // generation, so this value is genuinely unpredictable.
        let material = PairingSession::generate_static_key().expect("entropy");
        seed.copy_from_slice(&material[..32]);
        seed
    };

    let core_a = open_synced_core(dir_a.path(), root_a, addr, clock.clone()).await;
    wait_live(&core_a, TIMEOUT).await;
    create_task(&core_a, "written before pairing").await;

    // ---- Pair ----
    let (new_side, existing_side) = complete_handshake();

    // Both users see the same six digits. This is the only authentication the
    // numeric path has, so assert it rather than assuming it.
    let sas_new = new_side.sas().expect("sas on new device");
    let sas_existing = existing_side.sas().expect("sas on existing device");
    assert_eq!(
        sas_new, sas_existing,
        "both devices must display the same confirmation code"
    );

    // Both users tap "Match".
    let mut new_channel = new_side.into_channel(true).expect("new device channel");
    let mut existing_channel = existing_side
        .into_channel(true)
        .expect("existing device channel");

    // The existing device hands over the root through the encrypted channel.
    let exported = core_a.export_vault_root_for_pairing();
    let wire = existing_channel
        .send(exported.as_bytes())
        .expect("send vault root");
    assert!(
        !wire.windows(32).any(|w| w == root_a),
        "the vault root must not appear in the clear on the wire"
    );

    let received = new_channel.receive(&wire).expect("receive vault root");
    let mut root_b = [0u8; 32];
    root_b.copy_from_slice(&received);
    assert_eq!(
        root_b, root_a,
        "the paired device must recover exactly the sending device's root"
    );

    // ---- Device B opens its own vault with the transferred root ----
    let core_b = open_synced_core(dir_b.path(), root_b, addr, clock.clone()).await;
    wait_live(&core_b, TIMEOUT).await;

    // Pairing establishes the *key*; each side must still accept the other's
    // device cert before it will apply the other's ops. The key alone is not
    // authorisation — an op from an untrusted device is rejected before it is
    // even decrypted.
    trust_each_other(&core_a, &core_b).await;

    // B's first subscribe replayed A's pre-pairing op while B still had an
    // empty trust store, so that op was refused as `UnknownDevice` and the
    // relay does not redeliver. Reconnecting replays the retained ring with
    // trust in place. This is exactly what a real client does after completing
    // a pair, and it doubles as a check that the vault lock is released on
    // shutdown and reacquirable — which it was not before the lock rewrite.
    core_b.shutdown().await;
    drop(core_b);
    let core_b = open_synced_core(dir_b.path(), root_b, addr, clock).await;
    wait_live(&core_b, TIMEOUT).await;

    // ---- They converge, in both directions ----
    create_task(&core_b, "written on the paired device").await;
    create_task(&core_a, "written on the original device").await;

    wait_tasks_converge(&core_a, &core_b, 3, TIMEOUT).await;

    let mut titles = inbox_titles(&core_b).await;
    titles.sort();
    assert_eq!(
        titles,
        vec![
            "written before pairing".to_string(),
            "written on the original device".to_string(),
            "written on the paired device".to_string(),
        ],
        "the paired device must see history from before it existed, and both \
         devices' later writes"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
    relay.abort();
}

/// A device that did not complete the handshake cannot read the transfer, so a
/// relay sitting in the middle learns nothing by capturing the ciphertext.
#[tokio::test]
async fn an_unpaired_listener_cannot_read_the_transferred_root() {
    let (_new_a, existing_a) = complete_handshake();
    let (new_b, _existing_b) = complete_handshake();

    let mut sender = existing_a.into_channel(true).expect("sender channel");
    // `eavesdropper` completed a *different* handshake — which is exactly what
    // a man in the middle ends up holding.
    let mut eavesdropper = new_b.into_channel(true).expect("eavesdropper channel");

    let secret = [0xABu8; 32];
    let wire = sender.send(&secret).expect("send");
    assert!(
        eavesdropper.receive(&wire).is_err(),
        "a channel from an unrelated handshake must not decrypt the transfer"
    );
}
