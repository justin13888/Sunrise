//! The checks every [`MetadataStore`] must pass, written once and run against
//! each implementation: [`Store`] in this module's tests, and a shared backend
//! in its own. Also [`Unreachable`], the backend that fails every call, for the
//! tests of what a caller does with a failure.

use std::collections::HashMap;
use std::time::Duration;

use super::{
    Account, AccountSummary, DeclaredCursor, Device, MetadataError, MetadataStore, NewDevice,
    NewTombstone, StoreStats,
};
use crate::auth::Subject;
use crate::relay::{CursorGap, FrameHead, StreamKey};
use crate::relay_log::{account_key, Appended, DurableCaps, Replay};

/// A backend that never answers, standing in for a shared one that lost its
/// connection.
#[derive(Debug)]
pub(crate) struct Unreachable;

fn down() -> MetadataError {
    MetadataError::Unavailable("connection refused".to_owned())
}

#[async_trait::async_trait]
impl MetadataStore for Unreachable {
    async fn resolve_account(
        &self,
        _: &Subject,
        _: bool,
        _: u64,
    ) -> Result<Account, MetadataError> {
        Err(down())
    }
    async fn account_exists(&self, _: &Subject) -> Result<bool, MetadataError> {
        Err(down())
    }
    async fn account(&self, _: &str) -> Result<Option<Account>, MetadataError> {
        Err(down())
    }
    async fn set_identity(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: u64,
    ) -> Result<Account, MetadataError> {
        Err(down())
    }
    async fn recovery_blob(&self, _: &str) -> Result<Option<String>, MetadataError> {
        Err(down())
    }
    async fn register_device(
        &self,
        _: &str,
        _: &NewDevice,
        _: u64,
    ) -> Result<Device, MetadataError> {
        Err(down())
    }
    async fn list_devices(&self, _: &str) -> Result<Vec<Device>, MetadataError> {
        Err(down())
    }
    async fn active_device_count(&self, _: &str) -> Result<u32, MetadataError> {
        Err(down())
    }
    async fn active_device(&self, _: &str, _: &str) -> Result<Option<Device>, MetadataError> {
        Err(down())
    }
    async fn revoke_device(&self, _: &str, _: &str, _: u64) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn revoke_devices_by_vault_id(
        &self,
        _: &str,
        _: &str,
        _: u64,
    ) -> Result<usize, MetadataError> {
        Err(down())
    }
    async fn touch_device(&self, _: &str, _: u64) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn upsert_push_token(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: u64,
    ) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn push_tokens(&self, _: &str) -> Result<Vec<(String, String)>, MetadataError> {
        Err(down())
    }
    async fn push_targets(&self, _: &str, _: &str) -> Result<Vec<(String, String)>, MetadataError> {
        Err(down())
    }
    async fn push_target(&self, _: &str, _: &str) -> Result<Option<String>, MetadataError> {
        Err(down())
    }
    async fn delete_push_token(&self, _: &str, _: &str, _: &str) -> Result<bool, MetadataError> {
        Err(down())
    }
    async fn put_delete_token(&self, _: &str, _: &[u8; 32], _: u64) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn consume_delete_token(
        &self,
        _: &str,
        _: &[u8; 32],
        _: u64,
    ) -> Result<bool, MetadataError> {
        Err(down())
    }
    async fn request_account_deletion(&self, _: &str, _: u64) -> Result<u64, MetadataError> {
        Err(down())
    }
    async fn account_deletion_requested(&self, _: &str) -> Result<Option<u64>, MetadataError> {
        Err(down())
    }
    async fn accounts_due_for_erasure(&self, _: u64) -> Result<Vec<String>, MetadataError> {
        Err(down())
    }
    async fn erase_account(&self, _: &str) -> Result<bool, MetadataError> {
        Err(down())
    }
    async fn tombstone_blob(&self, _: &str, _: &NewTombstone, _: u64) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn clear_tombstone(&self, _: &str, _: &[u8; 16]) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn record_cursors(
        &self,
        _: &str,
        _: &[DeclaredCursor],
        _: u64,
    ) -> Result<(), MetadataError> {
        Err(down())
    }
    async fn collectable_blobs(
        &self,
        _: u64,
        _: u64,
    ) -> Result<Vec<(String, [u8; 16])>, MetadataError> {
        Err(down())
    }
    async fn account_summaries(&self) -> Result<Vec<AccountSummary>, MetadataError> {
        Err(down())
    }
    async fn account_summary(&self, _: &str) -> Result<Option<AccountSummary>, MetadataError> {
        Err(down())
    }
    async fn device_owner(&self, _: &str) -> Result<Option<String>, MetadataError> {
        Err(down())
    }
    async fn stats(&self) -> Result<StoreStats, MetadataError> {
        Err(down())
    }
    async fn relay_append(
        &self,
        _: StreamKey,
        _: &[u8],
        _: &[FrameHead],
        _: Option<&[u8; 32]>,
        _: u64,
        _: u64,
        _: DurableCaps,
    ) -> Result<Appended, MetadataError> {
        Err(down())
    }
    async fn relay_replay(
        &self,
        _: StreamKey,
        _: &HashMap<[u8; 16], u64>,
    ) -> Result<(Vec<Vec<u8>>, Vec<CursorGap>), MetadataError> {
        Err(down())
    }
    async fn relay_replay_after(
        &self,
        _: StreamKey,
        _: u64,
        _: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, MetadataError> {
        Err(down())
    }
    async fn relay_device_heads(
        &self,
        _: StreamKey,
    ) -> Result<HashMap<[u8; 16], u64>, MetadataError> {
        Err(down())
    }
    async fn relay_len(&self, _: StreamKey) -> Result<usize, MetadataError> {
        Err(down())
    }
    async fn ping(&self, _: Duration) -> Result<(), MetadataError> {
        Err(down())
    }
}

const NOW: u64 = 1_704_067_200_000;
const DEV: [u8; 16] = [0x22; 16];

fn subject(sub: &str) -> Subject {
    Subject::new("https://idp.example", sub)
}

fn new_device(nickname: &str) -> NewDevice {
    NewDevice {
        device_pub_s: format!("key-{nickname}"),
        device_pub_d: None,
        device_cert: None,
        vault_device_id: None,
        nickname: nickname.to_owned(),
        platform: "linux".to_owned(),
        app_version: None,
    }
}

fn unbounded() -> DurableCaps {
    DurableCaps {
        max_bytes: u64::MAX,
        max_age_ms: u64::MAX,
    }
}

fn head(seq: u64) -> Vec<FrameHead> {
    vec![FrameHead {
        device_id: DEV,
        max_seq: seq,
    }]
}

/// Run every check against a backend that starts empty.
pub(crate) async fn run(store: &dyn MetadataStore) {
    accounts(store).await;
    revocation_is_atomic_and_scoped(store).await;
    relay_log_is_ordered_and_evicts_oldest_first(store).await;
    erasure_is_whole_and_touches_no_other_account(store).await;
    store
        .ping(Duration::from_secs(2))
        .await
        .expect("an idle backend answers its probe");
}

/// A subject resolves to one account, every time; an unknown one is refused
/// while sign-up is off, and a known one is not.
async fn accounts(store: &dyn MetadataStore) {
    assert!(matches!(
        store.resolve_account(&subject("carol"), false, NOW).await,
        Err(MetadataError::SignupDisabled)
    ));
    let a = store
        .resolve_account(&subject("carol"), true, NOW)
        .await
        .unwrap();
    let b = store
        .resolve_account(&subject("carol"), false, NOW + 1)
        .await
        .unwrap();
    assert_eq!(a.account_id, b.account_id);
    assert!(store.account_exists(&subject("carol")).await.unwrap());

    store
        .set_identity(&a.account_id, "PUB_S", "PUB_D", Some("first"), NOW)
        .await
        .unwrap();
    assert!(matches!(
        store
            .set_identity(&a.account_id, "PUB_S", "PUB_D", Some("second"), NOW)
            .await,
        Err(MetadataError::RecoveryBlobExists)
    ));
    assert_eq!(
        store.recovery_blob(&a.account_id).await.unwrap().as_deref(),
        Some("first"),
        "a refused recovery blob wrote nothing"
    );
}

/// One revoke hides the device, drops its push tokens and keeps its row as
/// revoked, together; another account cannot revoke or read it.
async fn revocation_is_atomic_and_scoped(store: &dyn MetadataStore) {
    let alice = store
        .resolve_account(&subject("alice"), true, NOW)
        .await
        .unwrap();
    let bob = store
        .resolve_account(&subject("bob"), true, NOW)
        .await
        .unwrap();
    let d = store
        .register_device(&alice.account_id, &new_device("laptop"), NOW)
        .await
        .unwrap();
    store
        .upsert_push_token(&d.device_id, "fcm", "tok", NOW)
        .await
        .unwrap();

    assert!(matches!(
        store
            .revoke_device(&bob.account_id, &d.device_id, NOW)
            .await,
        Err(MetadataError::NotFound)
    ));
    assert!(store
        .active_device(&bob.account_id, &d.device_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.push_tokens(&d.device_id).await.unwrap().len(),
        1,
        "a refused revoke dropped nothing"
    );

    store
        .revoke_device(&alice.account_id, &d.device_id, NOW + 1)
        .await
        .unwrap();
    assert!(store
        .active_device(&alice.account_id, &d.device_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.active_device_count(&alice.account_id).await.unwrap(),
        0
    );
    assert!(
        store.push_tokens(&d.device_id).await.unwrap().is_empty(),
        "a revoked device must stop being wakeable in the same unit of work"
    );
    let listed = store.list_devices(&alice.account_id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked);
    assert_eq!(listed[0].revoked_at_ms, Some(NOW + 1));
    assert!(matches!(
        store
            .revoke_device(&alice.account_id, &d.device_id, NOW + 2)
            .await,
        Err(MetadataError::NotFound)
    ));
}

/// Frames replay in append order, channel by channel; a duplicate batch is
/// not stored twice; the size bound evicts oldest first and a cursor behind
/// what it evicted is reported as a gap, and one past it is not.
async fn relay_log_is_ordered_and_evicts_oldest_first(store: &dyn MetadataStore) {
    let key: StreamKey = ([0xa1; 16], [0x11; 16]);
    let other: StreamKey = ([0xa1; 16], [0x99; 16]);
    for seq in 1..=3u8 {
        store
            .relay_append(key, &[seq; 8], &head(seq.into()), None, 0, NOW, unbounded())
            .await
            .unwrap();
    }
    store
        .relay_append(other, b"theirs", &head(1), None, 0, NOW, unbounded())
        .await
        .unwrap();
    let (frames, gaps) = store
        .relay_replay_after(key, 0, &HashMap::new())
        .await
        .unwrap();
    assert!(gaps.is_empty());
    assert_eq!(
        frames.iter().map(|(_, b)| b[0]).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "append order, and nothing of another channel"
    );
    assert!(
        frames.windows(2).all(|w| w[0].0 < w[1].0),
        "frame ids rise in append order"
    );
    let (after, _) = store
        .relay_replay_after(key, frames[0].0, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(
        after.len(),
        2,
        "resuming after an id skips it and what came before"
    );

    let ops_h = [7u8; 32];
    let first = store
        .relay_append(
            key,
            b"batch",
            &head(4),
            Some(&ops_h),
            9,
            NOW + 5,
            unbounded(),
        )
        .await
        .unwrap();
    let again = store
        .relay_append(
            key,
            b"batch",
            &head(4),
            Some(&ops_h),
            9,
            NOW + 6,
            unbounded(),
        )
        .await
        .unwrap();
    assert!(matches!(first, Appended::Fresh { .. }));
    assert!(
        matches!(again, Appended::Duplicate { first_seen_ms } if first_seen_ms == NOW + 5),
        "{again:?}"
    );
    assert_eq!(store.relay_len(key).await.unwrap(), 4);

    let bounded: StreamKey = ([0xb2; 16], [0x11; 16]);
    let caps = DurableCaps {
        max_bytes: 20,
        max_age_ms: u64::MAX,
    };
    for seq in 1..=5u8 {
        store
            .relay_append(bounded, &[seq; 8], &head(seq.into()), None, 0, NOW, caps)
            .await
            .unwrap();
    }
    assert_eq!(
        store.relay_len(bounded).await.unwrap(),
        2,
        "20 bytes hold two"
    );
    let behind: HashMap<[u8; 16], u64> = [(DEV, 1)].into();
    let (frames, gaps) = store.relay_replay(bounded, &behind).await.unwrap();
    assert_eq!(frames, vec![vec![4; 8], vec![5; 8]], "oldest went first");
    assert_eq!(gaps.len(), 1);
    assert_eq!(
        (gaps[0].device_id, gaps[0].cursor, gaps[0].evicted_through),
        (DEV, 1, 3)
    );
    let caught_up: HashMap<[u8; 16], u64> = [(DEV, 5)].into();
    let (frames, gaps) = store.relay_replay(bounded, &caught_up).await.unwrap();
    assert!(frames.is_empty() && gaps.is_empty());
    assert_eq!(
        store.relay_device_heads(bounded).await.unwrap().get(&DEV),
        Some(&5),
        "an evicted head is still a head"
    );
}

/// Erasing an account removes its rows and its relay frames together, and
/// leaves another account's untouched.
async fn erasure_is_whole_and_touches_no_other_account(store: &dyn MetadataStore) {
    let gone = store
        .resolve_account(&subject("dave"), true, NOW)
        .await
        .unwrap();
    let kept = store
        .resolve_account(&subject("erin"), true, NOW)
        .await
        .unwrap();
    let mut channels = Vec::new();
    for account in [&gone, &kept] {
        let d = store
            .register_device(&account.account_id, &new_device("phone"), NOW)
            .await
            .unwrap();
        store
            .upsert_push_token(&d.device_id, "apns", "tok", NOW)
            .await
            .unwrap();
        let channel = (account_key(&account.account_id), [0x33; 16]);
        store
            .relay_append(channel, b"frame", &head(1), None, 0, NOW, unbounded())
            .await
            .unwrap();
        channels.push((d, channel));
    }
    store
        .request_account_deletion(&gone.account_id, NOW)
        .await
        .unwrap();
    assert_eq!(
        store.accounts_due_for_erasure(NOW).await.unwrap(),
        vec![gone.account_id.clone()]
    );

    assert!(store.erase_account(&gone.account_id).await.unwrap());
    assert!(
        !store.erase_account(&gone.account_id).await.unwrap(),
        "nothing left to erase"
    );
    assert!(store.account(&gone.account_id).await.unwrap().is_none());
    assert!(store
        .list_devices(&gone.account_id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .push_tokens(&channels[0].0.device_id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(store.relay_len(channels[0].1).await.unwrap(), 0);

    assert!(store.account(&kept.account_id).await.unwrap().is_some());
    assert_eq!(store.list_devices(&kept.account_id).await.unwrap().len(), 1);
    assert_eq!(
        store
            .push_tokens(&channels[1].0.device_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.relay_len(channels[1].1).await.unwrap(), 1);
}

#[cfg(test)]
mod tests {
    use super::{run, Unreachable};
    use crate::store::{MetadataError, MetadataStore, Store};

    /// The SQLite store owes everything any backend owes.
    #[tokio::test]
    async fn the_sqlite_store_passes_the_conformance_suite() {
        run(&Store::open(None).unwrap()).await;
    }

    /// And a file-backed one, whose journal and pragmas differ from memory's.
    #[tokio::test]
    async fn a_file_backed_sqlite_store_passes_it_too() {
        let dir = tempfile::tempdir().unwrap();
        run(&Store::open(Some(&dir.path().join("sunrise.db"))).unwrap()).await;
    }

    /// A store failure is the typed, retryable error, and only a SQLite store
    /// offers its SQLite-only operations.
    #[tokio::test]
    async fn an_unreachable_backend_is_unavailable_and_not_sqlite() {
        let down = Unreachable;
        assert!(matches!(
            down.account("x").await,
            Err(MetadataError::Unavailable(_))
        ));
        assert!(down.as_sqlite().is_none());
        assert!(MetadataStore::as_sqlite(&Store::open(None).unwrap()).is_some());
    }
}
