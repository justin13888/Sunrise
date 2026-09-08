//! The relay's half of a revocation, end to end against a real relay.
//!
//! `Command::RevokeDevice` writes two facts in two places. In the vault it is a
//! `device_revoke` op, sealed under the vault-meta Stream key, which the relay
//! holds no key for and must not: a relay that could read it would learn which
//! of an account's devices had been revoked and when, for every account it
//! serves. At the relay it is a `revoked` flag on its own `devices` row.
//!
//! `device_revocation.rs` covers the vault half — the read bound. This covers
//! the second half, and it exists because the second half is the one that was
//! written once, merged, and reverted: PR #86 queued
//! `DELETE /api/v1/devices/{device_id}`, which names a ULID the relay mints at
//! registration and no vault ever holds for a peer, so the call could only ever
//! `404` — and the code read the `404` as success and logged that the relay had
//! been told to stop accepting a revoked device. Nothing caught it, because the
//! driver tests used a hand-written transport that answered `Ok(())` and the
//! e2e ran against a relay with no registered device at all.
//!
//! So this test registers one, and it registers it the way a real client does:
//! carrying `vault_device_id`, the 16-byte id the vault knows the device by and
//! the only name a revoking device holds. It would pass identically with the
//! feature reverted only if the assertion were on the *intent* rather than on
//! the relay's own row, which is why it asserts on the row.
//!
//! # What it does not prove
//!
//! That the relay then refuses the revoked device's uploads. It does — the SQL
//! behind `active_device` ends `AND revoked = 0`, and
//! `sunrise_server::api::devices` and `::sync` drive that through HTTP — but not
//! here, because `SseTransport` signs no request, so nothing in this harness is
//! device-bound and there is no binding for the relay to refuse. That condition
//! is `[auth] require_device_sig`, and it is stated in
//! `docs/03-crypto/key-rotation.md` §Revocation rather than hidden.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, RevokeReason, SystemClock};
use sunrise_e2e::{open_paired_core, open_synced_core, spawn_relay_with, wait_live};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_server::store::NewDevice;
use sunrise_server::{ServerConfig, Store, Subject};

const ROOT: [u8; 32] = [0x41; 32];
const TIMEOUT: Duration = Duration::from_secs(30);

/// The account every caller maps to under the self-host `NullVerifier` the
/// harness relay runs.
fn self_host_subject() -> Subject {
    Subject::new(sunrise_server::auth::SELF_HOST_ISSUER, "self-host")
}

/// Poll the relay's own device row until it reports the revocation.
async fn wait_relay_revoked(store: &Store, account_id: &str, relay_device_id: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let revoked = store
            .list_devices(account_id)
            .expect("list devices")
            .into_iter()
            .find(|d| d.device_id == relay_device_id)
            .expect("the registered row survives revocation")
            .revoked;
        assert!(
            tokio::time::Instant::now() < deadline,
            "the relay never learned about the revocation"
        );
        if revoked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Revoking B on A makes the relay stop treating B as an active device.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_device_reaches_the_relay() {
    let mut captured: Option<Arc<Store>> = None;
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |state| {
        captured = Some(state.store.clone());
        state
    })
    .await;
    let store = captured.expect("the harness hands back the relay's own store");

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // B introduces itself to the relay the way `sunrise_relay_client::bootstrap`
    // does, carrying the id its siblings know it by. Written through the store
    // rather than over HTTP because this crate has no HTTP client and the route
    // that would be exercised has its own tests in `sunrise-server`.
    let account = store
        .resolve_account(&self_host_subject(), true, clock.now_ms())
        .expect("the self-host account");
    let b_vault_id = sunrise_id::crockford::encode_bytes(&b.device_id());
    let row = store
        .register_device(
            &account.account_id,
            &NewDevice {
                device_pub_s: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some(b_vault_id),
                nickname: "b".into(),
                platform: "linux".into(),
                app_version: None,
            },
            clock.now_ms(),
        )
        .expect("register B");
    assert!(!row.revoked);

    a.submit(Command::RevokeDevice {
        device_id: EntityRef::new(EntityKind::Device, b.device_id()),
        reason: RevokeReason::Lost,
    })
    .await
    .expect("revoke B");

    wait_relay_revoked(&store, &account.account_id, &row.device_id).await;

    relay.abort();
}
