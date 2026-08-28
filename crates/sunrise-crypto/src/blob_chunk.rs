//! Attachment blob chunks: seal, open, and the envelope they travel in.
//!
//! Per `docs/03-crypto/data-encryption-format.md` §Blob chunks. An attachment's
//! plaintext is split into [`CHUNK_PLAINTEXT_LEN`] pieces (the last may be
//! shorter) and each piece is sealed independently under the parent
//! attachment's per-blob key.
//!
//! # Why the nonce is derived rather than random
//!
//! A random 24-byte nonce would be safe, and it would also have to be stored
//! per chunk. Deriving it from `blob_key || u32_be(chunk_idx)` makes the chunk
//! envelope self-describing: given the key and the index — both of which a
//! reader already has, because it is asking for chunk *n* — the nonce is
//! recomputable, so the envelope carries no nonce field at all. The derivation
//! is unique per `(blob_key, chunk_idx)` and `blob_key` is fresh random per
//! attachment, so no `(key, nonce)` pair is ever reused.
//!
//! # Why the AAD names the chunk count
//!
//! The AAD binds `blob_id`, `chunk_idx` **and** `chunk_count`. Binding the
//! index alone would let a truncating relay hand back a prefix of the blob and
//! have every chunk verify: each piece really was chunk *n* of that blob. With
//! the count bound, a chunk sealed as "3 of 7" cannot be replayed as "3 of 3",
//! so truncation fails the tag rather than the content hash — which is the
//! difference between a decrypt error and a silently short file.
//!
//! Content integrity for the *assembled* attachment is a separate, single
//! BLAKE3 over the concatenated plaintext, recorded on the attachment metadata
//! op ([`content_hash`]). Per-chunk hashes would duplicate what the AEAD tag
//! already does.

use crate::aead::{aead_open_xchacha, aead_seal_xchacha, AeadError, AEAD_KEY_LEN, AEAD_NONCE_LEN};
use crate::blake3_kdf::derive_key;
use thiserror::Error;

/// Plaintext bytes per chunk: 256 KiB. The last chunk of a blob may be
/// shorter; every other chunk is exactly this long.
pub const CHUNK_PLAINTEXT_LEN: usize = 256 * 1024;

/// KDF context for the per-chunk nonce. Unique to this purpose, per
/// `docs/03-crypto/primitives.md`.
const NONCE_CONTEXT: &str = "sunrise.blob_chunk_nonce.v1";

/// Blob-chunk errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlobChunkError {
    /// `chunk_idx` was not below `chunk_count`.
    #[error("chunk index {idx} out of range for a blob of {count} chunks")]
    IndexOutOfRange {
        /// The index asked for.
        idx: u32,
        /// The blob's chunk count.
        count: u32,
    },
    /// A blob must have at least one chunk; a zero-length attachment is not
    /// representable and is rejected at the metadata layer.
    #[error("a blob must have at least one chunk")]
    EmptyBlob,
    /// AEAD verify failed: wrong key, wrong index, wrong count, wrong blob, or
    /// altered ciphertext. The five are deliberately indistinguishable.
    #[error("blob chunk failed authentication")]
    AuthFailed,
    /// The reassembled plaintext did not hash to the metadata's claim.
    #[error("attachment content hash mismatch")]
    ContentHashMismatch,
    /// The plaintext was too large for one chunk.
    #[error("chunk plaintext is {len} bytes, over the {CHUNK_PLAINTEXT_LEN}-byte limit")]
    ChunkTooLarge {
        /// The offending length.
        len: usize,
    },
}

impl From<AeadError> for BlobChunkError {
    fn from(_: AeadError) -> Self {
        Self::AuthFailed
    }
}

/// The nonce for one chunk: `BLAKE3.derive_key(ctx, blob_key || u32_be(idx))`.
#[must_use]
pub fn chunk_nonce(blob_key: &[u8; AEAD_KEY_LEN], chunk_idx: u32) -> [u8; AEAD_NONCE_LEN] {
    let mut material = [0u8; AEAD_KEY_LEN + 4];
    material[..AEAD_KEY_LEN].copy_from_slice(blob_key);
    material[AEAD_KEY_LEN..].copy_from_slice(&chunk_idx.to_be_bytes());
    let derived = derive_key(NONCE_CONTEXT, &material, AEAD_NONCE_LEN);
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    nonce.copy_from_slice(&derived);
    nonce
}

/// The AAD for one chunk: canonical CBOR `{1: blob_id, 2: chunk_idx, 3: chunk_count}`.
///
/// Built by hand rather than through `serde` because the map has three
/// small-integer keys in ascending order, which is already canonical: writing
/// it directly means the bytes cannot drift with a serde attribute.
#[must_use]
pub fn chunk_aad(blob_id: &[u8; 16], chunk_idx: u32, chunk_count: u32) -> Vec<u8> {
    use ciborium::value::{Integer, Value};

    let map = Value::Map(vec![
        (
            Value::Integer(Integer::from(1u8)),
            Value::Bytes(blob_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(2u8)),
            Value::Integer(Integer::from(chunk_idx)),
        ),
        (
            Value::Integer(Integer::from(3u8)),
            Value::Integer(Integer::from(chunk_count)),
        ),
    ]);
    let mut out = Vec::with_capacity(32);
    // Infallible: the value is a three-entry map of integers and a 16-byte
    // string, and the writer is a `Vec`. There is no failure mode to report.
    let _ = ciborium::ser::into_writer(&map, &mut out);
    out
}

