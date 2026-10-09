//! The checks every [`BlobBackend`] must pass, written once and run against
//! each implementation: [`FsBlobs`] in this module's tests, and an object
//! store in its own. Also [`Unreachable`], the backend that fails every call,
//! for the tests of what a caller does with a failure.

use std::path::Path;

use super::{
    AccountKey, AccountTree, BackupTotals, BlobBackend, BlobError, BlobUsage, Manifest, StaleUpload,
};

/// A backend that never answers, standing in for an object store that lost
/// its connection.
#[derive(Debug)]
pub(crate) struct Unreachable;

fn down() -> BlobError {
    BlobError::Unavailable("connection refused".to_owned())
}

#[async_trait::async_trait]
impl BlobBackend for Unreachable {
    async fn open_upload(&self, _: AccountKey, _: [u8; 16]) -> Result<(), BlobError> {
        Err(down())
    }
    async fn put_pending(
        &self,
        _: AccountKey,
        _: [u8; 16],
        _: u32,
        _: &[u8],
    ) -> Result<(), BlobError> {
        Err(down())
    }
    async fn get_pending(
        &self,
        _: AccountKey,
        _: [u8; 16],
        _: u32,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        Err(down())
    }
    async fn discard_upload(&self, _: AccountKey, _: [u8; 16]) -> Result<(), BlobError> {
        Err(down())
    }
    async fn put_committed(
        &self,
        _: AccountKey,
        _: [u8; 16],
        _: u32,
        _: &[u8],
    ) -> Result<(), BlobError> {
        Err(down())
    }
    async fn write_manifest(
        &self,
        _: AccountKey,
        _: [u8; 16],
        _: Manifest,
    ) -> Result<(), BlobError> {
        Err(down())
    }
    async fn manifest(&self, _: AccountKey, _: [u8; 16]) -> Result<Option<Manifest>, BlobError> {
        Err(down())
    }
    async fn has_all(&self, _: AccountKey, _: [u8; 16], _: u32) -> Result<bool, BlobError> {
        Err(down())
    }
    async fn get_committed(
        &self,
        _: AccountKey,
        _: [u8; 16],
        _: u32,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        Err(down())
    }
    async fn delete_committed(&self, _: AccountKey, _: [u8; 16]) -> Result<(), BlobError> {
        Err(down())
    }
    async fn erase_account(&self, _: AccountKey) -> Result<(), BlobError> {
        Err(down())
    }
    async fn stale_uploads(&self, _: u64, _: u64) -> Result<Vec<StaleUpload>, BlobError> {
        Err(down())
    }
    async fn remove_upload(&self, _: &StaleUpload) -> Result<(), BlobError> {
        Err(down())
    }
    async fn stale_account_trees(&self, _: u64, _: u64) -> Result<Vec<AccountTree>, BlobError> {
        Err(down())
    }
    async fn remove_account_tree(&self, _: AccountTree) -> Result<(), BlobError> {
        Err(down())
    }
    async fn usage(&self) -> Result<BlobUsage, BlobError> {
        Err(down())
    }
    async fn probe(&self) -> Result<(), BlobError> {
        Err(down())
    }
    async fn backup_to(&self, _: &Path) -> Result<BackupTotals, BlobError> {
        Err(down())
    }
    fn location(&self) -> String {
        "unreachable".to_owned()
    }
}

const ALICE: AccountKey = [0xa1; 16];
const BOB: AccountKey = [0xb0; 16];
const UPLOAD: [u8; 16] = [0x01; 16];
const OTHER_UPLOAD: [u8; 16] = [0x02; 16];
const BLOB: [u8; 16] = [0xbb; 16];

/// Run every check against a backend that starts empty.
pub(crate) async fn run(backend: &dyn BlobBackend) {
    pending_uploads_are_per_upload_and_per_account(backend).await;
    an_interrupted_finalize_leaves_no_readable_blob(backend).await;
    a_committed_blob_reads_back_and_is_removed_manifest_first(backend).await;
    erasure_takes_both_areas_of_one_account(backend).await;
    backend
        .probe()
        .await
        .expect("an idle backend takes a probe");
}

