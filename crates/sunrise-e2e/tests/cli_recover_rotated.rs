//! `sunrise recover` on an account whose identity has rotated (issue #583,
//! ADR-0070), with the real `sunrise` binary against a real relay.
//!
//! A recovery code opens the identity it was sealed under. Before ADR-0070 a
//! code made before a rotation restored that identity and nothing more: the
//! recovered device was in no roster, never adopted the successor, and could
//! read nothing written after it. Two cases, one per way a rotation treats
//! the code:
//!
//! - **`identity rotate --keep-code`.** The old code still works. The relay
//!   trims every frame that carried the rotation or a key, so what takes the
//!   recovered device to the identity in force is the identity chain record
//!   pinned beside the recovery blob, and what gives it the keys is the key
//!   bundle sealed to that identity.
//! - **`identity rotate`.** The rotation retires the code and prints a new
//!   one. The new code recovers a current device of the same account; the old
//!   one fails with an error that says the code was retired, not that it was
//!   mistyped, and writes no vault.

#![allow(clippy::missing_panics_doc)]

use std::time::Duration;

use sunrise_e2e::cli::{recovery_words, sign_in, Device, SELF_HOST_BEARER};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::relay_log::{account_key, DurableCaps};
use sunrise_server::ServerConfig;

/// Tasks written before the rotation, and after it.
const BEFORE: usize = 6;
const AFTER: usize = 4;

/// The relay's op retention for the trimmed case; see `cli_recover_trimmed.rs`
/// for why thirty seconds.
const RETENTION: Duration = Duration::from_secs(30);

/// The identity a device's `sunrise --json identity` reports: the account and
/// the identity in force.
async fn identity(device: &Device) -> (String, String, bool) {
    let st = device.json(&["identity"]).await;
    (
        st["account"].as_str().expect("an account").to_owned(),
        st["current"]
            .as_str()
            .expect("a current identity")
            .to_owned(),
        st["this_device_is_current"].as_bool().expect("a flag"),
    )
}

