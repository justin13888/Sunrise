//! The client half of ADR-0022's device binding, against a relay that demands
//! it (issue #159).
//!
//! `require_device_sig = true` is the configuration nothing in this workspace
//! had ever run end to end, and it is the one that would have caught both
//! halves of #159: the CLI registered a 16-byte device *id* where a 32-byte
//! Ed25519 public key belongs, so `POST /api/v1/devices` answered `400`; and no
//! client signed anything, so every signed route answered `401`. Neither showed
//! up, because `ServerConfig`'s default leaves the flag off and the self-host
//! `NullVerifier` maps every caller to one account, so a local relay never
//! asks.
//!
//! So this relay asks. It runs a real verifier — `ServerConfig::validate`
//! refuses the flag alongside the single-tenant verifier, on the grounds that a
//! device signature binds nothing when every caller is one account — and every
//! request these cores make carries `X-Sunrise-Device`,
//! `X-Sunrise-Device-Sig` and `Date`, produced by the vault's own signing key
//! through `Core::device_signer`.
//!
//! # What each test is guarding
//!
//! Convergence proves the *positive* path over all four signed sync operations
//! plus the stream: `POST /sync/session`, `POST /sync/subscribe`,
//! `GET /sync/events` and `POST /sync/ops`. The revocation test proves
//! `DELETE /api/v1/devices/by-vault-id/{id}` — the request whose entire purpose
//! is to work when a device has been lost, and which #158 shipped into a
//! configuration where it could only 401.
//!
//! The two refusal tests are what stop the others passing vacuously. If the
//! flag were quietly off, or the relay accepted an absent binding,
//! `an_unbound_client_is_refused_where_a_bound_one_is_accepted` would go green
//! against a relay that was checking nothing.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_core::{Clock, Command, Core, Query, QueryResult, RevokeReason, SystemClock};
use sunrise_domain::TaskDraft;
use sunrise_e2e::{
    open_core_offline, open_paired_core_offline, signed_ws_factory, spawn_relay_with, wait_live,
    wait_tasks_converge,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_server::store::NewDevice;
use sunrise_server::{ServerConfig, StaticVerifier, Store, Subject};
use sunrise_sync::{SseTransport, Transport, TransportError};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};

const ISSUER: &str = "https://idp.example";
const BEARER: &str = "alice-token";
const ROOT: [u8; 32] = [0x5b; 32];
const TIMEOUT: Duration = Duration::from_secs(30);

fn subject() -> Subject {
    Subject::new(ISSUER, "alice")
}

/// A relay that requires a device binding and can tell devices apart.
///
/// Both halves are load-bearing. `require_device_sig` alone over the self-host
/// verifier is the configuration `ServerConfig::validate` refuses, and running
/// it anyway would test a relay no deployment can be.
async fn spawn_bound_relay() -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<Store>) {
    let mut captured: Option<Arc<Store>> = None;
    let (addr, handle) = spawn_relay_with(
        ServerConfig {
            require_device_sig: true,
            ..ServerConfig::default()
        },
        |state| {
            captured = Some(state.store.clone());
            state.with_verifier(Arc::new(StaticVerifier::default().with(BEARER, subject())))
        },
    )
    .await;
    (
        addr,
        handle,
        captured.expect("the harness hands back the relay's own store"),
    )
}

/// Register `core`'s device at the relay and return the id the relay minted.
///
/// Written through the store rather than over HTTP for the reason
/// `relay_learns_revocation.rs` gives — this crate has no REST client, and
/// `POST /api/v1/devices` has its own tests in `sunrise-server` — but it
/// registers exactly what `sunrise_relay_client::bootstrap` now sends: the
/// vault's real `D_S_pub` in its wire form, and the vault id a peer would
/// revoke it by.
fn register(store: &Store, account_id: &str, core: &Core, nickname: &str, now_ms: u64) -> String {
    store
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
            now_ms,
        )
        .expect("register the device")
        .device_id
}

