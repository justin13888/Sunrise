//! Content-addressed encrypted blob store.
//!
//! Per `docs/04-storage/blob-store.md`. Each chunk is its own file under
//! `$VAULT/blobs/<blob_id_hex_2>/<blob_id_hex>/<chunk_idx>.bin`. Storage
//! is opaque ciphertext; integrity comes from the AEAD tag inside.
//!
//! The client vault (`sunrise-core`, through `sunrise-storage`'s re-export)
//! and the relay (`sunrise-server`, rooted per account) both write chunks
//! through this one type, so the two share one layout and one atomic write.
//! It lives here, beside the other staged-and-renamed write, so the relay
//! need not depend on the client's storage crate for it (issue #775). Chunks
//! are ciphertext and are not written owner-only: [`crate::write_private`] is
//! for secrets.

use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// Blob store errors.
#[derive(Debug)]
pub enum BlobStoreError {
    /// IO error.
    Io(std::io::Error),
    /// Chunk index out of range (negative is impossible by type).
    ChunkOutOfRange(u32, u32),
}

impl fmt::Display for BlobStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "blob store io error: {e}"),
            Self::ChunkOutOfRange(idx, count) => {
                write!(f, "chunk index {idx} exceeds chunk_count {count}")
            }
        }
    }
}

impl std::error::Error for BlobStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::ChunkOutOfRange(..) => None,
        }
    }
}

impl From<std::io::Error> for BlobStoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Lowercase hex of `bytes`: a blob id's directory name.
fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Blob store rooted at a vault directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Construct a blob store rooted at `vault_dir/blobs/`.
    pub fn new(vault_dir: &Path) -> Result<Self, BlobStoreError> {
        let root = vault_dir.join("blobs");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn chunk_path(&self, blob_id: &[u8; 16], chunk_idx: u32) -> PathBuf {
        let name = hex(blob_id);
        let prefix = &name[..2];
        self.root
            .join(prefix)
            .join(&name)
            .join(format!("{chunk_idx}.bin"))
    }

    /// Persist one ciphertext chunk.
    pub fn put_chunk(
        &self,
        blob_id: &[u8; 16],
        chunk_idx: u32,
        ciphertext: &[u8],
    ) -> Result<(), BlobStoreError> {
        let path = self.chunk_path(blob_id, chunk_idx);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("bin.tmp");
        // Closed before the rename by leaving scope. Not `drop(f)`: on
        // wasm32-unknown-unknown `File` has no `Drop` impl, and clippy's
        // `drop_non_drop` rejects the call when the web core is linted.
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(ciphertext)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Read one ciphertext chunk; returns `None` if absent.
    pub fn get_chunk(
        &self,
        blob_id: &[u8; 16],
        chunk_idx: u32,
    ) -> Result<Option<Vec<u8>>, BlobStoreError> {
        let path = self.chunk_path(blob_id, chunk_idx);
        match fs::File::open(&path) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether all chunks of a blob are present.
    pub fn has_all(&self, blob_id: &[u8; 16], chunk_count: u32) -> Result<bool, BlobStoreError> {
        for i in 0..chunk_count {
            let path = self.chunk_path(blob_id, i);
            if !path.exists() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Remove all chunks of a blob (tombstone GC).
    pub fn delete_all(&self, blob_id: &[u8; 16]) -> Result<(), BlobStoreError> {
        let name = hex(blob_id);
        let dir = self.root.join(&name[..2]).join(&name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let blob_id = [0xabu8; 16];
        store.put_chunk(&blob_id, 0, b"chunk-0").unwrap();
        store.put_chunk(&blob_id, 1, b"chunk-1").unwrap();
        assert_eq!(
            store.get_chunk(&blob_id, 0).unwrap().unwrap(),
            b"chunk-0".to_vec()
        );
        assert_eq!(
            store.get_chunk(&blob_id, 1).unwrap().unwrap(),
            b"chunk-1".to_vec()
        );
        assert!(store.has_all(&blob_id, 2).unwrap());
        assert!(!store.has_all(&blob_id, 3).unwrap());
    }

    /// The layout is shared with the relay and with every vault already on
    /// disk, so the exact path is pinned rather than only round-tripped.
    #[test]
    fn a_chunk_lands_at_the_documented_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let mut blob_id = [0u8; 16];
        blob_id[0] = 0x0a;
        blob_id[15] = 0xff;
        store.put_chunk(&blob_id, 7, b"c").unwrap();
        let expected = dir
            .path()
            .join("blobs")
            .join("0a")
            .join("0a0000000000000000000000000000ff")
            .join("7.bin");
        assert_eq!(fs::read(expected).unwrap(), b"c".to_vec());
    }

    #[test]
    fn an_io_error_keeps_its_message_and_source() {
        use std::error::Error as _;
        let e = BlobStoreError::from(std::io::Error::other("disk gone"));
        assert_eq!(e.to_string(), "blob store io error: disk gone");
        assert_eq!(e.source().unwrap().to_string(), "disk gone");

        let e = BlobStoreError::ChunkOutOfRange(3, 2);
        assert_eq!(e.to_string(), "chunk index 3 exceeds chunk_count 2");
        assert!(e.source().is_none());
    }

    #[test]
    fn missing_chunk_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        assert!(store.get_chunk(&[0u8; 16], 0).unwrap().is_none());
    }

    #[test]
    fn delete_all_works() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let blob_id = [1u8; 16];
        store.put_chunk(&blob_id, 0, b"x").unwrap();
        store.delete_all(&blob_id).unwrap();
        assert!(store.get_chunk(&blob_id, 0).unwrap().is_none());
    }

    #[test]
    fn delete_all_leaves_a_sibling_under_the_same_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let doomed = [0x5au8; 16];
        let mut sibling = doomed;
        sibling[15] = 0x00;
        assert_eq!(
            hex(&doomed)[..2],
            hex(&sibling)[..2],
            "both blobs live under one prefix directory"
        );
        store.put_chunk(&doomed, 0, b"doomed").unwrap();
        store.put_chunk(&sibling, 0, b"sibling-0").unwrap();
        store.put_chunk(&sibling, 1, b"sibling-1").unwrap();
        store.delete_all(&doomed).unwrap();
        assert!(store.get_chunk(&doomed, 0).unwrap().is_none());
        assert_eq!(
            store.get_chunk(&sibling, 0).unwrap().unwrap(),
            b"sibling-0".to_vec()
        );
        assert_eq!(
            store.get_chunk(&sibling, 1).unwrap().unwrap(),
            b"sibling-1".to_vec()
        );
        assert!(store.has_all(&sibling, 2).unwrap());
    }

    #[test]
    fn put_chunk_twice_overwrites_the_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path()).unwrap();
        let blob_id = [2u8; 16];
        store.put_chunk(&blob_id, 0, b"first, and longer").unwrap();
        store.put_chunk(&blob_id, 0, b"second").unwrap();
        assert_eq!(
            store.get_chunk(&blob_id, 0).unwrap().unwrap(),
            b"second".to_vec()
        );
        let leftovers: Vec<_> = fs::read_dir(store.chunk_path(&blob_id, 0).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("0.bin")]);
    }
}