fn sync_log(device: &Device) -> String {
    std::fs::read_to_string(device.vault.with_extension("log.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            l.contains("\"ev\":\"sync.")
                || l.contains("\"ev\":\"core.key")
                || l.contains("\"ev\":\"core.identity")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn recover(device: &Device, words: &[String]) -> std::process::Output {
    let mut args = vec!["recover"];
    args.extend(words.iter().map(String::as_str));
    device.run(&args).await
}

fn recovering(root: &std::path::Path, name: &str, url: &str) -> Device {
    let mut d = Device::new(&root.join(name), url);
    // The self-host relay is the one configuration exempt from the step-up.
    d.bearer = Some(SELF_HOST_BEARER.to_owned());
    d
}

/// The relay keeps exactly as many bytes of a replaced blob as the client
/// reads back as a retired check.
#[test]
fn the_relay_and_the_client_agree_on_a_retired_check() {
    assert_eq!(
        sunrise_server::store::RETIRED_CHECK_LEN,
        sunrise_crypto::RETIRED_CHECK_LEN
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_carried_across_a_rotation_recovers_every_task_after_the_relay_trimmed_it() {
    let mut metrics = None;
    let mut store = None;
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| {
        metrics = Some(s.metrics.clone());
        store = Some(s.store.clone());
        s.with_durable_caps(DurableCaps {
            max_bytes: u64::MAX,
            max_age_ms: RETENTION.as_millis().try_into().unwrap(),
        })
    })
    .await;
    let metrics = metrics.expect("the relay's registry");
    let store = store.expect("the relay's store");
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();

    // 1. The account, its history, a rotation that keeps the code, and more
    //    history, all before the first sync: so the first session's snapshot
    //    record holds every task, and is the only place the trimmed relay
    //    still has them.
    let a = Device::new(&dir.path().join("a"), &url);
    sign_in(&a.vault);
    let words = recovery_words(&a.ok(&["bootstrap", "a@example.com"]).await);
    let offline = Device {
        relay: None,
        ..a.clone()
    };
    for i in 0..BEFORE {
        offline.ok(&["capture", &format!("before {i}")]).await;
    }
    let rotated = offline.ok(&["identity", "rotate", "--keep-code"]).await;
    assert!(rotated.contains("still works"), "{rotated}");
    for i in 0..AFTER {
        offline.ok(&["capture", &format!("after {i}")]).await;
    }
    a.sync().await;
    let (account, head, _) = identity(&a).await;
    assert_ne!(account, head, "the identity moved");

    // 2. A write past the retention window, which pushes out every frame
    //    before it: the rotation's op and every key envelope are gone.
    tokio::time::sleep(RETENTION + Duration::from_secs(1)).await;
    a.ok(&["capture", "the last write"]).await;
    a.sync().await;
    let accounts = store.account_summaries().await.unwrap();
    let [summary] = accounts.as_slice() else {
        panic!("one account on the relay: {accounts:?}");
    };
    let meta = (account_key(&summary.account_id), [0u8; 16]);
    assert_eq!(
        store.relay_len(meta).await.unwrap(),
        1,
        "the relay holds only A's last vault-meta frame: the transition is gone"
    );
    assert!(metrics.get("sunrise_identity_chain_pin_total") >= 1);

    // 3. The words A was given before the rotation.
    let b = recovering(dir.path(), "b", &url);
    let out = recover(&b, &words).await;
    assert!(
        out.status.success(),
        "`sunrise recover` failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        sync_log(&b)
    );
    b.sync().await;

    let inbox = b.json(&["inbox"]).await;
    assert_eq!(
        inbox.as_array().map(Vec::len),
        Some(BEFORE + AFTER + 1),
        "B reads every task the account holds: {inbox}\n{}",
        sync_log(&b)
    );
    assert_eq!(
        identity(&b).await,
        (account, head, true),
        "B is a current device under the identity A rotated to"
    );
    let log = sync_log(&b);
    assert!(log.contains("\"core.identity.chain_seeded\""), "{log}");
    assert!(log.contains("\"core.identity.self_certified\""), "{log}");
    assert!(
        metrics.get("sunrise_relay_cursor_gap_total") > 0,
        "the relay could not replay the account's history to B"
    );
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rotation_that_retires_the_code_issues_one_that_recovers_and_the_old_one_says_why() {
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| s).await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();

    let a = Device::new(&dir.path().join("a"), &url);
    sign_in(&a.vault);
    let old_words = recovery_words(&a.ok(&["bootstrap", "a@example.com"]).await);
    for i in 0..BEFORE {
        a.ok(&["capture", &format!("before {i}")]).await;
    }
    a.sync().await;

    // The default rotation pushes itself to the relay, then shows the code
    // whose blob replaced the old one.
    let out = a.run(&["identity", "rotate"]).await;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "rotate failed:\n{stdout}\n{stderr}");
    assert!(stderr.contains("old recovery code is retired"), "{stderr}");
    let new_words = recovery_words(&stdout);
    assert_ne!(new_words, old_words);
    for i in 0..AFTER {
        a.ok(&["capture", &format!("after {i}")]).await;
    }
    a.sync().await;
    let (account, head, _) = identity(&a).await;
    assert_ne!(account, head);

    // The old words: refused as retired, before a vault is written.
    let d = recovering(dir.path(), "d", &url);
    let out = recover(&d, &old_words).await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the old code must not recover");
    assert!(
        err.contains("this recovery code was retired on"),
        "a distinct, explained error: {err}"
    );
    assert!(
        !d.vault.join("vault.db").exists(),
        "and no vault of the retired identity was written"
    );

    // A code that was never this account's still fails as a code.
    let stranger = sunrise_crypto::bip39::encode_recovery_code(&[0x77; 32]);
    let stranger: Vec<String> = stranger.reveal().split(' ').map(str::to_owned).collect();
    let f = recovering(dir.path(), "f", &url);
    let out = recover(&f, &stranger).await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        !err.contains("retired"),
        "a stranger's code is not retired: {err}"
    );

    // The new words: a current device of the same account, reading it all.
    let e = recovering(dir.path(), "e", &url);
    let out = recover(&e, &new_words).await;
    assert!(
        out.status.success(),
        "`sunrise recover` with the new code failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        sync_log(&e)
    );
    e.sync().await;
    let inbox = e.json(&["inbox"]).await;
    assert_eq!(
        inbox.as_array().map(Vec::len),
        Some(BEFORE + AFTER),
        "E reads every task the account holds: {inbox}\n{}",
        sync_log(&e)
    );
    assert_eq!(identity(&e).await, (account, head, true));
    relay.abort();
}
