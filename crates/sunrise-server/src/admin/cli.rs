//! `sunrise-server admin [-c <path>] [--json] <command>`.
//!
//! ```text
//! doctor                          check this install
//! stats                           counts across the database and the blob tree
//! gc --dry-run | --now            report, or run, one maintenance pass
//! account list                    every account
//! account show <account_id>       one account
//! account delete <account_id> [--immediately]
//!                                 mark it for deletion, or erase it now
//! device revoke <device_id>       revoke a device row
//! backup <dest_dir>               a consistent copy of the database and blobs
//! ```
//!
//! Output is a JSON document with `--json` and indented `key: value` lines
//! without it; both carry the same fields. Account ids appear, emails and OIDC
//! subjects never do. Exit status: 0 done, 1 the command failed (a `doctor`
//! check included), 2 a usage error, 78 a config that does not resolve or names
//! no data dir.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::maintenance;
use crate::state::ServerState;

/// Usage error.
const EX_USAGE: u8 = 2;
/// `EX_CONFIG`, as the serving binary uses it.
const EX_CONFIG: u8 = 78;
/// Anything else.
const EX_FAILURE: u8 = 1;

const USAGE: &str = "usage: sunrise-server admin [-c <path>] [--json] \
    <doctor | stats | gc (--dry-run|--now) | account list | account show <id> | \
    account delete <id> [--immediately] | device revoke <device_id> | backup <dest_dir>>";

/// How a command ended.
type Outcome = Result<Value, (u8, String)>;

/// Run `admin` with `args` (everything after the word `admin`), writing the
/// result to `out` and a failure's reason to `err`. Returns the exit status.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let mut config_args = Vec::new();
    let mut words = Vec::new();
    let mut json = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json = true,
            "-c" | "--config" => {
                config_args.push(arg.clone());
                if let Some(v) = it.next() {
                    config_args.push(v.clone());
                }
            }
            a if a.starts_with("--config=") => config_args.push(arg.clone()),
            _ => words.push(arg.as_str()),
        }
    }

    let outcome = open(&config_args).and_then(|state| dispatch(&state, &words));
    match outcome {
        Ok(value) => {
            let text = if json {
                serde_json::to_string_pretty(&value).unwrap_or_default()
            } else {
                render(&value, 0)
            };
            let _ = writeln!(out, "{}", text.trim_end());
            // A `doctor` that found a failing check is a failed command whose
            // report is still worth printing.
            if value.get("ok") == Some(&Value::Bool(false)) {
                EX_FAILURE
            } else {
                0
            }
        }
        Err((code, message)) => {
            let _ = writeln!(err, "admin: {message}");
            if code == EX_USAGE {
                let _ = writeln!(err, "{USAGE}");
            }
            code
        }
    }
}

/// Resolve the config as the server does and open its store.
fn open(config_args: &[String]) -> Result<ServerState, (u8, String)> {
    let cfg = crate::config::load(config_args).map_err(|e| (EX_CONFIG, e.to_string()))?;
    if cfg.sqlite_path.is_none() {
        return Err((
            EX_CONFIG,
            "no [storage] data_dir is configured: the server would run on an in-memory store, \
             and there is nothing on disk to administer"
                .to_owned(),
        ));
    }
    ServerState::try_new(cfg).map_err(|e| (EX_CONFIG, e.to_string()))
}

