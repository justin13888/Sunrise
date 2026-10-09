//! [`FsBlobs`]: the blob tree under `[storage] blob_root`, and the only code in
//! the relay that builds a path inside it.
//!
//! ```text
//! <root>/pending/<account_h>/up_<upload>/blobs/<xx>/<upload>/<idx>.bin
//! <root>/committed/<account_h>/blobs/<xx>/<blob>/<idx>.bin
//! <root>/committed/<account_h>/manifests/<blob>
//! ```
//!
//! Every name is lowercase hex of bytes the relay derived — the account's
//! BLAKE3 hash, a parsed upload id, a content address — so no path is ever
//! built from a string a client spelled, and none reveals an account id to
//! anyone reading the disk. Chunks are written to a temporary and renamed into
//! place whole by [`sunrise_storage::BlobStore`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sunrise_storage::BlobStore;

use super::{
    AccountKey, AccountTree, Area, BackupTotals, BlobBackend, BlobError, BlobUsage, Manifest,
    StaleUpload,
};

/// Where uploads wait between `init` and `finalize`.
const PENDING: &str = "pending";

/// Where finalized blobs live.
const COMMITTED: &str = "committed";

/// The blob tree rooted at one directory.
#[derive(Debug, Clone)]
pub struct FsBlobs {
    root: PathBuf,
}

impl FsBlobs {
    /// The tree under `root`, which is created on first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// One account's directory in one area: `<root>/<area>/<account_h hex>`.
    #[must_use]
    pub fn account_dir(&self, area: Area, account: AccountKey) -> PathBuf {
        self.root.join(area_name(area)).join(hex::encode(account))
    }

    /// Where one upload's chunks live, under that account's pending root.
    ///
    /// One directory per upload keeps a discard from touching any other
    /// in-flight upload of the same account. Named from the parsed bytes,
    /// re-encoded, so `up_AB…` and `up_ab…` can never be two directories.
    fn upload_dir(&self, account: AccountKey, upload: [u8; 16]) -> PathBuf {
        self.account_dir(Area::Pending, account)
            .join(format!("up_{}", hex::encode(upload)))
    }

    fn pending(&self, account: AccountKey, upload: [u8; 16]) -> Result<BlobStore, BlobError> {
        BlobStore::new(&self.upload_dir(account, upload)).map_err(unavailable)
    }

    fn committed(&self, account: AccountKey) -> Result<BlobStore, BlobError> {
        BlobStore::new(&self.account_dir(Area::Committed, account)).map_err(unavailable)
    }

    fn manifest_path(&self, account: AccountKey, blob: [u8; 16]) -> PathBuf {
        self.account_dir(Area::Committed, account)
            .join("manifests")
            .join(hex::encode(blob))
    }
}

const fn area_name(area: Area) -> &'static str {
    match area {
        Area::Pending => PENDING,
        Area::Committed => COMMITTED,
    }
}

fn unavailable(e: impl std::fmt::Display) -> BlobError {
    BlobError::Unavailable(e.to_string())
}

