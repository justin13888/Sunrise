//! One baseline Sunrise core, driven over stdin and stdout.
//!
//! The cross-version merge harness (#326, ADR-0057) has to run two builds of
//! `sunrise-core` against each other, and two builds of one crate cannot be
//! linked into one test binary without dragging the whole older workspace into
//! every `cargo test`. So the older build lives in this process, compiled
//! inside an extracted copy of the baseline commit, and the `HEAD` test
//! process talks to it one JSON object per line:
//!
//! - every request is `{"op": <name>, ...}` on one stdin line;
//! - every request gets exactly one response line on stdout, `{"ok": true, ...}`
//!   or `{"ok": false, "error": <text>}`.
//!
//! Nothing else is ever written to stdout; diagnostics go to stderr. The
//! protocol is spelled out where it is consumed, in
//! `crates/sunrise-e2e/src/cross_version/baseline.rs`, and the two have to
//! change together.
//!
//! `init` creates the account the way that build creates one: vault B is the
//! account's first device, vault A is paired to it with that build's pairing,
//! and B is closed again without ever syncing. The harness then opens B with
//! `HEAD`, which is the upgrade under test, while A stays on the baseline and
//! syncs through the `HEAD` relay.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use sunrise_core::{
    BoxTransport, Command, Core, CoreConfig, CoreError, EngineError, Query, QueryResult, Rng,
    SyncConfig, SystemClock, SystemRng, TransportFactory, Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{TaskDraft, TaskPatch};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_sync::SseTransport;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

/// App identity the baseline core advertises in its sync `Hello`.
const APP_ID: &str = "0.1.0+cross-version-baseline";

/// Anti-entropy period, clocked fast for the same reason `sunrise-e2e`'s own
/// harness cores are: a test has to watch recovery happen.
const RESYNC_INTERVAL: Duration = Duration::from_millis(200);

/// Events this process has logged since the last `events` request.
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// Records every warning and error the baseline core logs, plus the one
/// debug-level event that says an inbound op failed its integrity checks
/// (`sync.loss_evidence` with `cause = "corrupt_op"`). Those are the "never
/// errors" and "never classes an op as corruption" halves of the no-break
/// property; the harness reads them with `events`.
struct Capture;

#[derive(Default)]
struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_owned(), Value::from(format!("{value:?}")));
    }
}

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let level = *event.metadata().level();
        let loss = fields.0.get("ev").and_then(Value::as_str) == Some("sync.loss_evidence");
        if level > tracing::Level::WARN && !loss {
            return;
        }
        fields
            .0
            .insert("level".to_owned(), Value::from(level.to_string()));
        fields
            .0
            .insert("target".to_owned(), Value::from(event.metadata().target()));
        if let Ok(mut events) = EVENTS.lock() {
            events.push(Value::Object(fields.0));
        }
    }
}

fn bytes32(hex_str: &str) -> Result<[u8; 32], String> {
    let raw = hex::decode(hex_str).map_err(|e| e.to_string())?;
    raw.try_into().map_err(|_| "expected 32 bytes".to_owned())
}

fn str_field<'a>(req: &'a Value, name: &str) -> Result<&'a str, String> {
    req.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field `{name}`"))
}

fn task_ref(req: &Value) -> Result<EntityRef, String> {
    EntityRef::parse(str_field(req, "id")?, EntityKind::Task).map_err(|e| e.to_string())
}

fn priority(req: &Value) -> Option<u8> {
    req.get("priority")
        .and_then(Value::as_u64)
        .and_then(|p| u8::try_from(p).ok())
}

