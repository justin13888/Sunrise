//! The maintenance pass: account erasure, blob garbage collection, and the
//! pending-upload sweep.
//!
//! One function, [`run`], called on a timer by the serving binary and on
//! demand by `sunrise-server admin gc`. Every deadline it enforces is measured
//! against the clock it is handed, so a test drives it with an injected one and
//! an operator's dry run sees exactly what the next real pass would do.
//!
//! # Order
//!
//! Accounts first: erasing an account removes its tombstones and its whole
//! blob tree, so collecting its blobs one by one first would be wasted work.
//! Then tombstoned blobs, then abandoned uploads, then — only where the blob
//! root is one the operator configured — per-account directories whose account
//! no longer exists, which is what a crash between an erasure's commit and its
//! file deletion leaves behind.
//!
//! # Failure
//!
//! A pass never stops at the first failure. Each item that fails is logged as
//! `srv.maintenance.failed` and counted, and the rest go ahead: one unreadable
//! directory must not keep every other account's erasure waiting.

use std::path::Path;

use serde::Serialize;

use crate::api::blobs::{account_dir, delete_committed, COMMITTED, PENDING};
use crate::state::ServerState;
use crate::store::StoreError;

/// What a pass did, or with `dry_run` would have done.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Whether anything was deleted.
    pub dry_run: bool,
    /// Accounts erased: their rows, relay log and blob trees.
    pub accounts_erased: u64,
    /// Tombstoned blobs whose ciphertext was reclaimed.
    pub blobs_collected: u64,
    /// Abandoned uploads whose pending chunks were removed.
    pub uploads_swept: u64,
    /// Per-account directories with no account left, removed.
    pub orphans_swept: u64,
    /// Items that failed and were left for the next pass.
    pub failures: u64,
}

/// Run one pass at `now_ms`.
///
/// # Errors
/// A store query that fails before any item is reached. Per-item failures are
/// counted in [`Report::failures`] instead.
pub fn run(state: &ServerState, now_ms: u64, dry_run: bool) -> Result<Report, StoreError> {
    let retention = state.config.retention();
    let mut report = Report {
        dry_run,
        ..Report::default()
    };

    let due = state
        .store
        .accounts_due_for_erasure(now_ms.saturating_sub(retention.account_delete_grace_ms))?;
    for account_id in due {
        report.accounts_erased += 1;
        if !dry_run {
            if let Err(e) = erase_account(state, &account_id) {
                report.accounts_erased -= 1;
                failed(&mut report, "account_erase", &e);
            }
        }
    }

    let collectable = state.store.collectable_blobs(
        now_ms.saturating_sub(retention.gc_grace_ms),
        now_ms.saturating_sub(retention.device_active_window_ms),
    )?;
    for (account_id, blob) in collectable {
        report.blobs_collected += 1;
        if dry_run {
            continue;
        }
        let collected = delete_committed(&state.blob_root, &account_id, &blob)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                state
                    .store
                    .clear_tombstone(&account_id, &blob)
                    .map_err(|e| e.to_string())
            });
        match collected {
            Ok(()) => {
                state.metrics.incr("sunrise_blob_gc_deleted_total");
                tracing::info!(
                    ev = "srv.blob.gc_deleted",
                    account_h = %crate::logging::account_h(&account_id),
                    blob_h = %crate::logging::id_h(&blob),
                    "tombstoned blob collected"
                );
            }
            Err(e) => {
                report.blobs_collected -= 1;
                failed(&mut report, "blob_collect", &e);
            }
        }
    }

    let ttl = retention.pending_upload_ttl_ms;
    for upload in stale_children(&state.blob_root.join(PENDING), now_ms, ttl, 2) {
        report.uploads_swept += 1;
        if dry_run {
            continue;
        }
        match std::fs::remove_dir_all(&upload) {
            Ok(()) => tracing::info!(
                ev = "srv.blob.upload_swept",
                "abandoned upload swept: untouched past [storage] pending_upload_ttl_hours"
            ),
            Err(e) => {
                report.uploads_swept -= 1;
                failed(&mut report, "upload_sweep", &e.to_string());
            }
        }
    }

    // Only under a root the operator named: the default is a shared temp
    // directory, where another process's tree is not this store's orphan.
    if state.config.blob_root.is_some() {
        sweep_orphans(state, now_ms, ttl, dry_run, &mut report)?;
    }
    Ok(report)
}

