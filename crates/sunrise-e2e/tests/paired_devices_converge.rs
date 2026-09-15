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
use sunrise_domain::{StreamDraft, TaskDraft};
use sunrise_e2e::{
    open_paired_core, open_synced_core, spawn_relay, wait_live, wait_tasks_converge,
};
use sunrise_pairing::{
    decode_pairing_grant, decode_pairing_offer, decode_pairing_request, PairingJoiner,
    PairingSession, Role,
};

/// 32 unpredictable bytes, from the same CSPRNG that backs the Noise statics.
///
/// The joiner's `D_S_priv` is derived from one of these and every op the paired
/// device ever writes is signed under it, so a fixture constant would be a
/// fixture constant standing in for the device's whole identity.
fn random_seed() -> [u8; 32] {
    let material = PairingSession::generate_static_key().expect("entropy");
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&material[..32]);
    seed
}

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

async fn create_stream(core: &Core, name: &str) -> sunrise_id::EntityRef {
    core.submit(Command::CreateStream(StreamDraft {
        name: name.into(),
        ..Default::default()
    }))
    .await
    .expect("create stream")
    .entity
}

async fn create_task_in(core: &Core, stream: sunrise_id::EntityRef, title: &str) {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        stream_id: Some(stream),
        ..Default::default()
    }))
    .await
    .expect("create task in stream");
}