/// `remove_dir_all`, with an absent target not an error.
fn remove_if_present(target: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Remove one account tree, its `manifests/` directory first.
///
/// `remove_dir_all` alone removes entries in whatever order the directory
/// lists them, so chunks could go while their manifests remain. Removing the
/// manifests first keeps the order blob collection keeps, which an online
/// backup relies on: a manifest it copies names chunks it copied too. An absent
/// tree is not an error, so an interrupted removal finishes later.
fn remove_account_tree(dir: &Path) -> std::io::Result<()> {
    remove_if_present(&dir.join("manifests"))?;
    remove_if_present(dir)
}

#[async_trait::async_trait]
impl BlobBackend for FsBlobs {
    async fn open_upload(&self, account: AccountKey, upload: [u8; 16]) -> Result<(), BlobError> {
        self.pending(account, upload).map(drop)
    }

    async fn put_pending(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        idx: u32,
        bytes: &[u8],
    ) -> Result<(), BlobError> {
        self.pending(account, upload)?
            .put_chunk(&upload, idx, bytes)
            .map_err(unavailable)
    }

    async fn get_pending(
        &self,
        account: AccountKey,
        upload: [u8; 16],
        idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        // A read of an upload that was never opened finds nothing, and
        // creates no directory for the sweep to find later.
        if !self.upload_dir(account, upload).is_dir() {
            return Ok(None);
        }
        self.pending(account, upload)?
            .get_chunk(&upload, idx)
            .map_err(unavailable)
    }

    async fn discard_upload(&self, account: AccountKey, upload: [u8; 16]) -> Result<(), BlobError> {
        Ok(remove_if_present(&self.upload_dir(account, upload))?)
    }

    async fn put_committed(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
        bytes: &[u8],
    ) -> Result<(), BlobError> {
        self.committed(account)?
            .put_chunk(&blob, idx, bytes)
            .map_err(unavailable)
    }

    /// `"<chunk_count> <size_bytes>"`.
    async fn write_manifest(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        manifest: Manifest,
    ) -> Result<(), BlobError> {
        let path = self.manifest_path(account, blob);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            format!("{} {}", manifest.chunk_count, manifest.size_bytes),
        )?;
        Ok(())
    }

    async fn manifest(
        &self,
        account: AccountKey,
        blob: [u8; 16],
    ) -> Result<Option<Manifest>, BlobError> {
        let raw = match std::fs::read_to_string(self.manifest_path(account, blob)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut parts = raw.split_whitespace();
        let mut field = |name: &str| {
            parts
                .next()
                .ok_or_else(|| BlobError::Corrupt(format!("no {name}")))
        };
        let chunk_count = field("chunk count")?
            .parse()
            .map_err(|_| BlobError::Corrupt("chunk count is not a number".to_owned()))?;
        let size_bytes = field("size")?
            .parse()
            .map_err(|_| BlobError::Corrupt("size is not a number".to_owned()))?;
        Ok(Some(Manifest {
            chunk_count,
            size_bytes,
        }))
    }

    async fn has_all(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        chunk_count: u32,
    ) -> Result<bool, BlobError> {
        self.committed(account)?
            .has_all(&blob, chunk_count)
            .map_err(unavailable)
    }

    async fn get_committed(
        &self,
        account: AccountKey,
        blob: [u8; 16],
        idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        self.committed(account)?
            .get_chunk(&blob, idx)
            .map_err(unavailable)
    }

    async fn delete_committed(&self, account: AccountKey, blob: [u8; 16]) -> Result<(), BlobError> {
        match std::fs::remove_file(self.manifest_path(account, blob)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.committed(account)?
            .delete_all(&blob)
            .map_err(unavailable)
    }

    async fn erase_account(&self, account: AccountKey) -> Result<(), BlobError> {
        for area in [Area::Pending, Area::Committed] {
            remove_account_tree(&self.account_dir(area, account))?;
        }
        Ok(())
    }

    async fn stale_uploads(&self, now_ms: u64, ttl_ms: u64) -> Result<Vec<StaleUpload>, BlobError> {
        Ok(stale_children(&self.root.join(PENDING), now_ms, ttl_ms, 2)
            .into_iter()
            .filter_map(|dir| {
                dir.strip_prefix(&self.root)
                    .ok()
                    .and_then(Path::to_str)
                    .map(|rel| StaleUpload(rel.to_owned()))
            })
            .collect())
    }

    async fn remove_upload(&self, upload: &StaleUpload) -> Result<(), BlobError> {
        // Only ever a name `stale_uploads` produced, two levels under
        // `pending/`; refusing anything else keeps a forged one from reaching
        // outside the tree.
        let rel = Path::new(&upload.0);
        let inside = rel.starts_with(PENDING)
            && rel.components().count() == 3
            && rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !inside {
            return Err(BlobError::Unavailable(format!(
                "{} is not a pending upload",
                upload.0
            )));
        }
        Ok(std::fs::remove_dir_all(self.root.join(rel))?)
    }

    async fn stale_account_trees(
        &self,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<Vec<AccountTree>, BlobError> {
        let mut out = Vec::new();
        for area in [Area::Pending, Area::Committed] {
            for dir in stale_children(&self.root.join(area_name(area)), now_ms, ttl_ms, 1) {
                let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let mut account = [0u8; 16];
                // Only a name this backend could have written: 32 lowercase
                // hex characters.
                if name.len() != 32 || !name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                {
                    continue;
                }
                if hex::decode_to_slice(name, &mut account).is_ok() {
                    out.push(AccountTree { area, account });
                }
            }
        }
        Ok(out)
    }

    async fn remove_account_tree(&self, tree: AccountTree) -> Result<(), BlobError> {
        Ok(remove_account_tree(
            &self.account_dir(tree.area, tree.account),
        )?)
    }

    async fn usage(&self) -> Result<BlobUsage, BlobError> {
        let (blob_files, blob_bytes) = tree_size(&self.root.join(COMMITTED));
        let pending_uploads = std::fs::read_dir(self.root.join(PENDING))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|account| std::fs::read_dir(account.path()).ok())
            .map(|uploads| uploads.filter_map(Result::ok).count() as u64)
            .sum::<u64>();
        Ok(BlobUsage {
            blob_files,
            blob_bytes,
            pending_uploads,
        })
    }

    /// Creating the root first is deliberate: the blob routes create it on
    /// first use, so a fresh data directory without one is ready, and a root
    /// that cannot be created is exactly the condition a blob upload would
    /// fail on. The probe's name is unique per call, so two concurrent probes
    /// never remove each other's file and a crash mid-probe leaves a name
    /// nothing else uses.
    async fn probe(&self) -> Result<(), BlobError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(format!(
            ".ready-probe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let written = write_read_back(&path, b"ok");
        let removed = std::fs::remove_file(&path);
        written.and(removed)?;
        Ok(())
    }

    /// The tree's files, then its manifests.
    ///
    /// 1. The manifests present are listed first. A manifest is written only
    ///    after every chunk of its blob, and a finalized blob's chunks never
    ///    change, so each listed blob is complete at this moment.
    /// 2. Every other file is copied: chunks are renamed into place whole, and
    ///    temporaries are skipped.
    /// 3. The listed manifests are copied last, each one only if it still
    ///    exists. Every removal takes a manifest before its chunks, so a blob
    ///    removed during step 2 is left without one.
    async fn backup_to(&self, dest: &Path) -> Result<BackupTotals, BlobError> {
        let manifests = manifests_under(&self.root)?;
        let (mut files, mut bytes) = copy_tree(&self.root, dest)?;
        for manifest in manifests {
            let Ok(rel) = manifest.strip_prefix(&self.root) else {
                continue;
            };
            let target = dest.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::copy(&manifest, &target) {
                Ok(n) => {
                    files += 1;
                    bytes += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(BackupTotals { files, bytes })
    }

    fn location(&self) -> String {
        self.root.display().to_string()
    }
}

/// Write `data` to `path`, fsync, read it back and compare.
fn write_read_back(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    {
        let mut f = std::fs::File::create(path)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if std::fs::read(path)? != data {
        return Err(std::io::Error::other(
            "read back differs from what was written",
        ));
    }
    Ok(())
}

/// Directories `depth` levels below `root` whose newest modification anywhere
/// inside is older than `ttl_ms` before `now_ms`.
///
/// The newest time anywhere in the tree, not the directory's own: writing a
/// chunk into an existing directory does not always move the directory's
/// mtime, and an upload with a chunk written a minute ago is not abandoned.
fn stale_children(root: &Path, now_ms: u64, ttl_ms: u64, depth: u32) -> Vec<PathBuf> {
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

/// Whether `dir` is a blob manifest directory: `committed/<acct_h>/manifests`.
fn is_manifest_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "manifests")
}

/// Every file in a manifest directory under `root`.
fn manifests_under(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if is_manifest_dir(&path) {
            for m in std::fs::read_dir(&path)? {
                let m = m?;
                if m.file_type()?.is_file() && !m.file_name().to_string_lossy().ends_with(".tmp") {
                    out.push(m.path());
                }
            }
        } else {
            out.extend(manifests_under(&path)?);
        }
    }
    Ok(out)
}

/// Copy `from` to `to`, skipping temporaries and manifest directories, which
/// [`FsBlobs::backup_to`] copies last.
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
            if is_manifest_dir(&entry.path()) {
                continue;
            }
            let (f, b) = copy_tree(&entry.path(), &target)?;
            files += f;
            bytes += b;
        } else if kind.is_file() && !entry.file_name().to_string_lossy().ends_with(".tmp") {
            // A file removed between the listing and the copy — a blob
            // collected, an upload swept — is not part of the backup.
            match std::fs::copy(entry.path(), &target) {
                Ok(n) => {
                    bytes += n;
                    files += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::{Area, FsBlobs};

    /// An upload's directory is decided by its bytes, never by how they were
    /// spelled.
    ///
    /// `pending_store` used to join the raw URL segment. `parse_hex16` decodes
    /// case-insensitively, so `up_AB…` and `up_ab…` are the same sixteen bytes
    /// and used to become two directories: chunks PUT under one spelling were
    /// invisible to a `finalize` using the other, and the abandoned one was
    /// never swept.
    ///
    /// **Asserted on the constructed path, not on the filesystem.** The split
    /// only happens where directory names are case-*sensitive*, which is
    /// production (Linux) and is not macOS, whose APFS volumes fold case by
    /// default. A test that wrote a chunk under each spelling and looked for
    /// two directories would therefore have passed on a laptop while the
    /// defect was live on the relay. Comparing the `PathBuf` is the same
    /// question asked where the answer does not depend on the host.
    #[test]
    fn an_uploads_directory_is_named_by_its_bytes_and_not_by_their_spelling() {
        let blobs = FsBlobs::new("/blobs");
        let account = [0xac; 16];
        let root = blobs.account_dir(Area::Pending, account);
        let mut upper = [0u8; 16];
        let mut lower = [0u8; 16];
        hex::decode_to_slice("AB".repeat(16), &mut upper).unwrap();
        hex::decode_to_slice("ab".repeat(16), &mut lower).unwrap();
        assert_eq!(upper, lower, "the premise: one upload, two spellings");

        assert_eq!(
            blobs.upload_dir(account, upper),
            blobs.upload_dir(account, lower),
            "two spellings of one upload must not become two directories"
        );
        assert_eq!(
            blobs.upload_dir(account, upper),
            root.join(format!("up_{}", "ab".repeat(16))),
            "and the one they share is the canonical lowercase spelling"
        );
    }
}
