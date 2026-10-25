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
/// Both devices are this build and have told each other so, so their vault
/// requires `core.field_ops` and every command writes a `Patch` of the fields
/// it changed (ADR-0065 §7): A's priority and B's retitle and note all
/// survive (#447).
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
    assert_eq!(on_a["priority"], 1, "A's edit survived too");
    assert_eq!(
        conflicts(&a).await,
        serde_json::json!([]),
        "different fields: nothing to resolve"
    );
    relay.abort();
}

/// Two defers of one task, made while the devices are apart.
///
/// Both devices are this build and have told each other so, so each defer is
/// a `Patch` that sets the plan and adds one to `deferred_count` (ADR-0065
/// §7): both count (#334), and the two plans are a conflict both devices
/// report, even where one defer sets the plan the task already had.
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
    assert_eq!(on_a["deferred_count"], 2, "both deferrals count");
    let reported = |c: &Value| {
        c.as_array().is_some_and(|cs| {
            cs.iter()
                .any(|c| c["entity"] == task.as_str() && c["field"] == "scheduled_at")
        })
    };
    let (on_a, on_b) = (conflicts(&a).await, conflicts(&b).await);
    assert!(reported(&on_a), "{on_a}");
    assert_eq!(on_a, on_b, "both devices report the same conflict");
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
/// Both devices write `Patch`es naming their heads (ADR-0065 §7), so the drop
/// and the edit are concurrent versions. The delete wins (§3, tombstone),
/// and B's edit is kept inside the tombstone as an "edited while deleted"
/// conflict on `deleted`, which both devices report. `sunrise restore`, a
/// write that descends from both, brings the task back with B's edit and
/// clears the conflict everywhere (§4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_on_one_device_against_an_edit_on_the_other() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, task) = two_devices(dir.path(), &format!("http://{addr}")).await;

    a.ok(&["drop", &task]).await;
    b.ok(&["edit", &task, "!2"]).await;
    settle(&a, &b).await;

    assert!(!live(&a, &task).await, "the delete wins on A");
    assert!(!live(&b, &task).await, "and on B");
    let on_a = conflicts(&a).await;
    assert_eq!(on_a, conflicts(&b).await, "both report the same conflict");
    let deleted: Vec<&Value> = on_a
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["entity"] == task.as_str() && c["field"] == "deleted")
        .collect();
    assert_eq!(deleted.len(), 1, "{on_a}");
    let edits = deleted[0]["edited_while_deleted"].as_array().cloned();
    assert!(
        edits.is_some_and(|e| e.iter().any(|e| e["fields"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f == "priority")))),
        "B's edit is named: {on_a}"
    );

    b.ok(&["restore", &task]).await;
    settle(&a, &b).await;
    assert!(
        live(&a, &task).await && live(&b, &task).await,
        "restored on both"
    );
    assert_eq!(
        a.task(&task).await["priority"],
        2,
        "with the edit made while it was deleted"
    );
    for device in [&a, &b] {
        assert_eq!(conflicts(device).await, serde_json::json!([]), "settled");
    }
    relay.abort();
}

/// Two devices retitle one task while apart, and one of them settles it.
///
/// Both write `Patch`es naming their heads (ADR-0065 §7), so the two retitles
/// are concurrent `title` versions: each device reports one `tracked`
/// conflict on `title`, whose loser is the other title.
/// `conflicts resolve <id> title --pick <loser>` makes the loser the title on
/// both, and the list is then empty (§4).
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
    let listed = conflicts(&a).await;
    assert_eq!(listed, conflicts(&b).await, "both report the same conflict");
    let titles: Vec<&Value> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["entity"] == task.as_str() && c["field"] == "title")
        .collect();
    assert_eq!(titles.len(), 1, "{listed}");
    assert_eq!(titles[0]["class"], "tracked");
    let losers = titles[0]["losers"].as_array().cloned().unwrap_or_default();
    assert_eq!(losers.len(), 1, "{listed}");
    let (winner, loser) = (&titles[0]["winner"]["value"], &losers[0]["value"]);
    assert_eq!(*winner, on_a["title"]);
    let mut both = [winner.clone(), loser.clone()];
    both.sort_by_key(ToString::to_string);
    assert_eq!(
        both,
        [
            Value::from("Renew passport at the embassy"),
            Value::from("Renew passport by post")
        ],
        "the other title is the loser"
    );

    let pick = losers[0]["version"].as_str().expect("a version").to_owned();
    a.ok(&["conflicts", "resolve", &task, "title", "--pick", &pick])
        .await;
    settle(&a, &b).await;
    for device in [&a, &b] {
        assert_eq!(device.task(&task).await["title"], *loser);
        assert_eq!(conflicts(device).await, serde_json::json!([]));
    }
    relay.abort();
}