/// Wait until `core` can read `n` tasks in `stream`.
///
/// Readable is the operative word: the ops arrive as ciphertext, and the only
/// thing that turns them into rows is a `key_envelope` op having delivered that
/// stream's key. A device that never received one sits at zero forever.
async fn wait_stream_tasks(
    core: &Core,
    stream: sunrise_id::EntityRef,
    n: usize,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = match core
            .query(Query::StreamTasks(stream))
            .await
            .expect("stream tasks")
        {
            QueryResult::Tasks(t) | QueryResult::StreamTasks(t) => t.len(),
            other => panic!("expected Tasks, got {other:?}"),
        };
        if count == n {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {n} tasks in the post-pairing stream (saw {count})"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn inbox_titles(core: &Core) -> Vec<String> {
    match core.query(Query::Inbox).await.expect("inbox") {
        QueryResult::Tasks(t) | QueryResult::StreamTasks(t) => {
            t.iter().map(|x| x.title.clone()).collect()
        }
        other => panic!("expected Tasks, got {other:?}"),
    }
}

/// Drive the three pairing messages across a confirmed channel, and assert that
/// nothing secret crosses any of them in the clear.
///
/// Extracted from the test below because it is the part that grew: pairing used
/// to be one sealed blob in one direction, and it is offer, request and grant
/// alternating since the account's signing key stopped travelling (#105). The
/// sponsor cannot certify keys the joiner has not minted yet, so there is no
/// shorter form of this.
///
/// Returns the joiner — which is holding the device secrets it minted and has
/// not sent — and the sealed grant, for the caller to open.
fn exchange_pairing_messages(
    sponsor: &Core,
    sponsor_channel: &mut sunrise_pairing::PairedChannel,
    joiner_channel: &mut sunrise_pairing::PairedChannel,
    sponsor_root: [u8; 32],
) -> (PairingJoiner, Vec<u8>) {
    // 1. The offer: the account's public identity, and no secret at all.
    let offer = sponsor
        .export_pairing_offer()
        .expect("export the pairing offer");
    let offer_wire = sponsor_channel
        .send(&offer.encode().expect("encode the offer"))
        .expect("send the offer");
    let offer_b = decode_pairing_offer(&joiner_channel.receive(&offer_wire).expect("receive"))
        .expect("decode the offer on the new device");
    assert_eq!(
        offer_b.identity_id,
        sponsor.identity_id(),
        "the paired device joins the sending device's account, not a new one"
    );

    // 2. The request: keys the joining device just minted, public halves only.
    let joiner = PairingJoiner::new(
        offer_b,
        "the paired device".into(),
        "test".into(),
        random_seed(),
        random_seed(),
    );
    let request_wire = joiner_channel
        .send(&joiner.request().encode().expect("encode the request"))
        .expect("send the request");
    let request = decode_pairing_request(
        &sponsor_channel
            .receive(&request_wire)
            .expect("receive the request"),
    )
    .expect("decode the request on the sponsor");

    // 3. The grant: the cert the sponsor signed over those keys, the vault root
    //    and every Stream key.
    let grant = sponsor
        .issue_pairing_grant(&request)
        .expect("the sponsor holds ID_S_priv and issues the cert");
    let stream_keys: Vec<[u8; 32]> = grant
        .stream_keys
        .values()
        .flat_map(|epochs| epochs.values().copied())
        .collect();
    assert!(
        !stream_keys.is_empty(),
        "A has minted at least the meta and inbox keys by now"
    );
    let grant_wire = sponsor_channel
        .send(&grant.encode().expect("encode the grant"))
        .expect("send the grant");

    // Nothing the exchange carries may appear in the clear on the wire, on any
    // of the three legs. The vault root and every Stream key are in the grant.
    //
    // Neither identity private key is in this list, because neither is in any
    // message any more. Checking that an absent field does not appear on the
    // wire would pass whatever happened; what holds their absence is
    // `sunrise-pairing`'s `no_message_in_the_exchange_carries_an_identity_private_key`,
    // `sunrise_core::keychain`'s `a_paired_device_cannot_open_the_identity_copy`
    // and `sunrise-e2e`'s own `device_revocation` test.
    let mut secrets: Vec<[u8; 32]> = vec![sponsor_root];
    secrets.extend(stream_keys);
    for leg in [&offer_wire, &request_wire, &grant_wire] {
        for secret in &secrets {
            assert!(
                !leg.windows(32).any(|w| w == secret),
                "a secret from the pairing exchange appeared in the clear on the wire"
            );
        }
    }

    (joiner, grant_wire)
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

    // Three messages over the channel, not one, and nothing secret appears in
    // the clear on any of them. See `exchange_pairing_messages`.
    let (joiner, wire) =
        exchange_pairing_messages(&core_a, &mut existing_channel, &mut new_channel, root_a);

    let received = new_channel.receive(&wire).expect("receive the grant");
    let payload_b = joiner
        .accept(decode_pairing_grant(&received).expect("decode the grant"))
        .expect("the joiner accepts a cert issued for its own keys");
    let root_b = payload_b.vault_root;
    assert_eq!(
        root_b, root_a,
        "the paired device must recover exactly the sending device's root"
    );
    assert_eq!(
        payload_b.identity_id,
        core_a.identity_id(),
        "the paired device joins the sending device's account, not a new one"
    );

    // ---- Device B opens its own vault with the transferred payload ----
    // No trust step: B's cert is signed by the account identity it just
    // received, and it publishes that cert as an op at open. A device is
    // trusted because the identity vouched for it — which is what makes
    // revoking one meaningful.
    let core_b = open_paired_core(dir_b.path(), &core_a, addr, clock.clone()).await;
    wait_live(&core_b, TIMEOUT).await;

    // B's first subscribe replayed A's pre-pairing op while B had not yet seen
    // A's `device_cert` op, so that op was refused as `UnknownDevice` and the
    // relay does not redeliver. Reconnecting replays the retained ring with the
    // cert in place. This is exactly what a real client does after completing a
    // pair, and it doubles as a check that the vault lock is released on
    // shutdown and reacquirable — which it was not before the lock rewrite.
    core_b.shutdown().await;
    drop(core_b);
    let core_b = open_synced_core(dir_b.path(), root_b, addr, clock).await;
    wait_live(&core_b, TIMEOUT).await;

    // The case envelopes now have to earn: a Stream created on A *after*
    // pairing is readable on B, which can only happen if A's `key_envelope`
    // op reached B and B absorbed the key.
    let stream_after = create_stream(&core_a, "created after pairing").await;
    create_task_in(&core_a, stream_after, "in the new stream").await;

    // ---- They converge, in both directions ----
    create_task(&core_b, "written on the paired device").await;
    create_task(&core_a, "written on the original device").await;

    // Four, not three: `canonical_tasks` is the whole table, so the task in
    // the post-pairing Stream counts alongside the three in the Inbox.
    wait_tasks_converge(&core_a, &core_b, 4, TIMEOUT).await;

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

    wait_stream_tasks(&core_b, stream_after, 1, TIMEOUT).await;

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
