//! A `sunrise` process left running while the scenario drives the other
//! device, and pairing two devices over the relay with it.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Output};
use std::sync::mpsc;

use serde_json::Value;

use super::{Device, WAIT};

/// A `sunrise` process still running, and the `--json` events it prints.
/// The shape `cli_pair_relay.rs` drives pairing with.
#[derive(Debug)]
pub struct Running {
    child: Child,
    events: Option<mpsc::Receiver<Value>>,
}

impl Running {
    /// Start `sunrise <args>` as `device`, reading its `--json` events.
    ///
    /// # Panics
    /// When the process cannot be started.
    #[must_use]
    pub fn start(device: &Device, args: &[&str]) -> Self {
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
    ///
    /// # Panics
    /// When no event arrives in time, or it is not `name`.
    pub async fn event(&mut self, name: &str) -> Value {
        let rx = self.events.take().expect("one wait at a time");
        let (rx, got) = tokio::task::spawn_blocking(move || {
            let got = rx.recv_timeout(WAIT);
            (rx, got)
        })
        .await
        .expect("join the reader");
        self.events = Some(rx);
        let event = match got {
            Ok(event) => event,
            // The process closed its stdout: say how it ended, which is the
            // only account of why the event never came.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = self.child.kill();
                let mut stderr = String::new();
                if let Some(mut pipe) = self.child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
                }
                let status = self.child.wait().ok();
                panic!("no {name:?} event: the process ended ({status:?}); stderr:\n{stderr}")
            }
            Err(e) => panic!("no {name:?} event ({e})"),
        };
        assert_eq!(event["event"], name, "{event}");
        event
    }

    /// Wait for it to exit, successfully.
    ///
    /// # Panics
    /// When it does not exit in time, or fails.
    pub async fn finish(mut self) -> Output {
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

/// Pair a vault-less `b` onto `a`'s account over the relay: `sunrise pair
/// join` on `b`, `sunrise pair add` on `a`, both confirming the SAS.
///
/// # Panics
/// When either side fails.
pub async fn pair_over_relay(a: &Device, b: &Device) {
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
