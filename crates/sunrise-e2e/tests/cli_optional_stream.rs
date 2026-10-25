//! Optional streams between two CLI vaults on one account, through a real
//! relay and the real `sunrise` binary (ADR-0046, issue #332).
//!
//! A task that belongs to no stream syncs like any other, sealed under the
//! account's private key domain; and a stream deleted on one device while the
//! other edits one of its tasks, and files a new one into it, converges on
//! both devices to the outcome the deletion chose.

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

/// The convergence the issue asks for: A deletes a stream into another while
/// B, apart, retitles one of its tasks and files a new one into it. Both
/// devices end with both tasks in the stream A chose, B's title kept, and the
/// deleted stream gone from every listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_stream_while_the_other_device_edits_one_of_its_tasks_converges() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = two_devices(dir.path(), &format!("http://{addr}")).await;

    a.ok(&["streams", "create", "Old"]).await;
    a.ok(&["streams", "create", "New"]).await;
    let kept = first_id(&a.ok(&["capture", "Pack the boxes #old"]).await);
    settle(&a, &b).await;
    let new = stream_id(&a, "New").await.expect("created");

    // Apart.
    a.ok(&["streams", "delete", "Old", "--move-to", "New"])
        .await;
    b.ok(&["retitle", &kept, "Pack the boxes and label them"])
        .await;
    let late = first_id(&b.ok(&["capture", "Book the van #old"]).await);
    settle(&a, &b).await;

    for device in [&a, &b] {
        for task in [&kept, &late] {
            assert_eq!(
                device.task(task).await["stream"],
                new.as_str(),
                "{task} is where the deletion re-homed the stream"
            );
        }
        assert_eq!(
            device.task(&kept).await["title"],
            "Pack the boxes and label them",
            "the edit made apart survives"
        );
        assert_eq!(stream_id(device, "Old").await, None, "the stream is gone");
        let mut in_new = listed(device, &["stream", "New"]).await;
        in_new.sort();
        let mut want = vec![kept.clone(), late.clone()];
        want.sort();
        assert_eq!(in_new, want);
    }

    // Detaching sends everything to no stream instead.
    a.ok(&["streams", "delete", "New"]).await;
    settle(&a, &b).await;
    for device in [&a, &b] {
        for task in [&kept, &late] {
            assert_eq!(device.task(task).await["stream"], Value::Null);
        }
    }
    relay.abort();
}
