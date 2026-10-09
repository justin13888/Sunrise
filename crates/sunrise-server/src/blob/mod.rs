//! The blob seam ADR-0062 §1 and §7 put attachment ciphertext behind.
//!
//! [`BlobBackend`] covers pending uploads, committed chunks, manifests,
//! deletion, and the listings the maintenance pass sweeps. [`FsBlobs`], the
//! per-account tree under `[storage] blob_root`, is the first implementation
//! and the only one today; nothing outside it builds a blob path.
//!
//! Every key the seam takes is the account's 16-byte hash
//! ([`crate::relay_log::account_key`]), never its id: the per-account isolation
//! that keeps content addressing from naming another account's blob is a
//! property of the key, so a backend cannot forget it.
//!
//! # The two-phase commit a backend owes
//!
//! A blob is **readable exactly when its manifest exists**. `finalize` writes
//! every committed chunk first and the manifest last, so an interrupted
//! finalize leaves chunks no reader can see, which the maintenance pass sweeps;
//! and every removal takes the manifest before any chunk, so a reader never
//! finds a manifest naming a chunk already gone. The test-only `conformance::run` checks
//! both directions.

mod fs;

#[cfg(test)]
pub(crate) mod conformance;

use std::path::Path;

pub use fs::FsBlobs;

/// The account hash every blob key is filed under.
pub type AccountKey = [u8; 16];

/// What makes a committed blob readable: how many chunks it has and how many
/// ciphertext bytes they hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Manifest {
    /// Chunks, numbered from zero.
    pub chunk_count: u32,
    /// Total ciphertext bytes.
    pub size_bytes: u64,
}

/// The two halves of an account's blob tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// Uploads between `init` and `finalize`.
    Pending,
    /// Finalized blobs and their manifests.
    Committed,
}

/// One abandoned upload a backend listed, named the way that backend names it,
/// to be handed back to [`BlobBackend::remove_upload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleUpload(pub(crate) String);

/// One area of one account's tree that nothing has touched for a while: what
/// the orphan sweep compares against the accounts that still exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountTree {
    /// Which half.
    pub area: Area,
    /// Whose.
    pub account: AccountKey,
}

/// What `admin stats` reports about the blob store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlobUsage {
    /// Committed chunk and manifest files.
    pub blob_files: u64,
    /// Their bytes.
    pub blob_bytes: u64,
    /// Uploads begun and not finalized or swept.
    pub pending_uploads: u64,
}

/// What `admin backup` copied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackupTotals {
    /// Files written under the destination.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// Why a [`BlobBackend`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The backend did not answer, or refused the write or read: a full or
    /// read-only disk, a lost connection. Retryable: a request path answers
    /// `503 RELAY_STORAGE_UNAVAILABLE`.
    #[error("blob storage unavailable: {0}")]
    Unavailable(String),
    /// A manifest exists and does not say what a manifest says. Not
    /// retryable: the same read fails the same way until an operator looks.
    #[error("blob manifest unreadable: {0}")]
    Corrupt(String),
}

impl From<std::io::Error> for BlobError {
    fn from(e: std::io::Error) -> Self {
        Self::Unavailable(e.to_string())
    }
}

/// Where attachment ciphertext lives: ADR-0062's `BlobBackend`.
///
/// Async because a shared object store is across a network; [`FsBlobs`] never
/// awaits, so moving behind the trait changes no timing on the single-binary
/// deployment. What an implementation owes beyond the signatures is the
/// two-phase commit this module's documentation states, account isolation,
/// and idempotent removal, which the test-only `conformance::run` checks.
#[async_trait::async_trait]
pub trait BlobBackend: Send + Sync + std::fmt::Debug {
    /// Make the upload's pending area, so a chunk `PUT` is a write rather
    /// than a create racing another chunk's.
    async fn open_upload(&self, account: AccountKey, upload: [u8; 16]) -> Result<(), BlobError>;
    /// Store one pending chunk, replacing any earlier copy whole.
    async fn put_pending(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        idx: u32,
        bytes: &[u8],
    ) -> Result<(), BlobError>;
    /// One pending chunk, or `None` if it never arrived.
    async fn get_pending(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobError>;
    /// Remove an upload's pending area and nothing of any other upload's.
    /// Absent is not an error.
    async fn discard_upload(&self, account: AccountKey, upload: [u8; 16]) -> Result<(), BlobError>;

    /// Store one committed chunk. Invisible to a reader until
    /// [`write_manifest`](Self::write_manifest).
    async fn put_committed(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
        bytes: &[u8],
    ) -> Result<(), BlobError>;
    /// Write the manifest that makes `blob` readable. Called after every
    /// chunk is stored.
    async fn write_manifest(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        manifest: Manifest,
    ) -> Result<(), BlobError>;
    /// The blob's manifest, or `None` when it is not readable.
    async fn manifest(
        &self,
        account: AccountKey,
        blob: [u8; 16],
    ) -> Result<Option<Manifest>, BlobError>;
    /// Whether chunks `0..chunk_count` are all present.
    async fn has_all(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        chunk_count: u32,
    ) -> Result<bool, BlobError>;
    /// One committed chunk, or `None` if absent.
    async fn get_committed(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobError>;
    /// Remove one committed blob: its manifest first, then its chunks. Absent
    /// pieces are not an error, so a removal interrupted half-way finishes on
    /// the next call.
    async fn delete_committed(&self, account: AccountKey, blob: [u8; 16]) -> Result<(), BlobError>;

    /// Remove both halves of the account's tree, each manifests first.
    async fn erase_account(&self, account: AccountKey) -> Result<(), BlobError>;
    /// Uploads untouched for longer than `ttl_ms` before `now_ms`, measured
    /// from the newest write anywhere in the upload.
    async fn stale_uploads(&self, now_ms: u64, ttl_ms: u64) -> Result<Vec<StaleUpload>, BlobError>;
    /// Remove one upload [`stale_uploads`](Self::stale_uploads) listed.
    async fn remove_upload(&self, upload: &StaleUpload) -> Result<(), BlobError>;
    /// Account trees untouched for longer than `ttl_ms` before `now_ms`.
    async fn stale_account_trees(
        &self,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Vec<AccountTree>, BlobError>;
    /// Remove one area of one account's tree, manifests first.
    async fn remove_account_tree(&self, tree: AccountTree) -> Result<(), BlobError>;

    /// Counts for `admin stats`.
    async fn usage(&self) -> Result<BlobUsage, BlobError>;
    /// Write, read back and remove a probe object: readiness and `doctor`.
    async fn probe(&self) -> Result<(), BlobError>;
    /// Copy every committed blob into a filesystem tree at `dest`, the
    /// manifests last, so a blob removed during the copy is left without one.
    async fn backup_to(&self, dest: &Path) -> Result<BackupTotals, BlobError>;
    /// Where the blobs are, for an operator's message.
    fn location(&self) -> String;
}
