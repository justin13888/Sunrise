//! The metadata seam ADR-0062 §1 puts every relay table behind.
//!
//! [`MetadataStore`] covers accounts, devices, push tokens, cursors, blob
//! tombstones, deletion tokens and the durable relay log. Each method is one
//! unit of work that commits or rolls back whole, which is the shape [`Store`]
//! already had: revocation, account erasure and a relay append each touch
//! several tables, and keeping each inside one method keeps its atomicity
//! inside one implementation, where the test-only `store::conformance` suite
//! can see it, rather
//! than in every handler.
//!
//! [`Store`], the SQLite database, is the first implementation and the only one
//! today. Its methods never await, so moving behind the trait changes no
//! timing on the single-binary deployment.
//!
//! Errors are typed. The three refusals a caller acts on —
//! [`MetadataError::SignupDisabled`], [`MetadataError::NotFound`] and
//! [`MetadataError::RecoveryBlobExists`] — are the ones [`StoreError`] already
//! carried; everything else a backend can fail with is
//! [`MetadataError::Unavailable`], which a request path answers
//! `503 RELAY_STORAGE_UNAVAILABLE` and the client retries.

use std::collections::HashMap;
use std::time::Duration;

use super::{
    Account, AccountSummary, DeclaredCursor, Device, NewDevice, NewTombstone, Store, StoreError,
    StoreStats,
};
use crate::auth::Subject;
use crate::relay::{CursorGap, FrameHead, StreamKey};
use crate::relay_log::{Appended, DurableCaps, Replay};

/// Why a [`MetadataStore`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    /// The caller's `(iss, sub)` has no account and `allow_signup` is false.
    #[error("sign-up is disabled on this server")]
    SignupDisabled,
    /// No such row for this account.
    #[error("not found")]
    NotFound,
    /// A `recovery_blob` was offered for an account that already holds a
    /// different one. See [`StoreError::RecoveryBlobExists`].
    #[error("this account already holds a different recovery blob")]
    RecoveryBlobExists,
    /// The backend did not answer, or failed the statement. Nothing the call
    /// would have written was committed. Retryable: a request path answers
    /// `503 RELAY_STORAGE_UNAVAILABLE`.
    #[error("metadata store unavailable: {0}")]
    Unavailable(String),
}

impl From<StoreError> for MetadataError {
    /// The refusals keep their meaning; every other SQLite failure is the
    /// backend not answering. The open-time variants cannot reach here, since
    /// a store that did not open is never a `MetadataStore`.
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::SignupDisabled => Self::SignupDisabled,
            StoreError::NotFound => Self::NotFound,
            StoreError::RecoveryBlobExists => Self::RecoveryBlobExists,
            other => Self::Unavailable(other.to_string()),
        }
    }
}

