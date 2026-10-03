//! The `HEAD` side of the baseline driver's stdin/stdout protocol.
//!
//! The driver is `crates/sunrise-e2e/baseline-driver/src/main.rs`, built at a
//! pinned commit by `build-baseline.sh`. This file and that one are the two ends
//! of one protocol and change together: one JSON request per line in, one JSON
//! response per line out, `{"ok": true, ...}` or `{"ok": false, "error": ...}`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};
use sunrise_domain::Task;
use sunrise_id::EntityRef;

use super::events::LoggedEvent;
use super::{Applied, Replica};

/// Environment variable naming the built driver binary.
pub const DRIVER_ENV: &str = "SUNRISE_BASELINE_DRIVER";

/// A running baseline driver process and the one replica (A) it holds.
#[derive(Debug)]
pub(crate) struct BaselineReplica {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// The ref the driver was built at, as it reports itself.
    pub(crate) baseline: String,
}

/// What `init` hands back.
#[derive(Debug)]
pub(crate) struct Init {
    pub(crate) device_a: [u8; 16],
    pub(crate) device_b: [u8; 16],
    /// The vault root the baseline wrapped both vaults under.
    pub(crate) root: [u8; 32],
}

fn bytes<const N: usize>(v: &Value, key: &str) -> Result<[u8; N], String> {
    let s = v
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("init response lacks `{key}`"))?;
    hex::decode(s)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| format!("`{key}` is not {N} bytes"))
}

impl BaselineReplica {
    /// Start the driver at `binary`.
    pub(crate) fn spawn(binary: &Path) -> Result<Self, String> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", binary.display()))?;
        let stdin = child.stdin.take().ok_or("driver has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("driver has no stdout")?);
        let mut me = Self {
            child,
            stdin,
            stdout,
            baseline: String::new(),
        };
        let version = me.call(&json!({ "op": "version" }))?;
        version
            .get("baseline")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .clone_into(&mut me.baseline);
        Ok(me)
    }

    /// Send one request and read its one response line.
    fn call(&mut self, req: &Value) -> Result<Value, String> {
        writeln!(self.stdin, "{req}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("driver stdin: {e}"))?;
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| format!("driver stdout: {e}"))?;
        if n == 0 {
            return Err(format!("the driver exited while answering {req}"));
        }
        let resp: Value =
            serde_json::from_str(&line).map_err(|e| format!("driver answered `{line}`: {e}"))?;
        if resp.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(resp)
        } else {
            Err(resp
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("driver refused without an error")
                .to_owned())
        }
    }

    /// Create the account at the baseline: B first, A paired to it, B closed.
    pub(crate) fn init(
        &mut self,
        dir_a: &Path,
        dir_b: &Path,
        relay: &str,
        root: [u8; 32],
    ) -> Result<Init, String> {
        let resp = self.call(&json!({
            "op": "init",
            "dir_a": dir_a.to_string_lossy(),
            "dir_b": dir_b.to_string_lossy(),
            "relay": relay,
            "root": hex::encode(root),
        }))?;
        Ok(Init {
            device_a: bytes(&resp, "device_a")?,
            device_b: bytes(&resp, "device_b")?,
            root: bytes(&resp, "root")?,
        })
    }

    fn submit(&mut self, req: &Value) -> Result<Value, String> {
        self.call(req)
    }
}

impl Drop for BaselineReplica {
    fn drop(&mut self) {
        // A driver still running holds the vault lock on A. `close` asks it to
        // exit; this is the backstop for a scenario that panicked first.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[async_trait::async_trait]
impl Replica for BaselineReplica {
    async fn create_task(
        &mut self,
        title: &str,
        priority: Option<u8>,
        stream: EntityRef,
    ) -> Result<EntityRef, String> {
        let resp = self.submit(&json!({
            "op": "create_task",
            "title": title,
            "priority": priority,
            "stream": stream.to_str(),
        }))?;
        let id = resp
            .get("id")
            .and_then(Value::as_str)
            .ok_or("create_task returned no id")?;
        EntityRef::parse_any(id).map_err(|e| e.to_string())
    }

    async fn set_title(&mut self, id: EntityRef, title: &str) -> Result<(), String> {
        self.submit(&json!({ "op": "set_title", "id": id.to_str(), "title": title }))
            .map(drop)
    }

    async fn set_priority(&mut self, id: EntityRef, priority: Option<u8>) -> Result<(), String> {
        self.submit(&json!({ "op": "set_priority", "id": id.to_str(), "priority": priority }))
            .map(drop)
    }

    async fn delete(&mut self, id: EntityRef) -> Result<(), String> {
        self.submit(&json!({ "op": "delete", "id": id.to_str() }))
            .map(drop)
    }

    async fn apply_remote(&mut self, envelope: &[u8]) -> Applied {
        match self.call(&json!({ "op": "apply_remote", "envelope": hex::encode(envelope) })) {
            Ok(resp) if resp.get("refused").and_then(Value::as_bool) == Some(true) => {
                Applied::Refused {
                    corruption: resp.get("corruption").and_then(Value::as_bool) == Some(true),
                    error: resp
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }
            }
            Ok(_) => Applied::Accepted,
            Err(error) => Applied::Refused {
                corruption: false,
                error,
            },
        }
    }

    async fn task(&mut self, id: EntityRef) -> Option<Task> {
        let resp = self
            .call(&json!({ "op": "task", "id": id.to_str() }))
            .ok()?;
        let bytes = hex::decode(resp.get("task")?.as_str()?).ok()?;
        // A newer build reads what an older one wrote; that direction is the
        // one compatibility promise nobody disputes, so a failure here is a
        // harness defect worth seeing rather than a `None`.
        Some(ciborium::de::from_reader(bytes.as_slice()).expect("HEAD decodes a baseline Task"))
    }

    async fn status(&mut self) -> Result<(String, u32), String> {
        let resp = self.call(&json!({ "op": "status" }))?;
        let state = resp
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let pending = resp
            .get("outbox_pending")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(u32::MAX);
        Ok((state, pending))
    }

    async fn knows(&mut self, devices: &[[u8; 16]]) -> bool {
        let Ok(d) = self.call(&json!({ "op": "devices" })) else {
            return false;
        };
        let listed = d.get("devices").and_then(Value::as_array);
        devices.iter().all(|dev| {
            let want = hex::encode(dev);
            listed.is_some_and(|a| a.iter().any(|x| x.as_str() == Some(want.as_str())))
        })
    }

    async fn take_events(&mut self) -> Vec<LoggedEvent> {
        self.call(&json!({ "op": "events" }))
            .ok()
            .and_then(|resp| resp.get("events").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .map(LoggedEvent::from_json)
            .collect()
    }

    async fn close(mut self: Box<Self>) {
        let _ = self.call(&json!({ "op": "close" }));
        let _ = self.child.wait();
    }
}