fn dispatch(state: &ServerState, words: &[&str]) -> Outcome {
    let now_ms = state.clock.now_ms();
    match words {
        ["doctor"] => Ok(doctor(state)),
        ["stats"] => stats(state),
        ["gc", flag] if *flag == "--dry-run" || *flag == "--now" => {
            let report = maintenance::run(state, now_ms, *flag == "--dry-run").map_err(failure)?;
            serde_json::to_value(report).map_err(failure)
        }
        ["account", "list"] => {
            serde_json::to_value(state.store.account_summaries().map_err(failure)?)
                .map(|accounts| json!({ "accounts": accounts }))
                .map_err(failure)
        }
        ["account", "show", id] => {
            let summary = state.store.account_summary(id).map_err(failure)?;
            let summary = summary.ok_or_else(|| (EX_FAILURE, format!("no account {id}")))?;
            serde_json::to_value(summary).map_err(failure)
        }
        ["account", "delete", id] => {
            let requested =
                state
                    .store
                    .request_account_deletion(id, now_ms)
                    .map_err(|e| match e {
                        crate::store::StoreError::NotFound => {
                            (EX_FAILURE, format!("no account {id}"))
                        }
                        other => failure(other),
                    })?;
            let grace = state.config.retention().account_delete_grace_ms;
            Ok(json!({
                "account_id": id,
                "requested_at_ms": requested,
                "erase_after_ms": requested.saturating_add(grace),
            }))
        }
        ["account", "delete", id, "--immediately"] => {
            if state.store.account_summary(id).map_err(failure)?.is_none() {
                return Err((EX_FAILURE, format!("no account {id}")));
            }
            maintenance::erase_account(state, id).map_err(|e| (EX_FAILURE, e))?;
            Ok(json!({ "account_id": id, "erased": true }))
        }
        ["device", "revoke", device_id] => {
            let owner = state
                .store
                .device_owner(device_id)
                .map_err(failure)?
                .ok_or_else(|| (EX_FAILURE, format!("no device {device_id}")))?;
            state
                .store
                .revoke_device(&owner, device_id, now_ms)
                .map_err(|e| match e {
                    crate::store::StoreError::NotFound => {
                        (EX_FAILURE, format!("device {device_id} is already revoked"))
                    }
                    other => failure(other),
                })?;
            Ok(json!({ "device_id": device_id, "account_id": owner, "revoked_at_ms": now_ms }))
        }
        ["backup", dest] => backup(state, Path::new(dest)),
        _ => Err((EX_USAGE, "unrecognised command".to_owned())),
    }
}

fn failure(e: impl std::fmt::Display) -> (u8, String) {
    (EX_FAILURE, e.to_string())
}

/// `self-hosting.md` §Testing the install, the checks that apply to a
/// single-binary SQLite relay behind a TLS-terminating proxy.
fn doctor(state: &ServerState) -> Value {
    let cfg = &state.config;
    let mut checks = Vec::new();
    let mut check = |name: &str, status: &str, detail: String| {
        checks.push(json!({ "check": name, "status": status, "detail": detail }));
    };

    let single_tenant = cfg.oidc_issuer.is_none() || cfg.oidc_client_id.is_none();
    match cfg.validate(single_tenant) {
        Ok(()) => check("config", "ok", "the config validates".to_owned()),
        Err(e) => check("config", "fail", e.to_string()),
    }

    let quick: Result<String, rusqlite::Error> =
        state
            .store
            .conn
            .lock()
            .query_row("PRAGMA quick_check(1)", [], |r| r.get(0));
    match quick {
        Ok(r) if r == "ok" => check("database", "ok", "quick_check: ok".to_owned()),
        Ok(r) => check("database", "fail", format!("quick_check: {r}")),
        Err(e) => check("database", "fail", e.to_string()),
    }

    let data_dir = cfg
        .sqlite_path
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    match write_read_back(data_dir, 10 * 1024 * 1024) {
        Ok(()) => check(
            "storage",
            "ok",
            format!(
                "10 MiB written, fsynced, read back and removed in {}",
                data_dir.display()
            ),
        ),
        Err(e) => check("storage", "fail", format!("{}: {e}", data_dir.display())),
    }
    match write_read_back(&state.blob_root, 1) {
        Ok(()) => check(
            "blob_root",
            "ok",
            format!("{} is writable", state.blob_root.display()),
        ),
        Err(e) => check(
            "blob_root",
            "fail",
            format!("{}: {e}", state.blob_root.display()),
        ),
    }
    check(
        "free_space",
        "skipped",
        "the free-space ratio needs statvfs, which this binary cannot call without unsafe code; \
         read it with df"
            .to_owned(),
    );
    match &cfg.push.apns {
        // `ServerState::try_new` already built the provider, which refuses a
        // key it cannot sign with, so reaching here means it loaded.
        Some(apns) => check(
            "push",
            "ok",
            format!("APNs ({:?}) key loads and signs", apns.environment),
        ),
        None => check(
            "push",
            "skipped",
            "no [push] provider is configured".to_owned(),
        ),
    }
    check(
        "tls",
        "skipped",
        "the relay serves plain HTTP; TLS terminates at the reverse proxy".to_owned(),
    );
    check(
        "protocol",
        "ok",
        format!(
            "wire {}, doc schema {}, crypto suite {}",
            sunrise_cbor::version::WIRE_PROTO_V,
            sunrise_cbor::version::DOC_SCHEMA_V,
            sunrise_cbor::version::CRYPTO_SUITE_V
        ),
    );

    let ok = checks.iter().all(|c| c["status"] != "fail");
    json!({ "ok": ok, "checks": checks })
}