fn factory(url: String) -> TransportFactory {
    Arc::new(move || {
        let url = url.clone();
        Box::pin(async move {
            let t = SseTransport::connect(&url);
            Ok(Box::new(t) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

fn config(dir: PathBuf, relay: Option<&str>) -> CoreConfig {
    let mut cfg = CoreConfig::with_clock(dir, APP_ID, Arc::new(SystemClock), Arc::new(SystemRng));
    cfg.sync =
        relay.map(|url| SyncConfig::new(url.to_owned()).with_resync_interval(RESYNC_INTERVAL));
    cfg
}

/// Whether an `apply_remote` refusal is the one the sync driver counts as
/// corruption, as opposed to a policy outcome such as an untrusted device.
fn is_corruption(e: &CoreError) -> bool {
    matches!(e, CoreError::Engine(EngineError::RemoteOpInvalid(_)))
}

struct Driver {
    core: Option<Arc<Core>>,
}

impl Driver {
    fn core(&self) -> Result<&Arc<Core>, String> {
        self.core
            .as_ref()
            .ok_or_else(|| "no core; send `init` first".to_owned())
    }

    async fn submit(&self, cmd: Command) -> Result<Value, String> {
        let res = self.core()?.submit(cmd).await.map_err(|e| e.to_string())?;
        Ok(json!({ "id": res.entity.to_str() }))
    }

    async fn handle(&mut self, req: &Value) -> Result<Value, String> {
        match str_field(req, "op")? {
            // `build-baseline.sh` stamps the ref in at compile time, so the
            // harness learns which baseline it is talking to from the binary
            // itself rather than from a second variable that could disagree.
            "version" => Ok(json!({
                "baseline": option_env!("SUNRISE_BASELINE_REF").unwrap_or("unknown"),
            })),
            "init" => self.init(req).await,
            "create_task" => {
                let stream = match req.get("stream").and_then(Value::as_str) {
                    Some(s) => {
                        Some(EntityRef::parse(s, EntityKind::Stream).map_err(|e| e.to_string())?)
                    }
                    None => None,
                };
                let draft = TaskDraft {
                    title: str_field(req, "title")?.to_owned(),
                    stream_id: stream,
                    priority: priority(req),
                    ..TaskDraft::default()
                };
                self.submit(Command::CreateTask(draft)).await
            }
            "set_title" => {
                let patch = TaskPatch {
                    title: Some(str_field(req, "title")?.to_owned()),
                    ..TaskPatch::default()
                };
                self.submit(Command::UpdateTask {
                    id: task_ref(req)?,
                    patch,
                })
                .await
            }
            "set_priority" => {
                let patch = TaskPatch {
                    priority: Some(priority(req)),
                    ..TaskPatch::default()
                };
                self.submit(Command::UpdateTask {
                    id: task_ref(req)?,
                    patch,
                })
                .await
            }
            "delete" => self.submit(Command::DeleteTask(task_ref(req)?)).await,
            "apply_remote" => {
                let bytes = hex::decode(str_field(req, "envelope")?).map_err(|e| e.to_string())?;
                Ok(match self.core()?.apply_remote(&bytes).await {
                    Ok(_) => json!({ "refused": false }),
                    Err(e) => json!({
                        "refused": true,
                        "corruption": is_corruption(&e),
                        "error": e.to_string(),
                    }),
                })
            }
            "task" => {
                let id = task_ref(req)?;
                Ok(match self.core()?.query(Query::EntityById(id)).await {
                    Ok(QueryResult::Task(t)) => {
                        let cbor =
                            sunrise_cbor::encode_canonical(&*t).map_err(|e| e.to_string())?;
                        json!({ "task": hex::encode(cbor) })
                    }
                    Ok(_) | Err(_) => json!({ "task": Value::Null }),
                })
            }
            "devices" => match self.core()?.query(Query::DeviceList).await {
                Ok(QueryResult::Devices(rows)) => Ok(json!({
                    "devices": rows.iter().map(|r| hex::encode(r.device_id)).collect::<Vec<_>>(),
                })),
                Ok(other) => Err(format!("expected Devices, got {other:?}")),
                Err(e) => Err(e.to_string()),
            },
            "status" => match self.core()?.query(Query::SyncStatus).await {
                Ok(QueryResult::SyncStatus(s)) => Ok(json!({
                    "state": format!("{:?}", s.state),
                    "outbox_pending": s.outbox_pending,
                })),
                Ok(other) => Err(format!("expected SyncStatus, got {other:?}")),
                Err(e) => Err(e.to_string()),
            },
            "events" => {
                let drained = EVENTS
                    .lock()
                    .map(|mut e| std::mem::take(&mut *e))
                    .unwrap_or_default();
                Ok(json!({ "events": drained }))
            }
            "close" => {
                if let Some(core) = self.core.take() {
                    core.shutdown().await;
                    drop(core);
                }
                Ok(json!({}))
            }
            other => Err(format!("unknown op `{other}`")),
        }
    }

    async fn init(&mut self, req: &Value) -> Result<Value, String> {
        if self.core.is_some() {
            return Err("already initialised".to_owned());
        }
        let root = bytes32(str_field(req, "root")?)?;
        let relay = str_field(req, "relay")?;
        let dir_a = PathBuf::from(str_field(req, "dir_a")?);
        let dir_b = PathBuf::from(str_field(req, "dir_b")?);

        let b = Core::open(
            config(dir_b, None),
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(root),
                paired: None,
            },
        )
        .await
        .map_err(|e| format!("open B: {e}"))?;
        // The baseline's own pairing: the same three messages its clients
        // run, in one process. Fresh device seeds, so A's id is its own.
        let mut seed_s = [0u8; 32];
        let mut seed_d = [0u8; 32];
        SystemRng.fill_bytes(&mut seed_s);
        SystemRng.fill_bytes(&mut seed_d);
        let payload = b
            .pair_device_in_process("baseline-a".into(), "test".into(), seed_s, seed_d)
            .map_err(|e| format!("pair A: {e}"))?;
        let device_b = b.device_id();
        let account_root = payload.vault_root;
        b.close().await.map_err(|e| format!("close B: {e}"))?;

        let a = Core::open(
            config(dir_a, Some(relay)),
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(payload.vault_root),
                paired: Some(Box::new(payload)),
            },
        )
        .await
        .map_err(|e| format!("open A: {e}"))?;
        let a = Arc::new(a);
        a.start_sync(factory(relay.to_owned()))
            .map_err(|e| e.to_string())?;
        let device_a = a.device_id();
        self.core = Some(a);
        Ok(json!({
            "device_a": hex::encode(device_a),
            "device_b": hex::encode(device_b),
            // The root both vaults are wrapped under, as the baseline's own
            // pairing reports it: the harness reopens both with this one.
            "root": hex::encode(account_root),
        }))
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let filter = tracing_subscriber::filter::filter_fn(|meta| {
        meta.target().starts_with("sunrise") || *meta.level() <= tracing::Level::WARN
    });
    let subscriber = tracing_subscriber::registry().with(Capture.with_filter(filter));
    // A second subscriber cannot already be installed in a fresh process.
    let _ = tracing::subscriber::set_global_default(subscriber);

    let mut driver = Driver { core: None };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let req: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            // Answered as an unknown op, so the caller still gets its one line.
            Err(e) => json!({ "op": format!("unparseable request: {e}") }),
        };
        let closing = req.get("op").and_then(Value::as_str) == Some("close");
        let mut resp = match driver.handle(&req).await {
            Ok(Value::Object(mut body)) => {
                body.insert("ok".to_owned(), Value::Bool(true));
                Value::Object(body)
            }
            Ok(other) => json!({ "ok": true, "value": other }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        if let Value::Object(body) = &mut resp {
            body.entry("ok").or_insert(Value::Bool(true));
        }
        if writeln!(stdout, "{resp}")
            .and_then(|()| stdout.flush())
            .is_err()
            || closing
        {
            break;
        }
    }
}
