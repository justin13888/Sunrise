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

#![allow(clippy::missing_panics_doc)]

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Output};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;
use sunrise_e2e::cli::{sign_in, Device, SELF_HOST_BEARER};
use sunrise_e2e::spawn_relay_with;
use sunrise_server::relay_log::DurableCaps;
use sunrise_server::ServerConfig;

/// Generous: every step is a process start against a relay.
const WAIT: Duration = Duration::from_secs(180);

/// A `sunrise` process still running, and the `--json` events it prints.
/// The shape `cli_pair_relay.rs` drives pairing with.
struct Running {
    child: Child,
    events: Option<mpsc::Receiver<Value>>,
}

impl Running {
    fn start(device: &Device, args: &[&str]) -> Self {
        let mut child = device.spawn(args);
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            events: Some(rx),
        }
    }

    /// The next event, which must be `name`.
    async fn event(&mut self, name: &str) -> Value {
        let rx = self.events.take().expect("one wait at a time");
        let (rx, got) = tokio::task::spawn_blocking(move || {
            let got = rx.recv_timeout(WAIT);
            (rx, got)
        })
        .await
        .expect("join the reader");
        self.events = Some(rx);
        let event = got.unwrap_or_else(|e| panic!("no {name:?} event ({e})"));
        assert_eq!(event["event"], name, "{event}");
        event
    }

    /// Wait for it to exit, successfully.
    async fn finish(mut self) -> Output {
        drop(self.child.stdin.take());
        let out = tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || self.child.wait_with_output()),
        )
        .await
        .expect("the command exits")
        .expect("join the process")
        .expect("wait for sunrise");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
}

/// Pair a vault-less `b` onto `a`'s account over the relay.
async fn pair(a: &Device, b: &Device) {
    let mut join = Running::start(
        b,
        &[
            "--json",
            "pair",
            "join",
            "--yes",
            "--email",
            "a@example.com",
        ],
    );
    let code = join.event("code").await["code"]
        .as_str()
        .expect("the code is text")
        .to_owned();
    let mut add = Running::start(a, &["--json", "pair", "add", &code, "--yes"]);
    add.event("sas").await;
    join.event("sas").await;
    add.event("paired").await;
    join.event("paired").await;
    add.finish().await;
    join.finish().await;
}

/// Tasks A writes before each of its syncs. The first frame alone is more
/// than the relay keeps, so the second evicts it, and the record the first
/// sync pinned covers exactly what was evicted.
const ROUNDS: [usize; 3] = [20, 1, 1];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_joining_after_the_relay_trimmed_catches_up_from_the_snapshot() {
    let mut metrics = None;
    let (addr, relay) = spawn_relay_with(ServerConfig::default(), |s| {
        metrics = Some(s.metrics.clone());
        s.with_durable_caps(DurableCaps {
            max_bytes: 8 * 1024,
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
    pair(&a, &b).await;
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