/// Write `len` bytes under `dir`, fsync, read them back, compare, and remove.
fn write_read_back(dir: &Path, len: usize) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(".sunrise-doctor.tmp");
    // A pattern rather than zeros, so a filesystem that hands back a zeroed
    // page for an unwritten extent does not pass.
    let data: Vec<u8> = (0..=250u8).cycle().take(len).collect();
    let result = (|| {
        {
            let mut f = std::fs::File::create(&path)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        if std::fs::read(&path)? != data {
            return Err(std::io::Error::other(
                "read back differs from what was written",
            ));
        }
        Ok(())
    })();
    let removed = std::fs::remove_file(&path);
    result.and(removed)
}

fn stats(state: &ServerState) -> Outcome {
    let store = state.store.stats().map_err(failure)?;
    let (blob_files, blob_bytes) = tree_size(&state.blob_root.join(crate::api::blobs::COMMITTED));
    let pending_uploads = std::fs::read_dir(state.blob_root.join(crate::api::blobs::PENDING))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|account| std::fs::read_dir(account.path()).ok())
        .map(|uploads| uploads.filter_map(Result::ok).count() as u64)
        .sum::<u64>();
    let mut value = serde_json::to_value(store).map_err(failure)?;
    value["blob_files"] = json!(blob_files);
    value["blob_bytes"] = json!(blob_bytes);
    value["pending_uploads"] = json!(pending_uploads);
    Ok(value)
}

/// Files and bytes under `root`, recursively. Absent is empty.
fn tree_size(root: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    entries
        .filter_map(Result::ok)
        .fold((0, 0), |(files, bytes), e| match e.metadata() {
            Ok(m) if m.is_dir() => {
                let (f, b) = tree_size(&e.path());
                (files + f, bytes + b)
            }
            Ok(m) => (files + 1, bytes + m.len()),
            Err(_) => (files, bytes),
        })
}

/// `VACUUM INTO` for the database, then a copy of the blob tree.
///
/// The database copy is one instant. The blob copy is not, and does not need
/// to be: chunk files are written to a temporary name and renamed into place,
/// so every file copied is whole, and a blob finalized during the copy is
/// either in it completely or missing its manifest, which a restore reads as
/// "never uploaded". Temporary files are skipped.
fn backup(state: &ServerState, dest: &Path) -> Outcome {
    if dest.exists() {
        return Err((
            EX_FAILURE,
            format!("{} already exists; name a new directory", dest.display()),
        ));
    }
    std::fs::create_dir_all(dest).map_err(failure)?;
    let database = dest.join("sunrise.db");
    state.store.snapshot_to(&database).map_err(failure)?;
    let (files, bytes) = copy_tree(&state.blob_root, &dest.join("blobs")).map_err(failure)?;
    Ok(json!({
        "database": database.display().to_string(),
        "blob_files": files,
        "blob_bytes": bytes,
    }))
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<(u64, u64)> {
    std::fs::create_dir_all(to)?;
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok((0, 0));
    };
    let (mut files, mut bytes) = (0, 0);
    for entry in entries {
        let entry = entry?;
        let target: PathBuf = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let (f, b) = copy_tree(&entry.path(), &target)?;
            files += f;
            bytes += b;
        } else if kind.is_file() && !entry.file_name().to_string_lossy().ends_with(".tmp") {
            bytes += std::fs::copy(entry.path(), &target)?;
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// Indented `key: value` lines; arrays of objects as one block per element.
fn render(value: &Value, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v {
                Value::Object(_) | Value::Array(_) => {
                    format!("{pad}{k}:\n{}", render(v, indent + 1))
                }
                _ => format!("{pad}{k}: {}\n", scalar(v)),
            })
            .collect(),
        Value::Array(items) if items.is_empty() => format!("{pad}(none)\n"),
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::Object(_) => format!("{pad}-\n{}", render(v, indent + 1)),
                _ => format!("{pad}- {}\n", scalar(v)),
            })
            .collect(),
        _ => format!("{pad}{}\n", scalar(value)),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_owned(),
        other => other.to_string(),
    }
}
