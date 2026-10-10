//! A device files its APNs token with the relay, signed as itself, through the
//! seam the Swift app drives — and a rotated token replaces the old one.
//!
//! `POST /api/v1/devices/push-tokens` had no caller anywhere in the workspace
//! before #367, so the relay's push dispatcher
//! (`docs/06-server/push-notifications.md`) had nothing to wake. This runs
//! against a relay that **requires** the ADR-0022 device binding, because the
//! route takes a signed body: a test against the self-host verifier would pass
//! with no signature at all.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;

use sunrise_core_bindings::{BindingError, SunriseCore};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::{ServerConfig, StaticVerifier, Store, Subject};

const ISSUER: &str = "https://idp.example";
const BEARER: &str = "alice-token";

/// A relay that demands a device binding, and a handle on its store.
async fn bound_relay() -> (String, tokio::task::JoinHandle<()>, Arc<Store>) {
    let config = ServerConfig {
        oidc_issuer: Some(ISSUER.to_owned()),
        oidc_client_id: Some("sunrise".to_owned()),
        ..ServerConfig::default()
    };
    assert!(config.device_sig_required());
    let mut captured: Option<Arc<Store>> = None;
    let (addr, handle) = spawn_relay_with(config, |state| {
        captured = Some(state.store.clone());
        state.with_verifier(Arc::new(
            StaticVerifier::default().with(BEARER, Subject::new(ISSUER, "alice")),
        ))
    })
    .await;
    (
        format!("http://{addr}"),
        handle,
        captured.expect("the harness hands back the relay's own store"),
    )
}

async fn open_device(dir: &std::path::Path, root: u8) -> Arc<SunriseCore> {
    SunriseCore::open(
        dir.to_string_lossy().into_owned(),
        vec![root; 32],
        "0.1.0+e2e".into(),
        None,
    )
    .await
    .expect("the vault opens")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_files_its_push_token_and_a_rotation_replaces_it() {
    let (url, relay, store) = bound_relay().await;
    let dir = tempfile::tempdir().expect("vault dir");
    let device = open_device(dir.path(), 0x71).await;

    let registered = device
        .bootstrap_account(
            url.clone(),
            BEARER.into(),
            "alice@example.com".into(),
            "phone".into(),
            1,
        )
        .await
        .expect("the device registers");
    let relay_device_id = registered.device_id;

    device
        .register_push_token(
            url.clone(),
            BEARER.into(),
            relay_device_id.clone(),
            "a1b2c3d4".into(),
        )
        .await
        .expect("the relay files the token");
    assert_eq!(
        store.push_tokens(&relay_device_id).expect("read tokens"),
        vec![("apns".to_owned(), "a1b2c3d4".to_owned())],
        "one row, under this device and the APNs provider"
    );

    // Uploading again is harmless, and a rotated token replaces the row
    // rather than adding a second one the dispatcher would also wake.
    for token in ["a1b2c3d4", "e5f60718"] {
        device
            .register_push_token(
                url.clone(),
                BEARER.into(),
                relay_device_id.clone(),
                token.into(),
            )
            .await
            .expect("a repeat upload is accepted");
    }
    assert_eq!(
        store.push_tokens(&relay_device_id).expect("read tokens"),
        vec![("apns".to_owned(), "e5f60718".to_owned())],
        "the rotated token replaced the old one"
    );

    device.shutdown().await;
    relay.abort();
}

/// The binding is real: a token filed in another device's name, signed with
/// this device's key, is refused and files nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_signed_by_another_device_is_refused() {
    let (url, relay, store) = bound_relay().await;
    let dir_a = tempfile::tempdir().expect("vault dir");
    let dir_b = tempfile::tempdir().expect("vault dir");
    let a = open_device(dir_a.path(), 0x72).await;
    let b = open_device(dir_b.path(), 0x73).await;

    let _a_id = a
        .bootstrap_account(
            url.clone(),
            BEARER.into(),
            "alice@example.com".into(),
            "phone".into(),
            1,
        )
        .await
        .expect("a registers")
        .device_id;
    let b_id = b
        .register_relay_device(url.clone(), BEARER.into(), "tablet".into())
        .await
        .expect("b registers");

    let refused = a
        .register_push_token(url.clone(), BEARER.into(), b_id.clone(), "ffff".into())
        .await;
    assert!(
        matches!(refused, Err(BindingError::Relay(_))),
        "{refused:?}"
    );
    assert!(
        store.push_tokens(&b_id).expect("read tokens").is_empty(),
        "nothing may have been filed under b"
    );

    a.shutdown().await;
    b.shutdown().await;
    relay.abort();
}
