//! `sunrise sync` catching a device up from a stream's snapshot record when
//! the relay no longer holds the ops (issue #462, ADR-0068), with the real
//! `sunrise` binary against a real relay whose durable log keeps only a few
//! frames per channel.
//!
//! Device A writes its history over three syncs. The first `sunrise sync`
//! writes the inbox's snapshot record as the stream's compactor and pins it
//! on the relay before it reports `live`; the second pushes the frame that
//! record covers out of the relay. Device B, paired onto the account
//! afterwards over the relay, starts with no cursors; its `sunrise sync` is
//! told the inbox has a gap, catches the inbox up from the pinned record, takes
//! the frames above it from the relay, and reports `live` with every task —
//! not `changes_missing`.
//!
//! B pairs here rather than recovering from A's code, because pairing hands
//! over the Stream keys. Recovery from the code against a relay that has
//! trimmed every vault-meta frame is `cli_recover_trimmed.rs`: it needs the
//! account's key bundle as well as the records (#580, ADR-0069).

#![allow(clippy::missing_panics_doc)]

use sunrise_e2e::cli::{pair_over_relay, sign_in, Device, SELF_HOST_BEARER};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::relay_log::DurableCaps;
use sunrise_server::ServerConfig;

/// Tasks A writes before each of its syncs. The first frame alone is more
/// than the relay keeps, so the second evicts it, and the record the first
/// sync pinned covers exactly what was evicted.
const ROUNDS: [usize; 3] = [20, 1, 1];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_joining_after_the_relay_trimmed_catches_up_from_the_snapshot() {
    let mut metrics = None;
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| {
        metrics = Some(s.metrics.clone());
        // Small enough that the first frame overflows it on its own: a
        // capture is a create `Patch` of its title alone (ADR-0065 §7), a
        // few hundred bytes sealed.
        s.with_durable_caps(DurableCaps {
            max_bytes: 4 * 1024,
            max_age_ms: u64::MAX,
        })
    })
    .await;
    let metrics = metrics.expect("the relay's registry");
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();

    let a = Device::new(&dir.path().join("a"), &url);
    sign_in(&a.vault);
    a.ok(&["bootstrap", "a@example.com"]).await;
    // Three syncs, so three frames: the first sync's `live` writes and pins
    // the record, and the frames after it push the first out of the relay.
    for (round, tasks) in ROUNDS.iter().enumerate() {
        for i in 0..*tasks {
            a.ok(&["capture", &format!("history {round}.{i}")]).await;
        }
        a.sync().await;
    }
    let tasks: usize = ROUNDS.iter().sum();

    let mut b = Device::new(&dir.path().join("b"), &url);
    b.bearer = Some(SELF_HOST_BEARER.to_owned());
    pair_over_relay(&a, &b).await;
    // `sync --once` succeeds only on `live`: a gap left uncovered ends it
    // `Degraded`, which is what this did before the record travelled.
    b.sync().await;

    assert!(
        metrics.get("sunrise_relay_cursor_gap_total") > 0,
        "the relay could not replay A's history: B caught up from the record"
    );
    let inbox = b.json(&["inbox"]).await;
    assert_eq!(
        inbox.as_array().map(Vec::len),
        Some(tasks),
        "B holds every task A wrote: {inbox}"
    );
    relay.abort();
}