/// Seal one chunk of plaintext.
///
/// # Errors
///
/// [`BlobChunkError::IndexOutOfRange`] when `chunk_idx >= chunk_count`,
/// [`BlobChunkError::EmptyBlob`] when `chunk_count` is zero, and
/// [`BlobChunkError::ChunkTooLarge`] when the piece exceeds
/// [`CHUNK_PLAINTEXT_LEN`].
pub fn seal_chunk(
    blob_key: &[u8; AEAD_KEY_LEN],
    blob_id: &[u8; 16],
    chunk_idx: u32,
    chunk_count: u32,
    plaintext: &[u8],
) -> Result<Vec<u8>, BlobChunkError> {
    check_range(chunk_idx, chunk_count)?;
    if plaintext.len() > CHUNK_PLAINTEXT_LEN {
        return Err(BlobChunkError::ChunkTooLarge {
            len: plaintext.len(),
        });
    }
    let nonce = chunk_nonce(blob_key, chunk_idx);
    let aad = chunk_aad(blob_id, chunk_idx, chunk_count);
    Ok(aead_seal_xchacha(blob_key, &nonce, plaintext, &aad)?)
}

/// Open one sealed chunk.
///
/// # Errors
///
/// [`BlobChunkError::AuthFailed`] on any mismatch of key, blob id, index,
/// count or ciphertext — the AEAD tag does not distinguish them, and neither
/// does this.
pub fn open_chunk(
    blob_key: &[u8; AEAD_KEY_LEN],
    blob_id: &[u8; 16],
    chunk_idx: u32,
    chunk_count: u32,
    ciphertext: &[u8],
) -> Result<Vec<u8>, BlobChunkError> {
    check_range(chunk_idx, chunk_count)?;
    let nonce = chunk_nonce(blob_key, chunk_idx);
    let aad = chunk_aad(blob_id, chunk_idx, chunk_count);
    Ok(aead_open_xchacha(blob_key, &nonce, ciphertext, &aad)?)
}