/// A chunk round-trips through its upload; another upload and another
/// account see none of it; discarding one upload leaves the other.
async fn pending_uploads_are_per_upload_and_per_account(backend: &dyn BlobBackend) {
    backend.open_upload(ALICE, UPLOAD).await.unwrap();
    backend.open_upload(ALICE, OTHER_UPLOAD).await.unwrap();
    backend
        .put_pending(ALICE, UPLOAD, 0, b"first")
        .await
        .unwrap();
    backend
        .put_pending(ALICE, UPLOAD, 0, b"again")
        .await
        .unwrap();
    backend
        .put_pending(ALICE, OTHER_UPLOAD, 0, b"other")
        .await
        .unwrap();
    assert_eq!(
        backend
            .get_pending(ALICE, UPLOAD, 0)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"again"[..]),
        "a re-sent chunk replaces the first whole"
    );
    assert_eq!(backend.get_pending(ALICE, UPLOAD, 1).await.unwrap(), None);
    assert_eq!(
        backend.get_pending(BOB, UPLOAD, 0).await.unwrap(),
        None,
        "another account's pending area is its own"
    );
    assert_eq!(backend.usage().await.unwrap().pending_uploads, 2);

    backend.discard_upload(ALICE, UPLOAD).await.unwrap();
    backend.discard_upload(ALICE, UPLOAD).await.unwrap();
    assert_eq!(backend.get_pending(ALICE, UPLOAD, 0).await.unwrap(), None);
    assert_eq!(
        backend
            .get_pending(ALICE, OTHER_UPLOAD, 0)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"other"[..]),
        "a discard touches no other upload"
    );
    backend.discard_upload(ALICE, OTHER_UPLOAD).await.unwrap();
}

/// Chunks committed with no manifest after them — a finalize that stopped
/// part-way — are not a readable blob, and become one only when the manifest
/// is written.
async fn an_interrupted_finalize_leaves_no_readable_blob(backend: &dyn BlobBackend) {
    backend
        .put_committed(ALICE, BLOB, 0, b"half")
        .await
        .unwrap();
    assert_eq!(
        backend.manifest(ALICE, BLOB).await.unwrap(),
        None,
        "chunks alone are not a blob"
    );
    assert!(!backend.has_all(ALICE, BLOB, 2).await.unwrap());

    backend
        .put_committed(ALICE, BLOB, 1, b"done")
        .await
        .unwrap();
    let manifest = Manifest {
        chunk_count: 2,
        size_bytes: 8,
    };
    backend.write_manifest(ALICE, BLOB, manifest).await.unwrap();
    assert_eq!(backend.manifest(ALICE, BLOB).await.unwrap(), Some(manifest));
    assert_eq!(
        backend.manifest(BOB, BLOB).await.unwrap(),
        None,
        "the same content address under another account names nothing"
    );
    backend.delete_committed(ALICE, BLOB).await.unwrap();
}

/// A committed blob reads back chunk by chunk; deleting it removes the
/// manifest so it stops being readable, then the chunks; deleting it again is
/// not an error.
async fn a_committed_blob_reads_back_and_is_removed_manifest_first(backend: &dyn BlobBackend) {
    for (idx, chunk) in [&b"one"[..], b"two"].iter().enumerate() {
        backend
            .put_committed(ALICE, BLOB, u32::try_from(idx).unwrap(), chunk)
            .await
            .unwrap();
    }
    backend
        .write_manifest(
            ALICE,
            BLOB,
            Manifest {
                chunk_count: 2,
                size_bytes: 6,
            },
        )
        .await
        .unwrap();
    assert!(backend.has_all(ALICE, BLOB, 2).await.unwrap());
    assert_eq!(
        backend
            .get_committed(ALICE, BLOB, 1)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"two"[..])
    );
    assert_eq!(backend.get_committed(BOB, BLOB, 1).await.unwrap(), None);
    let usage = backend.usage().await.unwrap();
    assert!(usage.blob_files >= 2 && usage.blob_bytes >= 6, "{usage:?}");

    backend.delete_committed(ALICE, BLOB).await.unwrap();
    assert_eq!(backend.manifest(ALICE, BLOB).await.unwrap(), None);
    assert_eq!(backend.get_committed(ALICE, BLOB, 0).await.unwrap(), None);
    backend.delete_committed(ALICE, BLOB).await.unwrap();
}

