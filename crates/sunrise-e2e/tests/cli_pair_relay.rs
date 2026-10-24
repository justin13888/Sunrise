//! Pairing two CLI vaults over a live relay, through the real `sunrise`
//! binary: the path the Apple apps pair by, proven with no Apple tooling.
//!
//! Device A is created and published with `sunrise bootstrap`. Device B has no
//! vault at all; `sunrise pair join` on it prints a code, `sunrise pair add`
//! on A takes it, both print the same six digits, and each confirms them. The
//! test carries nothing between the two processes but the code, which is what
//! a camera or a paste carries. Then a task written on either device shows on
//! the other after sync — B is a second device on A's account, not a copy.
//!
//! The digits are the check. A device whose user declines them throws the
//! pairing away on both sides: no vault on the device being added, and no
//! second device on the account.

#![allow(clippy::missing_panics_doc)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, Output};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;
use sunrise_e2e::cli::{first_id, settle, sign_in, Device, SELF_HOST_BEARER};
use sunrise_e2e::spawn_relay;

/// Generous: the host running these may be loaded, and every step is a
/// process start against a relay.
const WAIT: Duration = Duration::from_secs(180);

/// A `sunrise` process still running, and the `--json` events it has printed.
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

    /// Type `line` at the prompt.
    fn answer(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin is piped");
        writeln!(stdin, "{line}").expect("write the answer");
        stdin.flush().expect("flush the answer");
    }

    /// Wait for it to exit.
    async fn finish(mut self) -> Output {
        drop(self.child.stdin.take());
        tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || self.child.wait_with_output()),
        )
        .await
        .expect("the command exits")
        .expect("join the process")
        .expect("wait for sunrise")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Device A: bootstrapped onto the relay, with a task already written.
async fn founding(root: &std::path::Path, relay: &str) -> (Device, String) {
    let a = Device::new(&root.join("a"), relay);
    sign_in(&a.vault);
    let task = first_id(&a.ok(&["capture", "Renew passport"]).await);
    a.ok(&["bootstrap", "a@example.com"]).await;
    a.sync().await;
    (a, task)
}

/// Device B: no vault yet, and the account's bearer to present.
fn joining(root: &std::path::Path, relay: &str) -> Device {
    let mut b = Device::new(&root.join("b"), relay);
    b.bearer = Some(SELF_HOST_BEARER.to_owned());
    b
}

async fn device_count(device: &Device) -> usize {
    device
        .json(&["devices"])
        .await
        .as_array()
        .map_or(0, Vec::len)
}