/// Everything the relay keeps about accounts, devices and the durable log,
/// behind one seam: ADR-0062 §1's `MetadataStore`.
///
/// Every method is documented on [`Store`], whose inherent method of the same
/// name is the SQLite implementation; what an implementation owes beyond the
/// signature is what the test-only `store::conformance::run` checks:
///
/// - **Revocation is atomic.** [`revoke_device`](Self::revoke_device) hides the
///   device from [`active_device`](Self::active_device) and drops its push
///   tokens in the same unit of work, and is scoped to the caller's account.
/// - **The relay log is ordered and bounded per channel.** Replay returns
///   frames in append order; retention evicts oldest first and reports what a
///   cursor can no longer recover as a [`CursorGap`].
/// - **Erasure is whole.** [`erase_account`](Self::erase_account) removes the
///   account's rows and its relay frames together, and nothing of another
///   account's.
#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync + std::fmt::Debug {
    // --- accounts -----------------------------------------------------------

    /// [`Store::resolve_account`].
    async fn resolve_account(
        &self,
        subject: &Subject,
        allow_signup: bool,
        now_ms: u64,
    ) -> Result<Account, MetadataError>;
    /// [`Store::account_exists`].
    async fn account_exists(&self, subject: &Subject) -> Result<bool, MetadataError>;
    /// [`Store::account`].
    async fn account(&self, account_id: &str) -> Result<Option<Account>, MetadataError>;
    /// [`Store::set_identity`].
    async fn set_identity(
        &self,
        account_id: &str,
        identity_pub_s: &str,
        identity_pub_d: &str,
        recovery_blob: Option<&str>,
        terms_at_ms: u64,
    ) -> Result<Account, MetadataError>;
    /// [`Store::recovery_blob`].
    async fn recovery_blob(&self, account_id: &str) -> Result<Option<String>, MetadataError>;

    // --- devices and push tokens --------------------------------------------

    /// [`Store::register_device`].
    async fn register_device(
        &self,
        account_id: &str,
        new: &NewDevice,
        now_ms: u64,
    ) -> Result<Device, MetadataError>;
    /// [`Store::list_devices`].
    async fn list_devices(&self, account_id: &str) -> Result<Vec<Device>, MetadataError>;
    /// [`Store::active_device_count`].
    async fn active_device_count(&self, account_id: &str) -> Result<u32, MetadataError>;
    /// [`Store::active_device`].
    async fn active_device(
        &self,
        account_id: &str,
        device_id: &str,
    ) -> Result<Option<Device>, MetadataError>;
    /// [`Store::revoke_device`].
    async fn revoke_device(
        &self,
        account_id: &str,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError>;
    /// [`Store::revoke_devices_by_vault_id`].
    async fn revoke_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        now_ms: u64,
    ) -> Result<usize, MetadataError>;
    /// [`Store::touch_device`].
    async fn touch_device(&self, device_id: &str, now_ms: u64) -> Result<(), MetadataError>;
    /// [`Store::upsert_push_token`].
    async fn upsert_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError>;
    /// [`Store::push_tokens`].
    async fn push_tokens(&self, device_id: &str) -> Result<Vec<(String, String)>, MetadataError>;
    /// [`Store::push_targets`].
    async fn push_targets(
        &self,
        account_id: &str,
        platform: &str,
    ) -> Result<Vec<(String, String)>, MetadataError>;
    /// [`Store::push_target`].
    async fn push_target(
        &self,
        device_id: &str,
        platform: &str,
    ) -> Result<Option<String>, MetadataError>;
    /// [`Store::delete_push_token`].
    async fn delete_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
    ) -> Result<bool, MetadataError>;

    // --- account and blob lifecycle -----------------------------------------

    /// [`Store::put_delete_token`].
    async fn put_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        expires_at_ms: u64,
    ) -> Result<(), MetadataError>;
    /// [`Store::consume_delete_token`].
    async fn consume_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<bool, MetadataError>;
    /// [`Store::request_account_deletion`].
    async fn request_account_deletion(
        &self,
        account_id: &str,
        now_ms: u64,
    ) -> Result<u64, MetadataError>;
    /// [`Store::account_deletion_requested`].
    async fn account_deletion_requested(
        &self,
        account_id: &str,
    ) -> Result<Option<u64>, MetadataError>;
    /// [`Store::accounts_due_for_erasure`].
    async fn accounts_due_for_erasure(
        &self,
        requested_by_ms: u64,
    ) -> Result<Vec<String>, MetadataError>;
    /// [`Store::erase_account`].
    async fn erase_account(&self, account_id: &str) -> Result<bool, MetadataError>;
    /// [`Store::tombstone_blob`].
    async fn tombstone_blob(
        &self,
        account_id: &str,
        t: &NewTombstone,
        now_ms: u64,
    ) -> Result<(), MetadataError>;
    /// [`Store::clear_tombstone`].
    async fn clear_tombstone(
        &self,
        account_id: &str,
        blob_key: &[u8; 16],
    ) -> Result<(), MetadataError>;
    /// [`Store::record_cursors`].
    async fn record_cursors(
        &self,
        device_id: &str,
        cursors: &[DeclaredCursor],
        now_ms: u64,
    ) -> Result<(), MetadataError>;
    /// [`Store::collectable_blobs`].
    async fn collectable_blobs(
        &self,
        tombstoned_by_ms: u64,
        active_since_ms: u64,
    ) -> Result<Vec<(String, [u8; 16])>, MetadataError>;
    /// [`Store::account_summaries`].
    async fn account_summaries(&self) -> Result<Vec<AccountSummary>, MetadataError>;
    /// [`Store::account_summary`].
    async fn account_summary(
        &self,
        account_id: &str,
    ) -> Result<Option<AccountSummary>, MetadataError>;
    /// [`Store::device_owner`].
    async fn device_owner(&self, device_id: &str) -> Result<Option<String>, MetadataError>;
    /// [`Store::stats`].
    async fn stats(&self) -> Result<StoreStats, MetadataError>;

    // --- the durable relay log ----------------------------------------------

    /// [`Store::relay_append`]: the frame, its routing heads, its dedup row
    /// and its channel's retention sweep, in one unit of work.
    #[allow(clippy::too_many_arguments)]
    async fn relay_append(
        &self,
        key: StreamKey,
        bytes: &[u8],
        heads: &[FrameHead],
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, MetadataError>;
    /// [`Store::relay_replay`].
    async fn relay_replay(
        &self,
        key: StreamKey,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<(Vec<Vec<u8>>, Vec<CursorGap>), MetadataError>;
    /// [`Store::relay_replay_after`].
    async fn relay_replay_after(
        &self,
        key: StreamKey,
        after_id: u64,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, MetadataError>;
    /// [`Store::relay_device_heads`].
    async fn relay_device_heads(
        &self,
        key: StreamKey,
    ) -> Result<HashMap<[u8; 16], u64>, MetadataError>;
    /// [`Store::relay_len`].
    async fn relay_len(&self, key: StreamKey) -> Result<usize, MetadataError>;

    // --- operations ---------------------------------------------------------

    /// Answer a trivial query within `deadline`: the readiness probe's store
    /// check.
    ///
    /// May block for up to `deadline` — the SQLite store waits on its one
    /// connection's lock — so a caller on the async runtime runs it on a
    /// blocking thread.
    async fn ping(&self, deadline: Duration) -> Result<(), MetadataError>;

    /// The SQLite store behind this seam, where it is one.
    ///
    /// For what only a single-file database has — `admin backup`'s online
    /// copy, `admin rekey`, the doctor's `quick_check`, and the WAL checkpoint
    /// at shutdown — none of which a shared backend offers in this form. A
    /// request path never calls it.
    fn as_sqlite(&self) -> Option<&Store> {
        None
    }
}

/// The SQLite implementation: each method is the inherent one of the same
/// name, which already runs as one transaction under the connection's mutex.
#[async_trait::async_trait]
impl MetadataStore for Store {
    async fn resolve_account(
        &self,
        subject: &Subject,
        allow_signup: bool,
        now_ms: u64,
    ) -> Result<Account, MetadataError> {
        Ok(Self::resolve_account(self, subject, allow_signup, now_ms)?)
    }
    async fn account_exists(&self, subject: &Subject) -> Result<bool, MetadataError> {
        Ok(Self::account_exists(self, subject)?)
    }
    async fn account(&self, account_id: &str) -> Result<Option<Account>, MetadataError> {
        Ok(Self::account(self, account_id)?)
    }
    async fn set_identity(
        &self,
        account_id: &str,
        identity_pub_s: &str,
        identity_pub_d: &str,
        recovery_blob: Option<&str>,
        terms_at_ms: u64,
    ) -> Result<Account, MetadataError> {
        Ok(Self::set_identity(
            self,
            account_id,
            identity_pub_s,
            identity_pub_d,
            recovery_blob,
            terms_at_ms,
        )?)
    }
    async fn recovery_blob(&self, account_id: &str) -> Result<Option<String>, MetadataError> {
        Ok(Self::recovery_blob(self, account_id)?)
    }

    async fn register_device(
        &self,
        account_id: &str,
        new: &NewDevice,
        now_ms: u64,
    ) -> Result<Device, MetadataError> {
        Ok(Self::register_device(self, account_id, new, now_ms)?)
    }
    async fn list_devices(&self, account_id: &str) -> Result<Vec<Device>, MetadataError> {
        Ok(Self::list_devices(self, account_id)?)
    }
    async fn active_device_count(&self, account_id: &str) -> Result<u32, MetadataError> {
        Ok(Self::active_device_count(self, account_id)?)
    }
    async fn active_device(
        &self,
        account_id: &str,
        device_id: &str,
    ) -> Result<Option<Device>, MetadataError> {
        Ok(Self::active_device(self, account_id, device_id)?)
    }
    async fn revoke_device(
        &self,
        account_id: &str,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Ok(Self::revoke_device(self, account_id, device_id, now_ms)?)
    }
    async fn revoke_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        now_ms: u64,
    ) -> Result<usize, MetadataError> {
        Ok(Self::revoke_devices_by_vault_id(
            self,
            account_id,
            vault_device_id,
            now_ms,
        )?)
    }
    async fn touch_device(&self, device_id: &str, now_ms: u64) -> Result<(), MetadataError> {
        Ok(Self::touch_device(self, device_id, now_ms)?)
    }
    async fn upsert_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Ok(Self::upsert_push_token(
            self, device_id, platform, token, now_ms,
        )?)
    }
    async fn push_tokens(&self, device_id: &str) -> Result<Vec<(String, String)>, MetadataError> {
        Ok(Self::push_tokens(self, device_id)?)
    }
    async fn push_targets(
        &self,
        account_id: &str,
        platform: &str,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        Ok(Self::push_targets(self, account_id, platform)?)
    }
    async fn push_target(
        &self,
        device_id: &str,
        platform: &str,
    ) -> Result<Option<String>, MetadataError> {
        Ok(Self::push_target(self, device_id, platform)?)
    }
    async fn delete_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
    ) -> Result<bool, MetadataError> {
        Ok(Self::delete_push_token(self, device_id, platform, token)?)
    }

    async fn put_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        expires_at_ms: u64,
    ) -> Result<(), MetadataError> {
        Ok(Self::put_delete_token(
            self,
            account_id,
            token_h,
            expires_at_ms,
        )?)
    }
    async fn consume_delete_token(
        &self,
        account_id: &str,
        token_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<bool, MetadataError> {
        Ok(Self::consume_delete_token(
            self, account_id, token_h, now_ms,
        )?)
    }
    async fn request_account_deletion(
        &self,
        account_id: &str,
        now_ms: u64,
    ) -> Result<u64, MetadataError> {
        Ok(Self::request_account_deletion(self, account_id, now_ms)?)
    }
    async fn account_deletion_requested(
        &self,
        account_id: &str,
    ) -> Result<Option<u64>, MetadataError> {
        Ok(Self::account_deletion_requested(self, account_id)?)
    }
    async fn accounts_due_for_erasure(
        &self,
        requested_by_ms: u64,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(Self::accounts_due_for_erasure(self, requested_by_ms)?)
    }
    async fn erase_account(&self, account_id: &str) -> Result<bool, MetadataError> {
        Ok(Self::erase_account(self, account_id)?)
    }
    async fn tombstone_blob(
        &self,
        account_id: &str,
        t: &NewTombstone,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Ok(Self::tombstone_blob(self, account_id, t, now_ms)?)
    }
    async fn clear_tombstone(
        &self,
        account_id: &str,
        blob_key: &[u8; 16],
    ) -> Result<(), MetadataError> {
        Ok(Self::clear_tombstone(self, account_id, blob_key)?)
    }
    async fn record_cursors(
        &self,
        device_id: &str,
        cursors: &[DeclaredCursor],
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        Ok(Self::record_cursors(self, device_id, cursors, now_ms)?)
    }
    async fn collectable_blobs(
        &self,
        tombstoned_by_ms: u64,
        active_since_ms: u64,
    ) -> Result<Vec<(String, [u8; 16])>, MetadataError> {
        Ok(Self::collectable_blobs(
            self,
            tombstoned_by_ms,
            active_since_ms,
        )?)
    }
    async fn account_summaries(&self) -> Result<Vec<AccountSummary>, MetadataError> {
        Ok(Self::account_summaries(self)?)
    }
    async fn account_summary(
        &self,
        account_id: &str,
    ) -> Result<Option<AccountSummary>, MetadataError> {
        Ok(Self::account_summary(self, account_id)?)
    }
    async fn device_owner(&self, device_id: &str) -> Result<Option<String>, MetadataError> {
        Ok(Self::device_owner(self, device_id)?)
    }
    async fn stats(&self) -> Result<StoreStats, MetadataError> {
        Ok(Self::stats(self)?)
    }

    async fn relay_append(
        &self,
        key: StreamKey,
        bytes: &[u8],
        heads: &[FrameHead],
        ops_h: Option<&[u8; 32]>,
        batch_id: u64,
        now_ms: u64,
        caps: DurableCaps,
    ) -> Result<Appended, MetadataError> {
        Ok(Self::relay_append(
            self, key, bytes, heads, ops_h, batch_id, now_ms, caps,
        )?)
    }
    async fn relay_replay(
        &self,
        key: StreamKey,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<(Vec<Vec<u8>>, Vec<CursorGap>), MetadataError> {
        Ok(Self::relay_replay(self, key, cursors)?)
    }
    async fn relay_replay_after(
        &self,
        key: StreamKey,
        after_id: u64,
        cursors: &HashMap<[u8; 16], u64>,
    ) -> Result<Replay, MetadataError> {
        Ok(Self::relay_replay_after(self, key, after_id, cursors)?)
    }
    async fn relay_device_heads(
        &self,
        key: StreamKey,
    ) -> Result<HashMap<[u8; 16], u64>, MetadataError> {
        Ok(Self::relay_device_heads(self, key)?)
    }
    async fn relay_len(&self, key: StreamKey) -> Result<usize, MetadataError> {
        Ok(Self::relay_len(self, key)?)
    }

    async fn ping(&self, deadline: Duration) -> Result<(), MetadataError> {
        Ok(Self::ping(self, deadline)?)
    }

    fn as_sqlite(&self) -> Option<&Store> {
        Some(self)
    }
}
