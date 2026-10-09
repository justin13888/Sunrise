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
//! backup <dest_dir>               a consistent copy of the database and blobs,
//!                                 taken while the relay runs
//! rekey <new_key_file>            re-encrypt the database under a new key,
//!                                 with the relay stopped
//! ```
//!
//! Output is a JSON document with `--json` and indented `key: value` lines
//! without it; both carry the same fields. Account ids appear, emails and OIDC
//! subjects never do. Exit status: 0 done, 1 the command failed (a `doctor`
//! check included), 2 a usage error, 78 a config that does not resolve or names
//! no data dir.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

use super::maintenance;
use crate::state::ServerState;
use crate::store::{MetadataError, Store};

/// Usage error.
const EX_USAGE: u8 = 2;
/// `EX_CONFIG`, as the serving binary uses it.
const EX_CONFIG: u8 = 78;
/// Anything else.
const EX_FAILURE: u8 = 1;

const USAGE: &str = "usage: sunrise-server admin [-c <path>] [--json] \
    <doctor | stats | gc (--dry-run|--now) | account list | account show <id> | \
    account delete <id> [--immediately] | device revoke <device_id> | backup <dest_dir> | \
    rekey <new_key_file>>";

/// How a command ended.
type Outcome = Result<Value, (u8, String)>;

/// Run `admin` with `args` (everything after the word `admin`), writing the
/// result to `out` and a failure's reason to `err`. Returns the exit status.
pub async fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
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

    let outcome = match open(&config_args) {
        Ok(state) => dispatch(&state, &words).await,
        Err(e) => Err(e),
    };
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