/// Digits that are certainly not `sas`: its last digit moved on by one.
fn other_than(sas: &str) -> String {
    let (head, last) = sas.split_at(sas.len() - 1);
    let last = last.parse::<u8>().expect("a digit");
    format!("{head}{}", (last + 1) % 10)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_cli_vaults_pair_over_the_relay_and_then_sync() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();
    let (a, before) = founding(dir.path(), &url).await;
    let b = joining(dir.path(), &url);

    let mut join = Running::start(
        &b,
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
    let mut add = Running::start(&a, &["--json", "pair", "add", &code, "--yes"]);

    let on_b = join.event("sas").await["sas"].clone();
    let on_a = add.event("sas").await["sas"].clone();
    assert_eq!(on_a, on_b, "both devices show the same six digits");
    assert_eq!(on_a.as_str().map(str::len), Some(6), "{on_a}");

    add.event("paired").await;
    let paired = join.event("paired").await;
    assert!(
        paired["relay_device"].is_string(),
        "the new device registered with the relay: {paired}"
    );
    let (added, joined) = (add.finish().await, join.finish().await);
    assert!(added.status.success(), "pair add: {}", stderr(&added));
    assert!(joined.status.success(), "pair join: {}", stderr(&joined));

    // One account, two devices.
    assert_eq!(
        a.json(&["identity"]).await["account"],
        b.json(&["identity"]).await["account"],
        "B joined A's account"
    );
    assert_eq!(
        b.json(&["identity"]).await["can_sponsor"],
        false,
        "a device added by pairing holds no identity signing key"
    );

    // What A wrote before B existed, and what each writes now, reach the other.
    b.sync().await;
    assert_eq!(b.task(&before).await["title"], "Renew passport");
    let from_a = first_id(&a.ok(&["capture", "Book the photo booth"]).await);
    let from_b = first_id(&b.ok(&["capture", "Find the old passport"]).await);
    settle(&a, &b).await;
    assert_eq!(b.task(&from_a).await["title"], "Book the photo booth");
    assert_eq!(a.task(&from_b).await["title"], "Find the old passport");
    assert_eq!(device_count(&a).await, 2, "A sees B on the account");
    relay.abort();
}

/// The device with the vault is asked, types digits that are not the ones it
/// shows, and the pairing ends on both devices with nothing handed over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn digits_typed_wrong_at_the_prompt_pair_nothing() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = founding(dir.path(), &url).await;
    let b = joining(dir.path(), &url);

    let mut join = Running::start(&b, &["--json", "pair", "join", "--yes"]);
    let code = join.event("code").await["code"]
        .as_str()
        .expect("the code is text")
        .to_owned();
    // No `--yes`: A asks, and reads the answer from stdin.
    let mut add = Running::start(&a, &["--json", "pair", "add", &code]);
    let sas = add.event("sas").await["sas"]
        .as_str()
        .expect("digits")
        .to_owned();
    add.answer(&other_than(&sas));

    let (added, joined) = (add.finish().await, join.finish().await);
    assert!(!added.status.success(), "a declined pairing fails");
    assert!(
        stderr(&added).contains("did not match"),
        "{}",
        stderr(&added)
    );
    assert!(
        !joined.status.success(),
        "the device being added learns it is over: {}",
        stderr(&joined)
    );
    assert!(
        !b.vault.join("vault.db").exists(),
        "no vault on the device being added"
    );
    a.sync().await;
    assert_eq!(device_count(&a).await, 1, "no second device on the account");
    relay.abort();
}

/// The device being added declines: `--confirm-sas` names digits it does not
/// show. The device with the vault, which accepted, is told and issues nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_device_that_declines_the_digits_is_not_added() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = founding(dir.path(), &url).await;
    let b = joining(dir.path(), &url);

    // Seven digits: never the six this device shows.
    let mut join = Running::start(&b, &["--json", "pair", "join", "--confirm-sas", "0000000"]);
    let code = join.event("code").await["code"]
        .as_str()
        .expect("the code is text")
        .to_owned();
    let add = Running::start(&a, &["--json", "pair", "add", &code, "--yes"]);

    let (joined, added) = (join.finish().await, add.finish().await);
    assert!(!joined.status.success());
    assert!(
        stderr(&joined).contains("did not match"),
        "{}",
        stderr(&joined)
    );
    assert!(
        !added.status.success(),
        "the device with the vault learns it is over: {}",
        stderr(&added)
    );
    assert!(!b.vault.join("vault.db").exists());
    assert_eq!(device_count(&a).await, 1);
    relay.abort();
}

/// Without a relay there is nothing to pair through, and the command says so
/// — and which way still works — before it touches anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_over_the_relay_needs_a_relay_and_a_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = Device::new(&dir.path().join("b"), "http://127.0.0.1:9");
    b.relay = None;
    let out = b.run(&["pair", "join", "--yes"]).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("SUNRISE_SYNC_URL"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("pair offer"), "{}", stderr(&out));

    let b = Device::new(&dir.path().join("b"), "http://127.0.0.1:9");
    let out = b.run(&["pair", "join", "--yes"]).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("SUNRISE_SYNC_TOKEN"),
        "{}",
        stderr(&out)
    );
    assert!(!b.vault.join("vault.db").exists());
}
