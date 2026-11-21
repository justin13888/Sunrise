//! [`MetadataStore`] for [`PgStore`]: each method is the inherent one of the
//! same name in `accounts.rs`, `lifecycle.rs` or `relay_log.rs`.

use std::collections::HashMap;
use std::time::Duration;

use super::{unavailable, PgStore};
use crate::auth::Subject;
use crate::relay::{ConnId, FrameHead, StreamKey};
use crate::relay_log::{Appended, DurableCaps, Replay};
use crate::store::{
    Account, AccountSummary, CommittedBlob, DeclaredCursor, Device, IdentityChainPinned,
    KeyBundlePinned, MetadataError, MetadataStore, NewDevice, NewTombstone, PinnedIdentityChain,
    PinnedKeyBundle, PinnedSnapshot, Reinstatement, RetiredRecoveryBlob, SignedBy, StoreStats,
};

#[async_trait::async_trait]
impl MetadataStore for PgStore {
    async fn resolve_account(
        &self,
        subject: &Subject,
        allow_signup: bool,
        now_ms: u64,
    ) -> Result<Account, MetadataError> {
        Self::resolve_account(self, subject, allow_signup, now_ms).await
    }
    async fn account_exists(&self, subject: &Subject) -> Result<bool, MetadataError> {
        Self::account_exists(self, subject).await
    }
    async fn account(&self, account_id: &str) -> Result<Option<Account>, MetadataError> {
        Self::account(self, account_id).await
    }
    async fn set_identity(
        &self,
        account_id: &str,
        identity_pub_s: &str,
        identity_pub_d: &str,
        recovery_blob: Option<&str>,
        terms_at_ms: u64,
    ) -> Result<Account, MetadataError> {
        Self::set_identity(
            self,
            account_id,
            identity_pub_s,
            identity_pub_d,
            recovery_blob,
            terms_at_ms,
        )
        .await
    }
    async fn recovery_blob(&self, account_id: &str) -> Result<Option<String>, MetadataError> {
        Self::recovery_blob(self, account_id).await
    }
    async fn replace_recovery_blob(
        &self,
        account_id: &str,
        blob: &str,
        check: Option<&[u8]>,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Self::replace_recovery_blob(self, account_id, blob, check, now_ms).await
    }
    async fn retired_recovery_blobs(
        &self,
        account_id: &str,
    ) -> Result<Vec<RetiredRecoveryBlob>, MetadataError> {
        Self::retired_recovery_blobs(self, account_id).await
    }
    async fn pin_identity_chain(
        &self,
        account_id: &str,
        pin: &PinnedIdentityChain,
        replaces: Option<[u8; 32]>,
    ) -> Result<IdentityChainPinned, MetadataError> {
        Self::pin_identity_chain(self, account_id, pin, replaces).await
    }
    async fn pinned_identity_chain(
        &self,
        account_id: &str,
    ) -> Result<Option<PinnedIdentityChain>, MetadataError> {
        Self::pinned_identity_chain(self, account_id).await
    }