async fn dispatch(state: &ServerState, words: &[&str]) -> Outcome {
    let now_ms = state.clock.now_ms();
    match words {
        ["doctor"] => Ok(doctor(state).await),
        ["stats"] => stats(state).await,
        ["gc", flag] if *flag == "--dry-run" || *flag == "--now" => {
            let report = maintenance::run(state, now_ms, *flag == "--dry-run")
                .await
                .map_err(failure)?;
            serde_json::to_value(report).map_err(failure)
        }
        ["account", "list"] => {
            serde_json::to_value(state.store.account_summaries().await.map_err(failure)?)
                .map(|accounts| json!({ "accounts": accounts }))
                .map_err(failure)
        }
        ["account", "show", id] => {
            let summary = state.store.account_summary(id).await.map_err(failure)?;
            let summary = summary.ok_or_else(|| (EX_FAILURE, format!("no account {id}")))?;
            serde_json::to_value(summary).map_err(failure)
        }
        ["account", "delete", id] => {
            let requested = state
                .store
                .request_account_deletion(id, now_ms)
                .await
                .map_err(|e| match e {
                    MetadataError::NotFound => (EX_FAILURE, format!("no account {id}")),
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
            if state
                .store
                .account_summary(id)
                .await
                .map_err(failure)?
                .is_none()
            {
                return Err((EX_FAILURE, format!("no account {id}")));
            }
            maintenance::erase_account(state, id)
                .await
                .map_err(|e| (EX_FAILURE, e))?;
            Ok(json!({ "account_id": id, "erased": true }))
        }
        ["device", "revoke", device_id] => {
            let owner = state
                .store
                .device_owner(device_id)
                .await
                .map_err(failure)?
                .ok_or_else(|| (EX_FAILURE, format!("no device {device_id}")))?;
            state
                .store
                .revoke_device(&owner, device_id, now_ms)
                .await
                .map_err(|e| match e {
                    MetadataError::NotFound => {
                        (EX_FAILURE, format!("device {device_id} is already revoked"))
                    }
                    other => failure(other),
                })?;
            Ok(json!({ "device_id": device_id, "account_id": owner, "revoked_at_ms": now_ms }))
        }
        ["backup", dest] => backup(state, Path::new(dest)).await,
        ["rekey", new_key_file] => rekey(state, Path::new(new_key_file)),
        _ => Err((EX_USAGE, "unrecognised command".to_owned())),
    }
}

fn failure(e: impl std::fmt::Display) -> (u8, String) {
    (EX_FAILURE, e.to_string())
}

/// `self-hosting.md` §Testing the install, the checks that apply to a
/// single-binary SQLite relay behind a TLS-terminating proxy.
async fn doctor(state: &ServerState) -> Value {
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

    match state.store.as_sqlite().map(Store::quick_check) {
        Some(Ok(r)) if r == "ok" => check("database", "ok", "quick_check: ok".to_owned()),
        Some(Ok(r)) => check("database", "fail", format!("quick_check: {r}")),
        Some(Err(e)) => check("database", "fail", e.to_string()),
        None => check(
            "database",
            "skipped",
            "the metadata store is not SQLite".to_owned(),
        ),
    }

    let (status, detail) = encryption(state);
    check("encryption", status, detail);

    let data_dir = cfg
        .sqlite_path
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    match state
        .store
        .as_sqlite()
        .map(|db| db.probe_data_dir(10 * 1024 * 1024))
    {
        Some(Ok(())) => check(
            "storage",
            "ok",
            format!(
                "10 MiB written, fsynced, read back and removed in {}",
                data_dir.display()
            ),
        ),
        Some(Err(e)) => check("storage", "fail", format!("{}: {e}", data_dir.display())),
        None => check(
            "storage",
            "skipped",
            "the metadata store is not SQLite".to_owned(),
        ),
    }
    let blobs = state.blobs.location();
    match state.blobs.probe().await {
        Ok(()) => check("blob_root", "ok", format!("{blobs} is writable")),
        Err(e) => check("blob_root", "fail", format!("{blobs}: {e}")),
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

/// The `doctor` encryption check.
///
/// Opening the store already refused a key that is missing, wrong, or readable
/// by others, so reaching here with encryption on means the key opened it.
/// What is left to find is the plaintext copy the migration kept.
fn encryption(state: &ServerState) -> (&'static str, String) {
    let kept = state
        .config
        .sqlite_path
        .as_deref()
        .map(crate::store::pre_encryption_copy);
    match kept.filter(|k| k.exists()) {
        Some(k) => (
            "fail",
            format!(
                "{} is the plaintext database as it was before encryption: once the relay has \
                 run on the encrypted one, delete it (and any backup that holds it)",
                k.display()
            ),
        ),
        None if state.store.as_sqlite().is_some_and(Store::is_encrypted) => (
            "ok",
            "the database is SQLCipher-encrypted under [storage] key_file".to_owned(),
        ),
        None => (
            "skipped",
            "[storage] encrypt is off: the database is plaintext at rest".to_owned(),
        ),
    }
}

async fn stats(state: &ServerState) -> Outcome {
    let store = state.store.stats().await.map_err(failure)?;
    let usage = state.blobs.usage().await.map_err(failure)?;
    let mut value = serde_json::to_value(store).map_err(failure)?;
    value["blob_files"] = json!(usage.blob_files);
    value["blob_bytes"] = json!(usage.blob_bytes);
    value["pending_uploads"] = json!(usage.pending_uploads);
    Ok(value)
}

/// The database through SQLite's online backup API, then the blob tree with
/// its manifests last.
///
/// The database copy is one committed instant, under the live database's own
/// key, and the relay's writes proceed while it is taken
/// ([`crate::store::Store::backup_to`]). The blob copy is not one instant, and
/// is made consistent by order instead
/// ([`crate::blob::BlobBackend::backup_to`]): the manifests present are listed
/// first, every other object is copied, and the listed manifests are copied
/// last, each only if it still exists. Every removal takes a manifest before
/// its chunks — blob collection, account erasure, and the orphan sweep alike —
/// so a blob removed during the copy is left without one.
///
/// A blob with no manifest in the backup reads as never uploaded. The only
/// such blobs are ones finalized after the database copy was taken, which no
/// op in it can name; ones collected during it, whose tombstones are in it;
/// and ones of an account erased during it, which the backup still holds and
/// a restore brings back with those blobs missing. Blobs come after the
/// database for that reason: a client finalizes a blob before it publishes
/// the op naming it.
async fn backup(state: &ServerState, dest: &Path) -> Outcome {
    let db = sqlite(state, "backup")?;
    if dest.exists() {
        return Err((
            EX_FAILURE,
            format!("{} already exists; name a new directory", dest.display()),
        ));
    }
    std::fs::create_dir_all(dest).map_err(failure)?;
    let database = dest.join("sunrise.db");
    db.backup_to(&database).map_err(failure)?;
    let copied = state
        .blobs
        .backup_to(&dest.join("blobs"))
        .await
        .map_err(failure)?;
    Ok(json!({
        "database": database.display().to_string(),
        "encrypted": db.is_encrypted(),
        "blob_files": copied.files,
        "blob_bytes": copied.bytes,
    }))
}

/// The SQLite store `command` works on, or the refusal for a metadata store
/// that is not one: an online file copy and a re-key are things only a
/// single-file database has.
fn sqlite<'a>(state: &'a ServerState, command: &str) -> Result<&'a Store, (u8, String)> {
    state.store.as_sqlite().ok_or_else(|| {
        (
            EX_FAILURE,
            format!(
                "{command} works on a SQLite database, and this relay's metadata store is not one"
            ),
        )
    })
}

/// `rekey <new_key_file>`: re-encrypt the database under the key in
/// `new_key_file`, with the relay stopped.
///
/// The new key file is held to the same rules as `[storage] key_file` — owner
/// only, outside the data dir — because it is about to become it. The command
/// does not edit the config: the operator points `key_file` at the new file
/// before the next start, and the old key no longer opens the database.
fn rekey(state: &ServerState, new_key_file: &Path) -> Outcome {
    let mut probe = (*state.config).clone();
    probe.sqlite_encrypt = true;
    probe.sqlite_key_file = Some(new_key_file.to_owned());
    let new = probe
        .sqlite_key()
        .map_err(failure)?
        .ok_or_else(|| (EX_FAILURE, "no key was read".to_owned()))?;
    sqlite(state, "rekey")?.rekey(&new).map_err(failure)?;
    Ok(json!({
        "rekeyed": true,
        "key_file": new_key_file.display().to_string(),
        "next": "set [storage] key_file to this file before the relay starts again",
    }))
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