/// Open a core, register it, and start its sync driver bound to that row.
///
/// The order is the one a real client runs in: `bootstrap` registers before any
/// signed request, because the relay's id for a device does not exist until it
/// has one.
async fn bound_core(
    dir: &std::path::Path,
    root: [u8; 32],
    addr: SocketAddr,
    store: &Store,
    account_id: &str,
    clock: &Arc<dyn Clock>,
    nickname: &str,
) -> Arc<Core> {
    let core = open_core_offline(dir, root, addr, Arc::clone(clock)).await;
    let device_id = register(store, account_id, &core, nickname, clock.now_ms());
    core.start_sync(signed_ws_factory(
        addr,
        Some(BEARER.to_owned()),
        core.device_signer(device_id),
    ))
    .expect("start sync");
    core
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

/// Wait until `watcher` has a vault row for `subject`'s device.
///
/// A revocation names a device the *vault* knows, and a vault learns about a
/// sibling from its `device_cert` op — which travels the relay like any other.
/// So the revocation cannot be submitted until that has arrived.
async fn wait_device_known(watcher: &Core, subject: &Core, timeout: Duration) {
    let wanted = subject.device_id();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let seen = match watcher.query(Query::DeviceList).await.expect("device list") {
            QueryResult::Devices(rows) => rows.iter().any(|d| d.device_id == wanted),
            other => panic!("expected Devices, got {other:?}"),
        };
        if seen {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the peer's device never reached this vault"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Drive one `Hello` over a transport and report what came back.
async fn handshake(t: &mut SseTransport) -> Result<(), TransportError> {
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&hello(), &mut payload).unwrap();
    t.send_frame(encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &payload).unwrap())
        .await?;
    let buf = t.recv_frame().await?.expect("a HelloAck, not a close");
    assert_eq!(decode_frame(&buf).unwrap().0.msg_kind, MsgKind::HelloAck);
    Ok(())
}

/// Two device-bound replicas converge through a relay that requires the
/// binding.
///
/// Every request this makes is signed: session establishment, the subscribe,
/// the event stream, and the op batch. Before #159 not one of them was, so this
/// scenario answered `401` at the first step.
#[tokio::test(flavor = "multi_thread")]
async fn two_bound_devices_converge_through_a_relay_that_requires_the_binding() {
    let (addr, relay, store) = spawn_bound_relay().await;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let account = store
        .resolve_account(&subject(), true, clock.now_ms())
        .expect("the account the bearer maps to");

    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = bound_core(
        dir_a.path(),
        ROOT,
        addr,
        &store,
        &account.account_id,
        &clock,
        "a",
    )
    .await;

    // B joins A's account, then registers its *own* key: two rows, two
    // signatures, one account. A single shared key would make the binding
    // untestable — the relay could not tell which device sent a request.
    let b = open_paired_core_offline(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    let b_device = register(&store, &account.account_id, &b, "b", clock.now_ms());
    b.start_sync(signed_ws_factory(
        addr,
        Some(BEARER.to_owned()),
        b.device_signer(b_device),
    ))
    .expect("start sync");

    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    a.submit(Command::CreateTask(TaskDraft {
        title: "signed all the way down".into(),
        ..Default::default()
    }))
    .await
    .expect("create task");

    let converged = wait_tasks_converge(&a, &b, 1, TIMEOUT).await;
    assert_eq!(converged[0].title, "signed all the way down");

    relay.abort();
}

/// The guard on the test above: the same relay refuses a client that sends no
/// binding, and says so in a way that names what is missing.
///
/// Without this, a relay that had quietly stopped checking would let the
/// convergence test pass while proving nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_unbound_client_is_refused_where_a_bound_one_is_accepted() {
    let (addr, relay, _store) = spawn_bound_relay().await;
    let mut t = SseTransport::connect_with_bearer(&format!("http://{addr}"), Some(BEARER));

    match handshake(&mut t).await {
        Err(TransportError::Server { code, message }) => {
            assert_eq!(
                code, "AUTH_DEVICE_SIG_INVALID",
                "the relay must name the signature, not the bearer — the bearer is fine"
            );
            assert!(
                message.contains("sent no device binding"),
                "a client that signed nothing must be told that, not asked to log in again: \
                 {message}"
            );
        }
        other => panic!("an unbound client must be refused, got {other:?}"),
    }

    relay.abort();
}

/// A clock outside the replay window is refused, and the refusal says so.
///
/// This is the one failure a user can actually act on, and until now it arrived
/// as a bare `401` that sent them to `sunrise login` — which cures nothing,
/// forever. The skew is measured against the `Date` the relay put on the very
/// response that refused the request, so it is arithmetic rather than a guess.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_whose_clock_is_wrong_is_told_it_is_its_clock() {
    /// `SystemClock`, moved. The core signs its `Date` from whatever clock it
    /// was given, which is what makes this reachable from a test at all.
    #[derive(Debug)]
    struct SkewedClock(i64);

    impl Clock for SkewedClock {
        fn now_ms(&self) -> u64 {
            SystemClock.now_ms().saturating_add_signed(self.0)
        }
    }

    let (addr, relay, store) = spawn_bound_relay().await;
    let clock: Arc<dyn Clock> = Arc::new(SkewedClock(
        (sunrise_http_sig::MAX_CLOCK_SKEW_SECS + 100) * 1000,
    ));
    let account = store
        .resolve_account(&subject(), true, SystemClock.now_ms())
        .expect("the account the bearer maps to");

    let dir = tempfile::tempdir().expect("temp dir");
    let core = open_core_offline(dir.path(), ROOT, addr, Arc::clone(&clock)).await;
    let device_id = register(
        &store,
        &account.account_id,
        &core,
        "skewed",
        SystemClock.now_ms(),
    );

    // Driven directly rather than through the driver: the driver's job is to
    // back off and retry forever, which is the right behaviour and the wrong
    // place to read one refusal out of.
    let mut t = SseTransport::connect_with_bearer(&format!("http://{addr}"), Some(BEARER))
        .with_device_signer(core.device_signer(device_id));

    match handshake(&mut t).await {
        Err(TransportError::Server { code, message }) => {
            assert_eq!(code, "AUTH_DEVICE_SIG_INVALID");
            assert!(
                message.contains("clock is") && message.contains("set the system clock"),
                "a skewed clock must be named as the cause: {message}"
            );
        }
        other => panic!("a skewed clock must be refused, got {other:?}"),
    }

    relay.abort();
}