/// Erase one account now: its rows and relay log in one transaction, then its
/// blob trees, then its channels in the in-memory ring.
///
/// # Errors
/// The store's failure, or the first directory that could not be removed. A
/// failure after the commit leaves files whose account is gone, which
/// [`run`]'s orphan sweep removes.
pub fn erase_account(state: &ServerState, account_id: &str) -> Result<(), String> {
    state
        .store
        .erase_account(account_id)
        .map_err(|e| e.to_string())?;
    state
        .relay
        .forget_account(crate::relay_log::account_key(account_id));
    for area in [PENDING, COMMITTED] {
        match std::fs::remove_dir_all(account_dir(&state.blob_root, area, account_id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    state.metrics.incr("sunrise_account_delete_total");
    tracing::info!(
        ev = "srv.account.delete_completed",
        account_h = %crate::logging::account_h(account_id),
        "account erased"
    );
    Ok(())
}

fn failed(report: &mut Report, what: &'static str, cause: &str) {
    report.failures += 1;
    tracing::warn!(
        ev = "srv.maintenance.failed",
        reason = what,
        cause = %cause,
        "maintenance item failed; the next pass retries it"
    );
}

/// Per-account directories under `pending/` and `committed/` whose account no
/// longer exists and which nothing has touched for `ttl_ms`.
///
/// The age check is what makes this safe against an account created after the
/// account list below was read: its directory is new.
fn sweep_orphans(
    state: &ServerState,
    now_ms: u64,
    ttl_ms: u64,
    dry_run: bool,
    report: &mut Report,
) -> Result<(), StoreError> {
    let live: std::collections::HashSet<String> = state
        .store
        .account_summaries()?
        .iter()
        .map(|a| hex::encode(crate::relay_log::account_key(&a.account_id)))
        .collect();
    for area in [PENDING, COMMITTED] {
        for dir in stale_children(&state.blob_root.join(area), now_ms, ttl_ms, 1) {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.len() != 32 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if live.contains(name) {
                continue;
            }
            report.orphans_swept += 1;
            if !dry_run {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    report.orphans_swept -= 1;
                    failed(report, "orphan_sweep", &e.to_string());
                }
            }
        }
    }
    Ok(())
}

/// Directories `depth` levels below `root` whose newest modification anywhere
/// inside is older than `ttl_ms` before `now_ms`.
///
/// The newest time anywhere in the tree, not the directory's own: writing a
/// chunk into an existing directory does not always move the directory's
/// mtime, and an upload with a chunk written a minute ago is not abandoned.
fn stale_children(root: &Path, now_ms: u64, ttl_ms: u64, depth: u32) -> Vec<std::path::PathBuf> {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..depth {
        level = level
            .iter()
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flat_map(|rd| rd.filter_map(Result::ok).map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
    }
    level
        .into_iter()
        .filter(|d| newest_mtime_ms(d).is_some_and(|t| now_ms.saturating_sub(t) > ttl_ms))
        .collect()
}

/// The newest modification time of `path` and everything under it, in ms
/// since the epoch.
fn newest_mtime_ms(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let own = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))?;
    if !meta.is_dir() {
        return Some(own);
    }
    let children = std::fs::read_dir(path).ok()?;
    Some(
        children
            .filter_map(Result::ok)
            .filter_map(|e| newest_mtime_ms(&e.path()))
            .fold(own, u64::max),
    )
}
