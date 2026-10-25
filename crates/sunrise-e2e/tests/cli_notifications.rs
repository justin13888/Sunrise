//! Security notifications between CLI vaults on one account, through a real
//! relay and the real `sunrise` binary (`docs/08-features/notifications.md`
//! #13–#14, issue #347).
//!
//! A device joining is news to every device that was there before it, and
//! to none that joined after; a revocation is news to every device that
//! remains. Each is shown once per device: a second plan prints nothing,
//! because the ledger beside each vault remembers what it printed. A device
//! that plans for the first time adopts what its vault already holds.

use sunrise_e2e::cli::{
    pair_over_relay, recovery_words, settle, sign_in, Device, SELF_HOST_BEARER,
};
use sunrise_e2e::spawn_relay;

/// The alerts `device` would post now, as `(kind, device id)`.
async fn alerts(device: &Device) -> Vec<(String, String)> {
    device.json(&["reminders", "plan"]).await["alerts"]
        .as_array()
        .expect("alerts is a list")
        .iter()
        .map(|a| {
            (
                a["kind"].as_str().unwrap_or_default().to_owned(),
                a["device"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

async fn device_id(device: &Device) -> String {
    let rows = device.json(&["devices"]).await;
    rows.as_array()
        .and_then(|rows| rows.iter().find(|r| r["this_device"] == true))
        .and_then(|r| r["id"].as_str())
        .expect("this device is listed")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_and_a_revocation_are_shown_once_on_every_device_they_concern() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();

    let a = Device::new(&dir.path().join("a"), &url);
    sign_in(&a.vault);
    let words = recovery_words(&a.ok(&["bootstrap", "a@example.com"]).await);
    a.sync().await;
    assert!(alerts(&a).await.is_empty(), "a new account has no news");

    // B joins from the recovery code: news on A, and nothing on B.
    let mut b = Device::new(&dir.path().join("b"), &url);
    b.bearer = Some(SELF_HOST_BEARER.to_owned());
    let mut args = vec!["recover"];
    args.extend(words.iter().map(String::as_str));
    b.ok(&args).await;
    b.sync().await;
    assert!(alerts(&b).await.is_empty(), "A was there first");
    settle(&a, &b).await;
    let b_id = device_id(&b).await;
    let on_a = alerts(&a).await;
    assert_eq!(on_a.len(), 1, "{on_a:?}");
    assert_eq!(on_a[0].0, "device_paired");
    assert_eq!(on_a[0].1, b_id);
    assert!(alerts(&a).await.is_empty(), "shown once");

    // C pairs onto A: news on both A and B.
    let mut c = Device::new(&dir.path().join("c"), &url);
    c.bearer = Some(SELF_HOST_BEARER.to_owned());
    pair_over_relay(&a, &c).await;
    c.sync().await;
    settle(&a, &b).await;
    let c_id = device_id(&c).await;
    for device in [&a, &b] {
        let got = alerts(device).await;
        assert_eq!(
            got.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["device_paired"],
            "{got:?}"
        );
        assert_eq!(got[0].1, c_id);
    }

    // A revokes C: news on A and B, once each.
    a.ok(&["device", "revoke", &c_id[..12], "--reason", "lost"])
        .await;
    settle(&a, &b).await;
    for device in [&a, &b] {
        let got = alerts(device).await;
        assert_eq!(
            got.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["device_revoked"],
            "{got:?}"
        );
        assert_eq!(got[0].1, c_id);
        assert!(alerts(device).await.is_empty(), "shown once");
    }

    // The catalog's other kinds are planned beside them.
    let plan = a.json(&["reminders", "plan"]).await;
    assert!(plan["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["kind"] == "evening_plan" || n["kind"] == "morning_brief"));
    relay.abort();
}