/// The revocation `DELETE`, under the flag that made it a `401`.
///
/// `DELETE /api/v1/devices/by-vault-id/{id}` is a `SignedParts` route, so under
/// `require_device_sig` an unsigned client cannot revoke anything — and this is
/// the one request whose entire purpose is to work when a device has been lost.
/// `relay_learns_revocation.rs` covers the same fact against a relay with the
/// flag off, and says in its own header that nothing in the harness was
/// device-bound. This is that gap.
#[tokio::test(flavor = "multi_thread")]
async fn a_bound_client_can_revoke_a_device_at_a_relay_that_requires_the_binding() {
    let (addr, relay, store) = spawn_bound_relay().await;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let account = store
        .resolve_account(&subject(), true, clock.now_ms())
        .expect("the account the bearer maps to");

    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = bound_core(
        dir_a.path(),
        ROOT,
        addr,
        &store,
        &account.account_id,
        &clock,
        "a",
    )
    .await;

    // B has to be live too, and not only registered: A can revoke a device it
    // has a vault row for, and B's `device_cert` op is how A learns it exists.
    let b = open_paired_core_offline(dir_b.path(), &a, addr, Arc::clone(&clock)).await;
    let b_relay_id = register(&store, &account.account_id, &b, "b", clock.now_ms());
    b.start_sync(signed_ws_factory(
        addr,
        Some(BEARER.to_owned()),
        b.device_signer(b_relay_id.clone()),
    ))
    .expect("start sync");
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    wait_device_known(&a, &b, TIMEOUT).await;

    a.submit(Command::RevokeDevice {
        device_id: EntityRef::new(EntityKind::Device, b.device_id()),
        reason: RevokeReason::Lost,
    })
    .await
    .expect("revoke B");

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let revoked = store
            .list_devices(&account.account_id)
            .expect("list devices")
            .into_iter()
            .find(|d| d.device_id == b_relay_id)
            .expect("the registered row survives revocation")
            .revoked;
        if revoked {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a signed revocation never reached the relay"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    relay.abort();
}
