//! Event-driven waits: a short poll interval inside a timeout, never a bare
//! sleep used for correctness.

use std::time::Duration;

use crate::canonical::{canonical_streams, canonical_task_by_id, canonical_tasks, CanonicalTask};
use sunrise_core::{Core, Query, QueryResult};
use sunrise_id::EntityRef;

/// Poll interval used inside every timeout loop.
const POLL: Duration = Duration::from_millis(25);

/// The current live [`sunrise_sync::SyncState`] of `core`.
async fn sync_state(core: &Core) -> sunrise_sync::SyncState {
    match core.query(Query::SyncStatus).await.expect("sync status") {
        QueryResult::SyncStatus(s) => s.state,
        other => panic!("expected SyncStatus, got {other:?}"),
    }
}

/// Wait until `core`'s driver reports [`sunrise_sync::SyncState::Live`].
pub async fn wait_live(core: &Core, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            if sync_state(core).await == sunrise_sync::SyncState::Live {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for Live");
}

/// Wait until every core in `cores` holds a `DeviceFeatures` naming
/// `core.field_ops` from each other device of the vault, so its next entity
/// write makes the vault require the feature and is a `Patch`
/// ([ADR-0065](../../../docs/11-adr/0065-copy-on-write-entity-versions.md) §7).
pub async fn wait_field_ops(cores: &[&Core], timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            let ready = cores.iter().all(|c| {
                c.devices_lacking_feature(sunrise_core::FIELD_OPS_FEATURE)
                    .is_ok_and(|lacking| lacking.is_empty())
            });
            if ready {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for every device to advertise core.field_ops");
}

/// Wait until `core`'s persistent outbox has fully drained (`outbox_pending == 0`).
pub async fn wait_pending_zero(core: &Core, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            let pending = match core.query(Query::SyncStatus).await.expect("sync status") {
                QueryResult::SyncStatus(s) => s.outbox_pending,
                other => panic!("expected SyncStatus, got {other:?}"),
            };
            if pending == 0 {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for outbox to drain");
}

/// Wait until both replicas agree on one task, tombstone included.
///
/// # Panics
/// On timeout, reporting the last two projections seen.
pub async fn wait_task_converges(a: &Core, b: &Core, id: EntityRef, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let ta = canonical_task_by_id(a, id).await;
        let tb = canonical_task_by_id(b, id).await;
        if ta.is_some() && ta == tb {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!(
        "task {id} never converged:\n  a = {:?}\n  b = {:?}",
        canonical_task_by_id(a, id).await,
        canonical_task_by_id(b, id).await
    );
}

/// Wait until both replicas expose identical canonical task tables of exactly
/// `expected_len` rows, then return that converged table.
///
/// Requiring the length guards against a premature match on the trivially-equal
/// empty state before any op has propagated.
pub async fn wait_tasks_converge(
    a: &Core,
    b: &Core,
    expected_len: usize,
    timeout: Duration,
) -> Vec<CanonicalTask> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(timeout, async {
        loop {
            last_a = canonical_tasks(a).await;
            last_b = canonical_tasks(b).await;
            if last_a.len() == expected_len && last_a == last_b {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(converged) = res else {
        panic!(
            "tasks did not converge to {expected_len} rows in time\n  A ({}): {last_a:#?}\n  B ({}): {last_b:#?}",
            last_a.len(),
            last_b.len()
        );
    };
    converged
}

/// Wait until both replicas expose identical canonical stream tables of exactly
/// `expected_len` rows.
pub async fn wait_streams_converge(a: &Core, b: &Core, expected_len: usize, timeout: Duration) {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(timeout, async {
        loop {
            last_a = canonical_streams(a).await;
            last_b = canonical_streams(b).await;
            if last_a.len() == expected_len && last_a == last_b {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(()) = res else {
        panic!(
            "streams did not converge to {expected_len} rows in time\n  A: {last_a:#?}\n  B: {last_b:#?}"
        );
    };
}