/// The ids of the blocks `device` lists for tomorrow.
async fn blocks_tomorrow(device: &Device) -> Vec<String> {
    let listed = device.json(&["blocks", "tomorrow"]).await;
    let mut ids: Vec<String> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["id"].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    ids
}

/// A merge and a bulk completion are each one commit (ADR-0065 §6): the
/// peer shows all of either or none, a batch with one task it cannot
/// complete changes nothing, and one `undo` takes the whole batch back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_and_a_bulk_completion_reach_the_peer_whole() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, first) = two_devices(dir.path(), &format!("http://{addr}")).await;
    let second = first_id(&a.ok(&["capture", "Book the ferry"]).await);
    let mut blocks = Vec::new();
    for (from, to) in [
        ("tomorrow 9am", "tomorrow 10am"),
        ("tomorrow 10am", "tomorrow 11am"),
    ] {
        blocks.push(first_id(
            &a.ok(&[
                "block", "create", "--from", from, "--to", to, "--title", "Desk",
            ])
            .await,
        ));
    }
    settle(&a, &b).await;

    // A batch naming a task that does not exist refuses every task in it.
    let refused = a
        .run(&["done", &first, "tsk_01J00000000000000000000000"])
        .await;
    assert!(!refused.status.success());
    assert_eq!(a.task(&first).await["state"], "todo");

    a.ok(&["done", &first, &second]).await;
    let merged = first_id(&a.ok(&["block", "merge", &blocks[0], &blocks[1]]).await);
    settle(&a, &b).await;
    for id in [&first, &second] {
        assert_eq!(b.task(id).await["state"], "done", "{id}");
    }
    assert_eq!(blocks_tomorrow(&b).await, vec![merged]);
    assert_eq!(blocks_tomorrow(&a).await, blocks_tomorrow(&b).await);

    // The batch is one step of the undo history, and its undo one commit.
    a.ok(&["undo"]).await;
    settle(&a, &b).await;
    for id in [&first, &second] {
        assert_eq!(b.task(id).await["state"], "todo", "{id}");
    }
    relay.abort();
}

/// The Inbox's ids, in the order `device` lists them.
async fn inbox_order(device: &Device) -> Vec<String> {
    device
        .json(&["inbox"])
        .await
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["id"].as_str().map(str::to_owned))
        .collect()
}

/// A hand-arranged order made on one device is the order on the other
/// (#340): `move` writes the task's `sort_order`, and it syncs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reorder_on_one_device_appears_on_the_other() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, first) = two_devices(dir.path(), &format!("http://{addr}")).await;
    let second = first_id(&a.ok(&["capture", "Book the ferry"]).await);
    let third = first_id(&a.ok(&["capture", "Pack"]).await);
    // Moving every row of an unarranged list to the top changes nothing.
    a.ok(&["move", &first, &second, &third, "first"]).await;
    assert!(a.task(&first).await["sort_order"].is_null());
    a.ok(&["move", &third, "before", &first]).await;
    let moved = a.json(&["move", &second, "after", &third]).await;
    let on_a = inbox_order(&a).await;
    assert_eq!(on_a, vec![third.clone(), second.clone(), first.clone()]);
    assert_eq!(
        moved.as_array().map(|ts| ts
            .iter()
            .filter_map(|t| t["id"].as_str())
            .collect::<Vec<_>>()),
        Some(on_a.iter().map(String::as_str).collect()),
        "move --json prints the list in its new order"
    );
    assert!(moved[0]["sort_order"].as_str().is_some(), "{moved}");
    settle(&a, &b).await;
    assert_eq!(inbox_order(&b).await, on_a, "B shows A's arrangement");

    // An undo puts the keys back, and that syncs too.
    a.ok(&["undo"]).await;
    settle(&a, &b).await;
    let undone = vec![third.clone(), first.clone(), second.clone()];
    assert_eq!(inbox_order(&a).await, undone);
    assert_eq!(inbox_order(&b).await, undone);
    relay.abort();
}