/// How many chunks `size_bytes` of plaintext splits into.
///
/// Zero bytes is zero chunks, which callers must reject: an attachment with no
/// bytes has nothing to seal and nothing to fetch.
#[must_use]
pub fn chunk_count_for(size_bytes: u64) -> u32 {
    let chunk = CHUNK_PLAINTEXT_LEN as u64;
    let count = size_bytes.div_ceil(chunk);
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// BLAKE3 over the concatenated plaintext — the attachment's `content_hash`.
///
/// A plain hash, not a KDF: this is an integrity check on user bytes, and it
/// has to match what any other implementation computes from the spec.
#[must_use]
pub fn content_hash(plaintext: &[u8]) -> [u8; 32] {
    *blake3::hash(plaintext).as_bytes()
}

/// Check a reassembled attachment against the hash its metadata claims.
///
/// # Errors
///
/// [`BlobChunkError::ContentHashMismatch`]. Constant-time, because the claim
/// travels in an op an attacker may have influenced.
pub fn verify_content(plaintext: &[u8], claimed: &[u8; 32]) -> Result<(), BlobChunkError> {
    use subtle::ConstantTimeEq;
    if content_hash(plaintext).ct_eq(claimed).into() {
        Ok(())
    } else {
        Err(BlobChunkError::ContentHashMismatch)
    }
}

fn check_range(chunk_idx: u32, chunk_count: u32) -> Result<(), BlobChunkError> {
    if chunk_count == 0 {
        return Err(BlobChunkError::EmptyBlob);
    }
    if chunk_idx >= chunk_count {
        return Err(BlobChunkError::IndexOutOfRange {
            idx: chunk_idx,
            count: chunk_count,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; AEAD_KEY_LEN] {
        let mut k = [0u8; AEAD_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    fn blob() -> [u8; 16] {
        let mut b = [0u8; 16];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (i as u8) ^ 0x5a;
        }
        b
    }

    #[test]
    fn a_chunk_round_trips() {
        let sealed = seal_chunk(&key(), &blob(), 2, 5, b"hello attachment").expect("seal");
        let opened = open_chunk(&key(), &blob(), 2, 5, &sealed).expect("open");
        assert_eq!(opened, b"hello attachment");
    }

    #[test]
    fn the_nonce_is_unique_per_index_and_stable_across_calls() {
        assert_eq!(chunk_nonce(&key(), 0), chunk_nonce(&key(), 0));
        assert_ne!(chunk_nonce(&key(), 0), chunk_nonce(&key(), 1));
        // A different key gives a different nonce at the same index, so two
        // attachments never share a (key, nonce) pair even at chunk 0.
        let mut other = key();
        other[0] ^= 1;
        assert_ne!(chunk_nonce(&key(), 0), chunk_nonce(&other, 0));
    }

    /// The truncation case the AAD's `chunk_count` exists for: a chunk sealed
    /// as "1 of 7" must not open as "1 of 2".
    #[test]
    fn a_chunk_will_not_open_under_a_different_chunk_count() {
        let sealed = seal_chunk(&key(), &blob(), 1, 7, b"payload").expect("seal");
        assert_eq!(
            open_chunk(&key(), &blob(), 1, 2, &sealed),
            Err(BlobChunkError::AuthFailed)
        );
    }

    #[test]
    fn a_chunk_will_not_open_at_another_index_or_under_another_blob() {
        let sealed = seal_chunk(&key(), &blob(), 1, 7, b"payload").expect("seal");
        assert_eq!(
            open_chunk(&key(), &blob(), 2, 7, &sealed),
            Err(BlobChunkError::AuthFailed)
        );
        let mut elsewhere = blob();
        elsewhere[0] ^= 1;
        assert_eq!(
            open_chunk(&key(), &elsewhere, 1, 7, &sealed),
            Err(BlobChunkError::AuthFailed)
        );
    }

    #[test]
    fn a_tampered_chunk_fails_closed() {
        let mut sealed = seal_chunk(&key(), &blob(), 0, 1, b"payload").expect("seal");
        sealed[0] ^= 0x01;
        assert_eq!(
            open_chunk(&key(), &blob(), 0, 1, &sealed),
            Err(BlobChunkError::AuthFailed)
        );
    }

    #[test]
    fn an_out_of_range_index_is_refused_before_any_crypto_runs() {
        assert_eq!(
            seal_chunk(&key(), &blob(), 3, 3, b"x"),
            Err(BlobChunkError::IndexOutOfRange { idx: 3, count: 3 })
        );
        assert_eq!(
            seal_chunk(&key(), &blob(), 0, 0, b"x"),
            Err(BlobChunkError::EmptyBlob)
        );
    }

    #[test]
    fn an_oversized_chunk_is_refused() {
        let big = vec![0u8; CHUNK_PLAINTEXT_LEN + 1];
        assert_eq!(
            seal_chunk(&key(), &blob(), 0, 1, &big),
            Err(BlobChunkError::ChunkTooLarge {
                len: CHUNK_PLAINTEXT_LEN + 1
            })
        );
    }

    #[test]
    fn chunk_counts_round_up_and_an_empty_blob_has_none() {
        assert_eq!(chunk_count_for(0), 0);
        assert_eq!(chunk_count_for(1), 1);
        assert_eq!(chunk_count_for(CHUNK_PLAINTEXT_LEN as u64), 1);
        assert_eq!(chunk_count_for(CHUNK_PLAINTEXT_LEN as u64 + 1), 2);
    }

    #[test]
    fn content_verification_accepts_the_claim_and_rejects_a_changed_byte() {
        let bytes = b"the whole attachment";
        let hash = content_hash(bytes);
        assert!(verify_content(bytes, &hash).is_ok());
        assert_eq!(
            verify_content(b"the whole attachmenu", &hash),
            Err(BlobChunkError::ContentHashMismatch)
        );
    }

    /// The AAD is the spec's canonical CBOR map, byte for byte. Asserting the
    /// bytes rather than the round trip is what stops a refactor from quietly
    /// changing a value another implementation has to reproduce.
    #[test]
    fn the_aad_is_the_canonical_map_the_spec_names() {
        let aad = chunk_aad(&[0xab; 16], 1, 2);
        let mut expected = vec![0xa3, 0x01, 0x50];
        expected.extend_from_slice(&[0xab; 16]);
        expected.extend_from_slice(&[0x02, 0x01, 0x03, 0x02]);
        assert_eq!(aad, expected);
    }

    /// A whole multi-chunk attachment, sealed and reassembled the way the
    /// upload and download paths do it.
    #[test]
    fn a_multi_chunk_blob_reassembles_and_verifies() {
        let plaintext: Vec<u8> = (0..CHUNK_PLAINTEXT_LEN * 2 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let count = chunk_count_for(plaintext.len() as u64);
        assert_eq!(count, 3);

        let sealed: Vec<Vec<u8>> = plaintext
            .chunks(CHUNK_PLAINTEXT_LEN)
            .enumerate()
            .map(|(i, piece)| seal_chunk(&key(), &blob(), i as u32, count, piece).expect("seal"))
            .collect();

        let mut assembled = Vec::with_capacity(plaintext.len());
        for (i, ct) in sealed.iter().enumerate() {
            assembled.extend_from_slice(&open_chunk(&key(), &blob(), i as u32, count, ct).unwrap());
        }
        assert_eq!(assembled, plaintext);
        assert!(verify_content(&assembled, &content_hash(&plaintext)).is_ok());
    }
}
