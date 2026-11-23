//! The relay's half of withdrawing a revocation (ADR-0056 §5), end to end
//! against a relay that requires the device binding.
//!
//! `device_bound_sync.rs` proves a signed revocation reaches such a relay and
//! `relay_learns_revocation.rs` proves it on one without the binding. This is
//! the reverse: A revokes B, the relay refuses B, A withdraws, and the
//! `relay_reinstatement_intents` row the withdrawal queues is drained by A's
//! own sync driver through `POST /api/v1/devices/by-vault-id/{id}/reinstate`,
//! after which the relay accepts B's signed requests again. A third device C,
//! which never revoked B, is refused when it asks the relay the same thing,
//! because the relay accepts a reinstatement only from the device it recorded
//! as the revoker.
//!
//! The assertions are on the relay's own row and on a signed request from B,
//! not on the queue, for the reason `relay_learns_revocation.rs` gives: an
//! intent that is drained into a call the relay refuses would look the same
//! from the queue's side.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_core::feature::{REVOKE_WITHDRAW_AUTHORITY_FEATURE, REVOKE_WITHDRAW_FEATURE};
use sunrise_core::{
    Clock, Command, Core, Query, QueryResult, RelayReinstatement, RevokeReason, SystemClock,
};
use sunrise_e2e::{
    open_core_offline, open_paired_core_offline, signed_ws_factory, spawn_relay_with, wait_live,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_server::store::NewDevice;
use sunrise_server::{MetadataStore, ServerConfig, StaticVerifier, Store, Subject};
use sunrise_sync::{ReinstateOutcome, SseTransport, Transport, TransportError};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};

const ISSUER: &str = "https://idp.example";
const BEARER: &str = "alice-token";
const ROOT: [u8; 32] = [0x6c; 32];
const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(25);

fn subject() -> Subject {
    Subject::new(ISSUER, "alice")
}

/// A relay that requires a device binding, under its default configuration
/// for a named issuer, as `device_bound_sync.rs` spawns it.
async fn spawn_bound_relay() -> (
    SocketAddr,
    tokio::task::JoinHandle<()>,
    Arc<dyn MetadataStore>,
) {
    let config = ServerConfig {
        oidc_issuer: Some(ISSUER.to_owned()),
        oidc_client_id: Some("sunrise".to_owned()),
        ..ServerConfig::default()
    };
    assert!(config.device_sig_required());
    let mut captured: Option<Arc<dyn MetadataStore>> = None;
    let (addr, handle) = spawn_relay_with(config, |state| {
        captured = Some(state.store.clone());
        state.with_verifier(Arc::new(StaticVerifier::default().with(BEARER, subject())))
    })
    .await;
    (
        addr,
        handle,
        captured.expect("the harness hands back the relay's own store"),
    )
}

/// Register `core` at the relay as `sunrise_relay_client::bootstrap` does,
/// and start its driver bound to that row. Returns the relay's id for it.
fn register_and_start(
    store: &Store,
    account_id: &str,
    core: &Arc<Core>,
    addr: SocketAddr,
    nickname: &str,
) -> String {
    let relay_id = store
        .register_device(
            account_id,
            &NewDevice {
                device_pub_s: sunrise_http_sig::device_pub_b64(&core.device_signing_pub()),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some(sunrise_id::crockford::encode_bytes(&core.device_id())),
                nickname: nickname.to_owned(),
                platform: "linux".to_owned(),
                app_version: None,
            },
            SystemClock.now_ms(),
        )
        .expect("register the device")
        .device_id;
    core.sync_credential().set(Some(BEARER.to_owned()));
    core.start_sync(signed_ws_factory(
        addr,
        core.device_signer(relay_id.clone()),
    ))
    .expect("start sync");
    relay_id
}

/// A transport signed as `core`'s relay row, outside its driver.
fn signed_transport(addr: SocketAddr, core: &Arc<Core>, relay_id: &str) -> SseTransport {
    SseTransport::connect_with_bearer(&format!("http://{addr}"), Some(BEARER))
        .with_device_signer(core.device_signer(relay_id.to_owned()))
}

fn hello() -> Hello {
    Hello {
        client_app_v: "1.0.0+test".into(),
        client_platform: "test".into(),
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        doc_schema_min: u32::from(DOC_SCHEMA_FLOOR),
        doc_schema_max: u32::from(DOC_SCHEMA_V),
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

/// Drive one signed `Hello`, the first request a session makes.
async fn handshake(t: &mut SseTransport) -> Result<(), TransportError> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    t.send_frame(encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap())
        .await?;
    let buf = t.recv_frame().await?.expect("a HelloAck, not a close");
    assert_eq!(decode_frame(&buf).unwrap().0.msg_kind, MsgKind::HelloAck);
    Ok(())
}

/// Whether the relay's row `relay_id` is revoked.
fn relay_revoked(store: &Store, account_id: &str, relay_id: &str) -> bool {
    store
        .list_devices(account_id)
        .expect("list devices")
        .into_iter()
        .find(|d| d.device_id == relay_id)
        .expect("the registered row survives")
        .revoked
}

/// Poll until `done` holds, or fail with `what`.
async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(POLL).await;
    }
}