    async fn register_device(
        &self,
        account_id: &str,
        new: &NewDevice,
        now_ms: u64,
    ) -> Result<Device, MetadataError> {
        Self::register_device(self, account_id, new, now_ms).await
    }
    async fn list_devices(&self, account_id: &str) -> Result<Vec<Device>, MetadataError> {
        Self::list_devices(self, account_id).await
    }
    async fn active_device_count(&self, account_id: &str) -> Result<u32, MetadataError> {
        Self::active_device_count(self, account_id).await
    }
    async fn active_device(
        &self,
        account_id: &str,
        device_id: &str,
    ) -> Result<Option<Device>, MetadataError> {
        Self::active_device(self, account_id, device_id).await
    }
    async fn revoke_device(
        &self,
        account_id: &str,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Self::revoke_device(self, account_id, device_id, now_ms).await
    }
    async fn revoke_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        revoked_by: Option<SignedBy<'_>>,
        now_ms: u64,
    ) -> Result<usize, MetadataError> {
        Self::revoke_devices_by_vault_id(self, account_id, vault_device_id, revoked_by, now_ms)
            .await
    }
    async fn reinstate_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        caller: SignedBy<'_>,
    ) -> Result<Reinstatement, MetadataError> {
        Self::reinstate_devices_by_vault_id(self, account_id, vault_device_id, caller).await
    }
    async fn touch_device(&self, device_id: &str, now_ms: u64) -> Result<(), MetadataError> {
        Self::touch_device(self, device_id, now_ms).await
    }
    async fn upsert_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Self::upsert_push_token(self, device_id, platform, token, now_ms).await
    }
    async fn push_tokens(&self, device_id: &str) -> Result<Vec<(String, String)>, MetadataError> {
        Self::push_tokens(self, device_id).await
    }
    async fn push_targets(
        &self,
        account_id: &str,
        platform: &str,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        Self::push_targets(self, account_id, platform).await
    }
    async fn push_target(
        &self,
        device_id: &str,
        platform: &str,
    ) -> Result<Option<String>, MetadataError> {
        Self::push_target(self, device_id, platform).await
    }
    async fn delete_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
    ) -> Result<bool, MetadataError> {
        Self::delete_push_token(self, device_id, platform, token).await
    }

    async fn put_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        expires_at_ms: u64,
    ) -> Result<(), MetadataError> {
        Self::put_delete_token(self, account_id, token_h, expires_at_ms).await
    }
    async fn consume_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<bool, MetadataError> {
        Self::consume_delete_token(self, account_id, token_h, now_ms).await
    }
    async fn request_account_deletion(
        &self,
        account_id: &str,
        now_ms: u64,
    ) -> Result<u64, MetadataError> {
        Self::request_account_deletion(self, account_id, now_ms).await
    }
    async fn account_deletion_requested(
        &self,
        account_id: &str,
    ) -> Result<Option<u64>, MetadataError> {
        Self::account_deletion_requested(self, account_id).await
    }
    async fn accounts_due_for_erasure(
        &self,
        requested_by_ms: u64,
    ) -> Result<Vec<String>, MetadataError> {
        Self::accounts_due_for_erasure(self, requested_by_ms).await
    }
    async fn erase_account(&self, account_id: &str) -> Result<bool, MetadataError> {
        Self::erase_account(self, account_id).await
    }
    async fn tombstone_blob(
        &self,
        account_id: &str,
        t: &NewTombstone,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Self::tombstone_blob(self, account_id, t, now_ms).await
    }
    async fn clear_tombstone(
        &self,
        account_id: &str,
        blob_key: &[u8; 16],
    ) -> Result<(), MetadataError> {
        Self::clear_tombstone(self, account_id, blob_key).await
    }
    async fn record_blob(&self, blob: &CommittedBlob) -> Result<(), MetadataError> {
        Self::record_blob(self, blob).await
    }
    async fn forget_blob(
        &self,
        account_id: &str,
        blob_key: &[u8; 16],
    ) -> Result<(), MetadataError> {
        Self::forget_blob(self, account_id, blob_key).await
    }
    async fn backfill_blobs(&self, blobs: &[CommittedBlob]) -> Result<u64, MetadataError> {
        Self::backfill_blobs(self, blobs).await
    }
    async fn record_cursors(
        &self,
        device_id: &str,
        cursors: &[DeclaredCursor],
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Self::record_cursors(self, device_id, cursors, now_ms).await
    }
    async fn pin_snapshot(
        &self,
        account_id: &str,
        pin: &PinnedSnapshot,
    ) -> Result<Option<[u8; 16]>, MetadataError> {
        Self::pin_snapshot(self, account_id, pin).await
    }
    async fn pinned_snapshot(
        &self,
        account_id: &str,
        stream_id: &[u8; 16],
    ) -> Result<Option<PinnedSnapshot>, MetadataError> {
        Self::pinned_snapshot(self, account_id, stream_id).await
    }
    async fn pin_key_bundle(
        &self,
        account_id: &str,
        pin: &PinnedKeyBundle,
        replaces: Option<[u8; 16]>,
    ) -> Result<KeyBundlePinned, MetadataError> {
        Self::pin_key_bundle(self, account_id, pin, replaces).await
    }
    async fn pinned_key_bundle(
        &self,
        account_id: &str,
    ) -> Result<Option<PinnedKeyBundle>, MetadataError> {
        Self::pinned_key_bundle(self, account_id).await
    }
    async fn collectable_blobs(
        &self,
        tombstoned_by_ms: u64,
        active_since_ms: u64,
    ) -> Result<Vec<(String, [u8; 16])>, MetadataError> {
        Self::collectable_blobs(self, tombstoned_by_ms, active_since_ms).await
    }
    async fn account_summaries(&self) -> Result<Vec<AccountSummary>, MetadataError> {
        Self::account_summaries(self).await
    }
    async fn account_summary(
        &self,
        account_id: &str,
    ) -> Result<Option<AccountSummary>, MetadataError> {
        Self::account_summary(self, account_id).await
    }
    async fn device_owner(&self, device_id: &str) -> Result<Option<String>, MetadataError> {
        Self::device_owner(self, device_id).await
    }
    async fn stats(&self) -> Result<StoreStats, MetadataError> {
        Self::stats(self).await
    }

    async fn relay_append(
        &self,
        key: StreamKey,
        bytes: &[u8],
        heads: &[FrameHead],
        n_ops: u64,
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, MetadataError> {
        Self::relay_append_from(
            self, 0, key, bytes, heads, n_ops, ops_h, batch_id, now_ms, caps,
        )
        .await
    }
    async fn relay_append_from(
        &self,
        from: ConnId,
        key: StreamKey,
        bytes: &[u8],
        heads: &[FrameHead],
        n_ops: u64,
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, MetadataError> {
        Self::relay_append_from(
            self, from, key, bytes, heads, n_ops, ops_h, batch_id, now_ms, caps,
        )
        .await
    }
    async fn relay_replay_after(
        &self,
        key: StreamKey,
        after_id: u64,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, MetadataError> {
        Self::relay_replay_after(self, key, after_id, cursors).await
    }
    async fn relay_device_heads(
        &self,
        key: StreamKey,
    ) -> Result<HashMap<[u8; 16], u64>, MetadataError> {
        Self::relay_device_heads(self, key).await
    }
    async fn relay_len(&self, key: StreamKey) -> Result<usize, MetadataError> {
        Self::relay_len(self, key).await
    }

    /// `SELECT 1` within `deadline`, the pool wait included. Never blocks a
    /// thread: the whole probe is awaited.
    async fn ping(&self, deadline: Duration) -> Result<(), MetadataError> {
        let probe = async {
            self.client()
                .await?
                .query_one("SELECT 1", &[])
                .await
                .map_err(unavailable)
                .map(|_| ())
        };
        tokio::time::timeout(deadline, probe)
            .await
            .map_err(|_| MetadataError::Unavailable("the database did not answer in time".into()))?
    }

    async fn lead_maintenance(&self) -> Result<Option<Box<dyn Send>>, MetadataError> {
        Self::lead_maintenance(self).await
    }
}
