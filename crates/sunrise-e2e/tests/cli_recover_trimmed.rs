//! `sunrise recover` from the 24 words on an account whose relay has trimmed
//! every vault-meta frame that carried a key (issue #580, ADR-0069), with the
//! real `sunrise` binary against a real relay whose durable log keeps thirty
//! seconds of each channel.
//!
//! The account's keys have rotated past the genesis, which is what makes the
//! trim bite: the vault-meta stream's genesis key is derived from the identity
//! and a recovery recomputes it, but every later key reached a recovering
//! device only as an identity-addressed `key_envelope` op, and those ops are
//! gone. The vault-meta snapshot record cannot stand in, because it is sealed
//! under the rotated key it would deliver.
//!
//! 1. A creates the account (`sunrise bootstrap`, which prints the code) and
//!    writes its history.
//! 2. C pairs over the relay and syncs, then revokes A (A is lost). The
//!    revocation rotates every stream, and C's next `sync --once` pushes the
//!    new epochs, pins snapshot records sealed under them as the streams'
//!    compactor, and pins the account's key bundle.
//! 3. C writes once more, past the retention window, so the relay keeps only
//!    that last write in each channel.
//! 4. B recovers from A's words, with nothing but the identity. Its sync is
//!    told every stream has a gap; the vault-meta record needs a key B lacks,
//!    so B fetches the key bundle, opens it with the restored `ID_D_priv`,
//!    applies the records and ends `live` holding every task.
//!
//! Before the bundle, step 4 ended `Degraded` holding nothing.

#![allow(clippy::missing_panics_doc)]

use std::time::Duration;

use sunrise_e2e::cli::{pair_over_relay, recovery_words, sign_in, Device, SELF_HOST_BEARER};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::relay_log::{account_key, DurableCaps};
use sunrise_server::ServerConfig;

/// Tasks A writes before C joins.
const HISTORY: usize = 12;

/// The relay's op retention for this test. A channel's append evicts every
/// frame older than this, so a write after a wait this long leaves that write
/// alone in its channel. Long enough that the frames written after it
/// (C's last write, then B's own ops while it recovers) do not evict each
/// other: by size, B's first append would evict the frame above the records,
/// which is a gap no record covers and not the case this test is about.
const RETENTION: Duration = Duration::from_secs(30);

/// How many paired devices to try for a C that is not the streams' compactor
/// before it revokes A. The compactor is the lowest device id among the known
/// devices (`Engine::is_stream_compactor`) and writes its snapshot record at
/// most once a day, so a C below A would write its one record before the
/// rotation, and the record the trimmed relay must serve would predate the
/// keys the test is about. Device ids are random, so each try succeeds with
/// probability one half.
const PAIRING_TRIES: usize = 12;

/// This device's id, as `sunrise --json devices` prints it.
async fn device_id(device: &Device) -> String {
    let rows = device.json(&["devices"]).await;
    rows.as_array()
        .and_then(|rows| rows.iter().find(|r| r["this_device"] == true))
        .and_then(|r| r["id"].as_str())
        .expect("this device is listed")
        .to_owned()
}

/// The sync and key events `device` logged, for a failure message.
fn sync_log(device: &Device) -> String {
    std::fs::read_to_string(device.vault.with_extension("log.ndjson"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\"ev\":\"sync.") || l.contains("\"ev\":\"core.key"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_from_the_code_reads_every_task_after_the_relay_trimmed_the_keys() {
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

    // 1. The account, and its history.
    let a = Device::new(&dir.path().join("a"), &url);
    sign_in(&a.vault);
    let words = recovery_words(&a.ok(&["bootstrap", "a@example.com"]).await);
    for i in 0..HISTORY {
        a.ok(&["capture", &format!("history {i}")]).await;
    }
    a.sync().await;
    let a_id = device_id(&a).await;

    // 2. C joins, and revokes A.
    let mut c = None;
    for attempt in 0..PAIRING_TRIES {
        let mut candidate = Device::new(&dir.path().join(format!("c{attempt}")), &url);
        candidate.bearer = Some(SELF_HOST_BEARER.to_owned());
        pair_over_relay(&a, &candidate).await;
        if device_id(&candidate).await > a_id {
            c = Some(candidate);
            break;
        }
    }
    let c = c.expect("a paired device with an id above A's");
    c.sync().await;
    assert_eq!(
        c.json(&["inbox"]).await.as_array().map(Vec::len),
        Some(HISTORY),
        "C caught up on A's history before revoking it"
    );
    c.ok(&["device", "revoke", &a_id[..12], "--reason", "lost"])
        .await;
    c.ok(&["capture", "after the revocation"]).await;
    c.sync().await;

    // 3. The last write, past the retention window, which pushes out every
    // frame before it.
    tokio::time::sleep(RETENTION + Duration::from_secs(1)).await;
    c.ok(&["contexts", "create", "after"]).await;
    c.ok(&["capture", "the last write"]).await;
    c.sync().await;

    let accounts = store.account_summaries().await.unwrap();
    let [account] = accounts.as_slice() else {
        panic!("one account on the relay: {accounts:?}");
    };
    let meta = (account_key(&account.account_id), [0u8; 16]);
    assert_eq!(
        store.relay_len(meta).await.unwrap(),
        1,
        "the relay holds only C's last vault-meta frame: every key envelope is gone"
    );
    assert!(
        metrics.get("sunrise_key_bundle_pin_total") >= 2,
        "A pinned the account's key bundle, and C replaced it after the rotation"
    );

    // 4. Recovery from the words alone.
    let mut b = Device::new(&dir.path().join("b"), &url);
    b.bearer = Some(SELF_HOST_BEARER.to_owned());
    let mut args = vec!["recover"];
    args.extend(words.iter().map(String::as_str));
    let out = b.run(&args).await;
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
        Some(HISTORY + 2),
        "B reads every task the account holds: {inbox}"
    );
    assert!(
        metrics.get("sunrise_relay_cursor_gap_total") > 0,
        "the relay could not replay the account's history to B"
    );
    let log = std::fs::read_to_string(b.vault.with_extension("log.ndjson")).unwrap_or_default();
    assert!(
        log.contains("\"sync.key_bundle.recovered\""),
        "B took its keys from the account's key bundle"
    );
    relay.abort();
}