/// Whether `watcher` has a vault row for every one of `devices`.
async fn knows_devices(watcher: &Core, devices: &[[u8; 16]]) -> bool {
    match watcher.query(Query::DeviceList).await.expect("device list") {
        QueryResult::Devices(rows) => devices
            .iter()
            .all(|id| rows.iter().any(|d| d.device_id == *id)),
        other => panic!("expected Devices, got {other:?}"),
    }
}

/// A revokes B, the relay refuses B, A withdraws, A's driver reinstates B at
/// the relay, and B's signed requests are accepted again. C, which did not
/// revoke B, cannot reinstate it.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_revocation_reinstates_the_device_at_the_relay_and_only_its_revoker_can() {
    let (addr, relay, held) = spawn_bound_relay().await;
    let store = held.as_sqlite().expect("the default store is SQLite");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let account = store
        .resolve_account(&subject(), true, clock.now_ms())
        .expect("the account the bearer maps to");

    let dirs: Vec<_> = (0..3)
        .map(|_| tempfile::tempdir().expect("temp dir"))
        .collect();
    let a = open_core_offline(dirs[0].path(), ROOT, addr, Arc::clone(&clock)).await;
    let _a_relay = register_and_start(store, &account.account_id, &a, addr, "a");
    let b = open_paired_core_offline(dirs[1].path(), &a, addr, Arc::clone(&clock)).await;
    let b_relay = register_and_start(store, &account.account_id, &b, addr, "b");
    let c = open_paired_core_offline(dirs[2].path(), &a, addr, Arc::clone(&clock)).await;
    let c_relay = register_and_start(store, &account.account_id, &c, addr, "c");
    for core in [&a, &b, &c] {
        wait_live(core, TIMEOUT).await;
    }

    // The withdrawal is refused until every device advertises it, and B
    // cannot advertise anything once the relay refuses it, so A must have
    // both siblings' certs and features before the revocation.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let features = [REVOKE_WITHDRAW_FEATURE, REVOKE_WITHDRAW_AUTHORITY_FEATURE]
            .iter()
            .all(|f| a.devices_lacking_feature(f).is_ok_and(|l| l.is_empty()));
        if features && knows_devices(&a, &[b.device_id(), c.device_id()]).await {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "A never learned both siblings and their features"
        );
        tokio::time::sleep(POLL).await;
    }

    let b_ref = EntityRef::new(EntityKind::Device, b.device_id());
    a.submit(Command::RevokeDevice {
        device_id: b_ref,
        reason: RevokeReason::Lost,
    })
    .await
    .expect("revoke B");
    wait_until("the relay never learned of the revocation", || {
        relay_revoked(store, &account.account_id, &b_relay)
    })
    .await;
    assert!(
        handshake(&mut signed_transport(addr, &b, &b_relay))
            .await
            .is_err(),
        "the relay refuses a revoked device"
    );

    // C is an active device of the account, and signs, but it is not the
    // revoker the relay recorded.
    let refused = signed_transport(addr, &c, &c_relay)
        .reinstate_device(b.device_id())
        .await
        .expect("the relay answers C");
    assert_eq!(refused, ReinstateOutcome::NotRevoker);
    assert!(
        relay_revoked(store, &account.account_id, &b_relay),
        "C's request changes no row"
    );

    // The relay accepts a reinstatement only when its signed `Date`, which
    // has one-second resolution, is later than the revoke's. A real user is
    // never this quick; the test is, so it lets the second turn.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let outcome = a
        .submit(Command::WithdrawRevocation { device_id: b_ref })
        .await
        .expect("withdraw B's revocation");
    let w = outcome.withdrawal.expect("a withdrawal reports");
    assert!(
        !w.relay_intent_cancelled,
        "the relay had been told, so there was nothing unsent to cancel"
    );

    wait_until("A's driver never reinstated B at the relay", || {
        !relay_revoked(store, &account.account_id, &b_relay)
    })
    .await;
    wait_until("the reinstatement never cleared from A's queue", || {
        a.relay_reinstatement_pending(&b.device_id())
            .is_ok_and(|r| r == RelayReinstatement::Clear)
    })
    .await;
    handshake(&mut signed_transport(addr, &b, &b_relay))
        .await
        .expect("the relay accepts B again");

    relay.abort();
}
