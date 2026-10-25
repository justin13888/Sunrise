//! A routine edit "this and future" between two CLI vaults on one account,
//! through a real relay and the real `sunrise` binary (issue #331,
//! `docs/02-domain/routines-and-recurrence.md` §Edit scope).
//!
//! The edit splits the series where it starts: the old routine keeps the
//! occurrences before it, a new routine continues from it with the new
//! title, and the occurrences after it are the same tasks on both devices,
//! adopted by the new routine, with none generated twice.

use serde_json::Value;
use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, Device};
use sunrise_e2e::spawn_relay;

/// `(id, title, routine)` of every task in stream `name`, by id.
async fn listed(device: &Device, name: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = device
        .json(&["stream", name])
        .await
        .as_array()
        .map(|ts| {
            ts.iter()
                .map(|t| {
                    let s = |k: &str| t[k].as_str().unwrap_or_default().to_owned();
                    (s("id"), s("title"), s("routine"))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn titles(routines: &Value) -> Vec<String> {
    let mut out: Vec<String> = routines
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r["title"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_this_and_future_edit_splits_the_series_on_both_devices() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;

    a.ok(&["streams", "create", "Garden"]).await;
    let tomorrow = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .tomorrow()
        .unwrap();
    let routine = first_id(
        &a.ok(&[
            "routines",
            "create",
            "Water plants",
            "--rule",
            "FREQ=DAILY",
            "--stream",
            "Garden",
            "--zone",
            "UTC",
            "--starts",
            &format!("{tomorrow} 09:00"),
        ])
        .await,
    );
    settle(&a, &b).await;
    let before = listed(&b, "Garden").await;
    assert!(before.len() >= 10, "{before:?}");

    // Split at the fourth occurrence.
    let pivot = tomorrow.checked_add(jiff::Span::new().days(3)).unwrap();
    let key = format!("{pivot}T09:00");
    let out = a
        .ok(&[
            "routines",
            "edit",
            &routine,
            "--title",
            "Water the garden",
            "--scope",
            "future",
            "--at",
            &key,
        ])
        .await;
    let next = first_id(&out);
    assert_ne!(
        next, routine,
        "the split prints the routine that continues it"
    );
    settle(&a, &b).await;

    for device in [&a, &b] {
        assert_eq!(
            titles(&device.json(&["routines"]).await),
            vec!["Water plants".to_owned(), "Water the garden".to_owned()]
        );
        let after = listed(device, "Garden").await;
        assert_eq!(
            after.iter().map(|t| &t.0).collect::<Vec<_>>(),
            before.iter().map(|t| &t.0).collect::<Vec<_>>(),
            "the same occurrences, none generated twice"
        );
        let moved: Vec<_> = after.iter().filter(|t| t.1 == "Water the garden").collect();
        let kept: Vec<_> = after.iter().filter(|t| t.1 == "Water plants").collect();
        assert_eq!(kept.len(), 3, "{after:?}");
        assert_eq!(moved.len(), after.len() - 3);
        assert!(moved.iter().all(|t| t.2 == next));
        assert!(kept.iter().all(|t| t.2 == routine));
    }
    assert_eq!(listed(&a, "Garden").await, listed(&b, "Garden").await);
    relay.abort();
}