/// Two devices that rearrange one list while apart converge on one order:
/// each task's key is one register, so the moves of different tasks both
/// survive, and equal keys break by id the same way on both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reorders_converge() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, first) = two_devices(dir.path(), &format!("http://{addr}")).await;
    a.ok(&["capture", "Book the ferry"]).await;
    let third = first_id(&a.ok(&["capture", "Pack"]).await);
    // Place all three: a drop below unplaced rows places them too, in the
    // order the list shows them, so read that order back.
    a.ok(&["move", &third, "first"]).await;
    a.ok(&["move", &first, "last"]).await;
    let base = inbox_order(&a).await;
    for t in a.json(&["inbox"]).await.as_array().unwrap() {
        assert!(t["sort_order"].is_string(), "{t}");
    }
    settle(&a, &b).await;
    assert_eq!(inbox_order(&b).await, base);

    // Apart: A sends the top task to the bottom, B brings the bottom one to
    // the top. Two tasks, two registers: both moves survive.
    a.ok(&["move", &base[0], "last"]).await;
    b.ok(&["move", &base[2], "first"]).await;
    settle(&a, &b).await;
    let on_a = inbox_order(&a).await;
    assert_eq!(on_a, inbox_order(&b).await, "one order on both devices");
    assert_eq!(
        on_a,
        vec![base[2].clone(), base[1].clone(), base[0].clone()]
    );

    // Apart again, each moving a task into the same gap: one key on both,
    // and the id decides between the two the same way everywhere.
    let fourth = first_id(&a.ok(&["capture", "Water the plants"]).await);
    a.ok(&["move", &fourth, "last"]).await;
    settle(&a, &b).await;
    a.ok(&["move", &base[0], "after", &base[2]]).await;
    b.ok(&["move", &fourth, "after", &base[2]]).await;
    settle(&a, &b).await;
    let on_a = inbox_order(&a).await;
    assert_eq!(on_a, inbox_order(&b).await, "one order on both devices");
    assert_eq!(on_a[0], base[2]);
    assert_eq!(on_a[3], base[1], "both landed between the same two tasks");
    relay.abort();
}

/// The ids of the tasks `views apply <name>` lists on `device`.
async fn applied(device: &Device, name: &str) -> Vec<String> {
    device
        .json(&["views", "apply", name])
        .await
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["id"].as_str().map(str::to_owned))
        .collect()
}

/// A view saved on one device is the vault's, so it runs on the other
/// (#341), and it names its contexts by id, so renaming a context on either
/// device breaks nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_view_saved_on_one_device_runs_on_the_other_through_a_rename() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, _) = two_devices(dir.path(), &format!("http://{addr}")).await;
    a.ok(&["contexts", "create", "errands"]).await;
    let milk = first_id(&a.ok(&["capture", "Buy milk @errands"]).await);
    a.ok(&["capture", "Write the report"]).await;
    a.ok(&["views", "save", "out", "inbox", "--contexts", "errands"])
        .await;
    settle(&a, &b).await;

    let listed = b.json(&["views"]).await;
    assert_eq!(listed[0]["name"], "out", "{listed}");
    assert_eq!(listed[0]["contexts"][0], "errands");
    assert_eq!(applied(&b, "out").await, vec![milk.clone()]);

    b.ok(&["contexts", "edit", "errands", "--name", "shops"])
        .await;
    settle(&a, &b).await;
    assert_eq!(applied(&a, "out").await, vec![milk.clone()], "on A");
    assert_eq!(applied(&b, "out").await, vec![milk.clone()], "on B");
    assert_eq!(a.json(&["views"]).await[0]["contexts"][0], "shops");

    // Renamed on A, deleted on B: one list on both.
    a.ok(&["views", "rename", "out", "errands run"]).await;
    settle(&a, &b).await;
    assert_eq!(b.json(&["views"]).await[0]["name"], "errands run");
    b.ok(&["views", "delete", "errands run"]).await;
    settle(&a, &b).await;
    assert_eq!(a.json(&["views"]).await, serde_json::json!([]));
    relay.abort();
}

/// A device's old `views.toml` is imported once into the vault: its context
/// names become ids, a name the vault lacks is reported and kept, and the
/// file is never written again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_views_file_is_imported_once() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b, _) = two_devices(dir.path(), &format!("http://{addr}")).await;
    a.ok(&["contexts", "create", "errands"]).await;
    let milk = first_id(&a.ok(&["capture", "Buy milk @errands"]).await);
    let file = a
        .vault
        .with_extension("config")
        .join("sunrise")
        .join("views.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let body = "out = \"view=inbox;contexts=errands,ghost\"\n";
    std::fs::write(&file, body).unwrap();

    let out = a.run(&["views"]).await;
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost"),
        "the unresolved name is reported: {stderr}"
    );
    assert_eq!(applied(&a, "out").await, vec![milk.clone()]);
    let listed = a.json(&["views"]).await;
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(1),
        "imported once: {listed}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        body,
        "the file is left as it was"
    );
    a.ok(&["views", "save", "more", "today"]).await;
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        body,
        "and never written"
    );
    settle(&a, &b).await;
    assert_eq!(applied(&b, "out").await, vec![milk]);
    relay.abort();
}