/// Erasing an account removes both its areas and nothing of another's.
async fn erasure_takes_both_areas_of_one_account(backend: &dyn BlobBackend) {
    for account in [ALICE, BOB] {
        backend.open_upload(account, UPLOAD).await.unwrap();
        backend.put_pending(account, UPLOAD, 0, b"p").await.unwrap();
        backend.put_committed(account, BLOB, 0, b"c").await.unwrap();
        backend
            .write_manifest(
                account,
                BLOB,
                Manifest {
                    chunk_count: 1,
                    size_bytes: 1,
                },
            )
            .await
            .unwrap();
    }
    backend.erase_account(ALICE).await.unwrap();
    backend.erase_account(ALICE).await.unwrap();
    assert_eq!(backend.get_pending(ALICE, UPLOAD, 0).await.unwrap(), None);
    assert_eq!(backend.manifest(ALICE, BLOB).await.unwrap(), None);
    assert_eq!(backend.get_committed(ALICE, BLOB, 0).await.unwrap(), None);
    assert!(backend.get_pending(BOB, UPLOAD, 0).await.unwrap().is_some());
    assert!(backend.manifest(BOB, BLOB).await.unwrap().is_some());
}

#[cfg(test)]
mod tests {
    use super::{run, Unreachable, ALICE, BLOB, UPLOAD};
    use crate::blob::{Area, BlobBackend, BlobError, FsBlobs, Manifest};

    /// The filesystem tree owes everything any backend owes.
    #[tokio::test]
    async fn the_filesystem_backend_passes_the_conformance_suite() {
        let dir = tempfile::tempdir().unwrap();
        run(&FsBlobs::new(dir.path().join("blobs"))).await;
    }

    /// A failure is the typed, retryable error.
    #[tokio::test]
    async fn an_unreachable_backend_is_unavailable() {
        assert!(matches!(
            Unreachable.manifest(ALICE, BLOB).await,
            Err(BlobError::Unavailable(_))
        ));
    }

    /// A root that cannot be created is unavailable, not a panic, on the
    /// write path and the probe alike.
    #[tokio::test]
    async fn a_root_under_a_file_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let blobs = FsBlobs::new(file.join("blobs"));
        assert!(matches!(
            blobs.open_upload(ALICE, UPLOAD).await,
            Err(BlobError::Unavailable(_))
        ));
        assert!(matches!(
            blobs.probe().await,
            Err(BlobError::Unavailable(_))
        ));
    }

    /// A manifest that does not parse is corrupt rather than absent: absent
    /// would answer `404` for a blob the relay still holds.
    #[tokio::test]
    async fn a_manifest_that_does_not_parse_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = FsBlobs::new(dir.path());
        blobs
            .write_manifest(
                ALICE,
                BLOB,
                Manifest {
                    chunk_count: 1,
                    size_bytes: 1,
                },
            )
            .await
            .unwrap();
        let path = blobs
            .account_dir(Area::Committed, ALICE)
            .join("manifests")
            .join(hex::encode(BLOB));
        std::fs::write(&path, "one").unwrap();
        assert!(matches!(
            blobs.manifest(ALICE, BLOB).await,
            Err(BlobError::Corrupt(_))
        ));
    }

    /// The sweep's listing names only what `stale_uploads` produced; a forged
    /// name cannot reach outside `pending/`.
    #[tokio::test]
    async fn remove_upload_refuses_a_name_outside_the_pending_tree() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = FsBlobs::new(dir.path().join("blobs"));
        for forged in [
            "../../etc",
            "committed/aa/bb",
            "pending/aa",
            "pending/../x/y",
        ] {
            assert!(
                blobs
                    .remove_upload(&crate::blob::StaleUpload(forged.to_owned()))
                    .await
                    .is_err(),
                "{forged}"
            );
        }
    }
}
