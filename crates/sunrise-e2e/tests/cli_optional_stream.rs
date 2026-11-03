//! Optional streams through the real `sunrise` binary (ADR-0046, issue #332):
//! between two CLI vaults on one account over a real relay, and on one vault.
//!
//! A task that belongs to no stream syncs like any other, sealed under the
//! account's private key domain; and on one device, `streams delete Old
//! --move-to New` re-homes a task filed with `#old` and retitled, while
//! `streams delete New` detaches it. How a deletion converges against an
//! edit made apart is proved in `sunrise-core`'s
//! `src/engine/tests/optional_stream.rs` and in `delete_defect_probe.rs`.

use serde_json::Value;
use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, Device};
use sunrise_e2e::spawn_relay;

/// Two devices on one account.
async fn two_devices(root: &std::path::Path, relay: &str) -> (Device, Device) {
    bootstrap_and_recover(root, relay).await
}

/// The ids of `device`'s listing `args` prints as JSON.
async fn listed(device: &Device, args: &[&str]) -> Vec<String> {
    device
        .json(args)
        .await
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The id `sunrise streams --json` lists for the stream called `name`.
async fn stream_id(device: &Device, name: &str) -> Option<String> {
    device.json(&["streams"]).await.as_array().and_then(|ss| {
        ss.iter()
            .find(|s| s["name"] == name)
            .and_then(|s| s["id"].as_str().map(str::to_owned))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_with_no_stream_syncs_between_two_vaults() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = two_devices(dir.path(), &format!("http://{addr}")).await;

    let task = first_id(&a.ok(&["capture", "Renew passport"]).await);
    settle(&a, &b).await;

    let on_b = b.task(&task).await;
    assert_eq!(on_b["title"], "Renew passport");
    assert_eq!(on_b["stream"], Value::Null, "no stream, on both devices");
    assert_eq!(listed(&b, &["inbox"]).await, vec![task.clone()]);
    assert!(
        b.json(&["streams"])
            .await
            .as_array()
            .is_some_and(Vec::is_empty),
        "the private domain is not a stream"
    );

    // Triaged on B, it leaves the Inbox on A and stays a task of no stream.
    b.ok(&["triage", &task]).await;
    settle(&a, &b).await;
    assert!(listed(&a, &["inbox"]).await.is_empty());
    assert_eq!(listed(&a, &["stream", "none"]).await, vec![task.clone()]);

    // Filed into a stream on A and back out of it on B, it ends with none.
    a.ok(&["streams", "create", "Errands"]).await;
    a.ok(&["edit", &task, "#errands"]).await;
    settle(&a, &b).await;
    let errands = stream_id(&b, "Errands").await.expect("the stream synced");
    assert_eq!(b.task(&task).await["stream"], errands.as_str());
    b.ok(&["edit", &task, "#inbox"]).await;
    settle(&a, &b).await;
    for device in [&a, &b] {
        assert_eq!(device.task(&task).await["stream"], Value::Null);
    }
    relay.abort();
}

/// On one device with no relay: a task filed with `#old` and retitled is
/// re-homed by `streams delete Old --move-to New`, keeping its title, and
/// `streams delete New` then leaves it in no stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_stream_moves_its_task_to_another_or_to_no_stream() {
    let dir = tempfile::tempdir().unwrap();
    let a = Device {
        relay: None,
        ..Device::new(&dir.path().join("a"), "")
    };

    a.ok(&["streams", "create", "Old"]).await;
    a.ok(&["streams", "create", "New"]).await;
    let task = first_id(&a.ok(&["capture", "Pack the boxes #old"]).await);
    let old = stream_id(&a, "Old").await.expect("created");
    let new = stream_id(&a, "New").await.expect("created");
    assert_eq!(
        a.task(&task).await["stream"],
        old.as_str(),
        "`#old` filed it"
    );
    a.ok(&["retitle", &task, "Pack the boxes and label them"])
        .await;

    a.ok(&["streams", "delete", "Old", "--move-to", "New"])
        .await;
    let moved = a.task(&task).await;
    assert_eq!(moved["stream"], new.as_str(), "re-homed into New");
    assert_eq!(moved["title"], "Pack the boxes and label them");
    assert_eq!(stream_id(&a, "Old").await, None, "the stream is gone");
    assert_eq!(listed(&a, &["stream", "New"]).await, vec![task.clone()]);

    // Deleting with no `--move-to` detaches it instead.
    a.ok(&["streams", "delete", "New"]).await;
    assert_eq!(a.task(&task).await["stream"], Value::Null);
    assert_eq!(listed(&a, &["stream", "none"]).await, vec![task]);
}
