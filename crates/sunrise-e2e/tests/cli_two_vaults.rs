//! Two CLI vaults on one account, through a real relay, with the real
//! `sunrise` binary: the CLI as a complete client.
//!
//! Device A is created and published with `sunrise bootstrap`; device B joins
//! the only way the CLI joins an existing account without a second machine's
//! files today, `sunrise recover` from the 24 words A printed. Then both edit
//! while apart and sync, and the scenarios assert what converges.
//!
//! Each command is its own process, as a person at two terminals runs them, so
//! nothing here shares a `Core`, a lock or a cache between the two devices.

use serde_json::Value;
use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, sign_in, Device};
use sunrise_e2e::spawn_relay;

/// Two devices on one account, and a task A wrote before B existed.
async fn two_devices(root: &std::path::Path, relay: &str) -> (Device, Device, String) {
    let a = Device::new(&root.join("a"), relay);
    sign_in(&a.vault);
    let task = first_id(&a.ok(&["capture", "Renew passport"]).await);
    let (a, b) = bootstrap_and_recover(root, relay).await;
    (a, b, task)
}

/// The fields of a task a scenario compares across devices.
fn stored(t: &Value) -> Value {
    serde_json::json!({
        "title": t["title"],
        "priority": t["priority"],
        "scheduled_at": t["scheduled_at"]["value"],
        "kind": t["scheduled_at"]["kind"],
        "deferred_count": t["deferred_count"],
        "note": t["note"],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_recovered_device_reads_what_the_first_one_wrote() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, task) = two_devices(dir.path(), &format!("http://{addr}")).await;

    assert_eq!(b.task(&task).await["title"], "Renew passport");
    // B is a second device, not a copy of A.
    let devices = b.json(&["devices"]).await;
    assert_eq!(devices.as_array().map(Vec::len), Some(2), "{devices}");
    let _ = a;
    relay.abort();
}

/// Edits to different fields of one task, made while the devices are apart.
///
/// What holds **today**: the devices converge, on the later write's whole
/// task. This build still emits full-state entity writes; ADR-0044 §9 forbids
/// emitting a field-level `Patch` until a vault requires `core.field_ops`, and
/// the feature registry does not register it yet (`crates/sunrise-core/src/inner_op.rs`).
/// So A's priority is lost to B's later retitle.
///
/// TODO(ADR-0044 §9): once `core.field_ops` is registered, both edits survive:
/// replace the last assertion with `assert_eq!(on_a["priority"], 1)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_to_different_fields_of_one_task_converge() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, task) = two_devices(dir.path(), &format!("http://{addr}")).await;

    // Apart: A sets the priority, then B retitles and writes a note.
    a.ok(&["edit", &task, "!1"]).await;
    b.ok(&["retitle", &task, "Renew passport and visa"]).await;
    b.ok(&["note", &task, "Photos are in the drawer"]).await;
    settle(&a, &b).await;

    let on_a = a.task(&task).await;
    let on_b = b.task(&task).await;
    assert_eq!(stored(&on_a), stored(&on_b), "the two devices converged");
    assert_eq!(
        on_a["title"], "Renew passport and visa",
        "B's edit survived"
    );
    assert!(on_a["note"].as_str().is_some_and(|n| n.contains("drawer")));
    assert_eq!(
        on_a["priority"],
        Value::Null,
        "entity-level last-writer-wins today; see the TODO above"
    );
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_defers_converge_on_one_plan() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, _) = two_devices(dir.path(), &format!("http://{addr}")).await;
    let task = first_id(&a.ok(&["capture", "Call the bank ^tomorrow 9am"]).await);
    settle(&a, &b).await;

    a.ok(&["defer", &task, "+1d"]).await;
    b.ok(&["defer", &task, "+3d"]).await;
    settle(&a, &b).await;

    let on_a = a.task(&task).await;
    let on_b = b.task(&task).await;
    assert_eq!(stored(&on_a), stored(&on_b), "one plan on both devices");
    // Either defer may win the plan, and it keeps the wall clock and kind.
    assert_eq!(on_a["scheduled_at"]["kind"], "floating");
    assert!(on_a["scheduled_at"]["value"]
        .as_str()
        .is_some_and(|v| v.ends_with("T09:00:00")));
    // Both deferrals were made; the counter is a register today, so it
    // reads the winner's count rather than the sum.
    assert!(on_a["deferred_count"].as_i64().is_some_and(|n| n >= 1));
    relay.abort();
}

