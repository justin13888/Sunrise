//! The triage queue between two CLI vaults on one account, through a real
//! relay and the real `sunrise` binary (ADR-0047 §3–§4, issue #335).
//!
//! Two late tasks are in the queue on both devices. A batch naming one task
//! that is gone is refused whole and changes nothing; a drop with a reason
//! reaches the other device; a Keep takes a task out of the queue on both.

use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, Device};
use sunrise_e2e::spawn_relay;

/// The ids in `device`'s triage queue, in its order.
async fn queue(device: &Device) -> Vec<String> {
    device
        .json(&["triage"])
        .await
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_triage_batch_is_all_or_nothing_and_a_drop_reason_syncs() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;

    let late = first_id(&a.ok(&["capture", "Renew passport"]).await);
    let later = first_id(&a.ok(&["capture", "File taxes"]).await);
    let gone = first_id(&a.ok(&["capture", "Old idea"]).await);
    a.ok(&["edit", &late, "due:2020-01-01"]).await;
    a.ok(&["edit", &later, "due:2020-06-01"]).await;
    a.ok(&["drop", &gone]).await;
    settle(&a, &b).await;
    assert_eq!(queue(&a).await, vec![late.clone(), later.clone()]);
    assert_eq!(
        queue(&b).await,
        queue(&a).await,
        "one queue on both devices"
    );
    let row = &a.json(&["triage"]).await[0];
    assert_eq!(row["lateness"]["state"], "late_hard");

    // One id that is gone refuses the batch, and nothing is written.
    let refused = a
        .run(&["triage", "drop", &late, &gone, "--reason", "not needed"])
        .await;
    assert!(!refused.status.success(), "a deleted id refuses the batch");
    assert_eq!(a.task(&late).await["state"], "todo");
    assert_eq!(queue(&a).await, vec![late.clone(), later.clone()]);

    // A drop with a reason reaches the other device.
    a.ok(&["triage", "drop", &late, "--reason", "moved abroad"])
        .await;
    settle(&a, &b).await;
    let on_b = b.task(&late).await;
    assert_eq!(on_b["state"], "cancelled");
    assert_eq!(on_b["cancel_reason"], "moved abroad");

    // A Keep on B takes the other out of the queue on both.
    b.ok(&["triage", "keep", &later]).await;
    settle(&a, &b).await;
    assert!(queue(&a).await.is_empty());
    assert!(queue(&b).await.is_empty());
    assert!(a.task(&later).await["late_acknowledged_at_ms"].is_i64());

    // Already done, but never before the task existed; on its own day, as
    // a date, from A.
    let early = a
        .run(&["triage", "done", &later, "--on", "2024-01-02"])
        .await;
    assert!(
        !early.status.success(),
        "a completion before the task existed"
    );
    a.ok(&["triage", "done", &later, "--on", "today"]).await;
    settle(&a, &b).await;
    let done = b.task(&later).await;
    assert_eq!(done["state"], "done");
    assert_eq!(done["completed_at"]["kind"], "all_day");
    relay.abort();
}
