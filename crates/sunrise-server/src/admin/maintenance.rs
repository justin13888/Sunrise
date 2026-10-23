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

use serde::Serialize;

use crate::relay_log::account_key;
use crate::state::ServerState;
use crate::store::MetadataError;

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
pub async fn run(state: &ServerState, now_ms: u64, dry_run: bool) -> Result<Report, MetadataError> {
    let retention = state.config.retention();
    let mut report = Report {
        dry_run,
        ..Report::default()
    };

    let due = state
        .store
        .accounts_due_for_erasure(now_ms.saturating_sub(retention.account_delete_grace_ms))
        .await?;
    for account_id in due {
        report.accounts_erased += 1;
        if !dry_run {
            if let Err(e) = erase_account(state, &account_id).await {
                report.accounts_erased -= 1;
                failed(&mut report, "account_erase", &e);
            }
        }
    }

    let collectable = state
        .store
        .collectable_blobs(
            now_ms.saturating_sub(retention.gc_grace_ms),
            now_ms.saturating_sub(retention.device_active_window_ms),
        )
        .await?;
    for (account_id, blob) in collectable {
        report.blobs_collected += 1;
        if dry_run {
            continue;
        }
        let collected = match state
            .blobs
            .delete_committed(account_key(&account_id), blob)
            .await
        {
            Ok(()) => state
                .store
                .clear_tombstone(&account_id, &blob)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
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
    for upload in state
        .blobs
        .stale_uploads(now_ms, ttl)
        .await
        .unwrap_or_else(|e| {
            failed(&mut report, "upload_sweep", &e.to_string());
            Vec::new()
        })
    {
        report.uploads_swept += 1;
        if dry_run {
            continue;
        }
        match state.blobs.remove_upload(&upload).await {
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
        sweep_orphans(state, now_ms, ttl, dry_run, &mut report).await?;
    }
    Ok(report)
}

/// Erase one account now: its rows and relay log in one transaction, then its
/// blobs, then its channels in the in-memory ring.
///
/// # Errors
/// The store's failure, or the blob backend's. A failure after the commit
/// leaves blobs whose account is gone, which [`run`]'s orphan sweep removes.
pub async fn erase_account(state: &ServerState, account_id: &str) -> Result<(), String> {
    state
        .store
        .erase_account(account_id)
        .await
        .map_err(|e| e.to_string())?;
    state.relay.forget_account(account_key(account_id));
    // Each area manifests first, so an online backup never copies a manifest
    // naming chunks it did not copy.
    state
        .blobs
        .erase_account(account_key(account_id))
        .await
        .map_err(|e| e.to_string())?;
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

/// Account trees in either area whose account no longer exists and which
/// nothing has touched for `ttl_ms`.
///
/// The age check is what makes this safe against an account created after the
/// account list below was read: its tree is new.
async fn sweep_orphans(
    state: &ServerState,
    now_ms: u64,
    ttl_ms: u64,
    dry_run: bool,
    report: &mut Report,
) -> Result<(), MetadataError> {
    let live: std::collections::HashSet<[u8; 16]> = state
        .store
        .account_summaries()
        .await?
        .iter()
        .map(|a| account_key(&a.account_id))
        .collect();
    let trees = match state.blobs.stale_account_trees(now_ms, ttl_ms).await {
        Ok(trees) => trees,
        Err(e) => {
            failed(report, "orphan_sweep", &e.to_string());
            return Ok(());
        }
    };
    for tree in trees {
        if live.contains(&tree.account) {
            continue;
        }
        report.orphans_swept += 1;
        if !dry_run {
            if let Err(e) = state.blobs.remove_account_tree(tree).await {
                report.orphans_swept -= 1;
                failed(report, "orphan_sweep", &e.to_string());
            }
        }
    }
    Ok(())
}