/// Whether `task` is in the Inbox `device` lists.
async fn live(device: &Device, task: &str) -> bool {
    device
        .json(&["inbox"])
        .await
        .as_array()
        .is_some_and(|ts| ts.iter().any(|t| t["id"] == task))
}

/// What `sunrise conflicts --json` reports, quiet fields included.
async fn conflicts(device: &Device) -> Value {
    device.json(&["conflicts", "--all"]).await
}

/// A delete on one device against an edit on the other, then a restore.
///
/// What holds **today**: the devices agree on whether the task is live, and
/// the later write wins the whole task, so B's edit, made after A's drop,
/// brings it back. Both CLIs write full-state ops: ADR-0065 D9 keeps every
/// command on them until the vault requires `core.field_ops`, which no build
/// registers yet. Two full-state ops name no parents and never surface a
/// conflict (D4), so neither device reports one. `sunrise restore` brings a
/// dropped task back on both.
///
/// TODO(ADR-0065 8/8): once commands write `Patch` with parents, the delete
/// wins and B's edit is kept as an "edited while deleted" conflict. Assert
/// `!live(&a, ..)` after the first settle, that `conflicts` names B's edit
/// under `deleted`, and that `restore` clears it. The engine already folds
/// and reports exactly that for `Patch` versions
/// (`crates/sunrise-core/src/engine/tests/conflicts.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_on_one_device_against_an_edit_on_the_other() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, task) = two_devices(dir.path(), &format!("http://{addr}")).await;

    a.ok(&["drop", &task]).await;
    b.ok(&["edit", &task, "!2"]).await;
    settle(&a, &b).await;

    let on_a = live(&a, &task).await;
    assert_eq!(
        on_a,
        live(&b, &task).await,
        "the devices converged on the delete"
    );
    assert!(
        on_a,
        "entity-level last-writer-wins: the later edit resurrects it"
    );
    assert_eq!(
        conflicts(&a).await,
        serde_json::json!([]),
        "D4: no conflict"
    );
    assert_eq!(conflicts(&b).await, serde_json::json!([]));

    // A drops it again, and B restores it.
    a.ok(&["drop", &task]).await;
    settle(&a, &b).await;
    assert!(!live(&b, &task).await, "dropped on both");
    b.ok(&["restore", &task]).await;
    settle(&a, &b).await;
    assert!(
        live(&a, &task).await && live(&b, &task).await,
        "restored on both"
    );
    assert_eq!(a.task(&task).await["priority"], 2, "with the fields it had");
    relay.abort();
}

/// Two devices retitle one task while apart, and one of them settles it.
///
/// What holds **today**: both CLIs write full-state ops (ADR-0065 D9), so the
/// later retitle wins, the earlier one is not kept, and no conflict is
/// reported (D4). `conflicts resolve --value` still settles the field, as a
/// write both devices converge on.
///
/// TODO(ADR-0065 8/8): once commands write `Patch` with parents, the two
/// retitles are concurrent `title` versions: assert that `conflicts --json`
/// on each device reports one `tracked` conflict on `title` with the other
/// title as its loser, that `conflicts resolve <id> title --pick <loser>`
/// makes the loser the title on both, and that the list is then empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_retitles_converge_and_resolve() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, task) = two_devices(dir.path(), &format!("http://{addr}")).await;

    a.ok(&["retitle", &task, "Renew passport at the embassy"])
        .await;
    b.ok(&["retitle", &task, "Renew passport by post"]).await;
    settle(&a, &b).await;

    let on_a = a.task(&task).await;
    assert_eq!(on_a["title"], b.task(&task).await["title"], "one title");
    assert_eq!(on_a["title"], "Renew passport by post", "the later write");
    assert_eq!(
        conflicts(&a).await,
        serde_json::json!([]),
        "D4: no conflict"
    );

    a.ok(&[
        "conflicts",
        "resolve",
        &task,
        "title",
        "--value",
        "Renew passport at the embassy",
    ])
    .await;
    settle(&a, &b).await;
    for device in [&a, &b] {
        assert_eq!(
            device.task(&task).await["title"],
            "Renew passport at the embassy"
        );
        assert_eq!(conflicts(device).await, serde_json::json!([]));
    }
    relay.abort();
}
