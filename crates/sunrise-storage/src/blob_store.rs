//! Content-addressed encrypted blob store.
//!
//! Per `spec/04-storage/blob-store.md`. Each chunk is its own file under
//! `$VAULT/blobs/<blob_id_hex_2>/<blob_id_hex>/<chunk_idx>.bin`. Storage
//! is opaque ciphertext; integrity comes from the AEAD tag inside.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Blob store errors.
#[derive(Debug, Error)]
pub enum BlobStoreError {
    /// IO error.
    #[error("blob store io error: {0}")]
    Io(#[from] std::io::Error),
    /// Chunk index out of range (negative is impossible by type).
    #[error("chunk index {0} exceeds chunk_count {1}")]
    ChunkOutOfRange(u32, u32),
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
        let hex = hex::encode(blob_id);
        let prefix = &hex[..2];
        self.root
            .join(prefix)
            .join(&hex)
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
        let mut f = fs::File::create(&tmp)?;
        f.write_all(ciphertext)?;
        f.sync_all()?;
        drop(f);
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
        let hex = hex::encode(blob_id);
        let dir = self.root.join(&hex[..2]).join(&hex);
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
}
