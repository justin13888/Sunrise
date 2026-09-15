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
//! per chunk. Deriving it makes the chunk envelope self-describing: given the
//! key and the chunking — both of which a reader already has, because it is
//! asking for chunk *n* of *m* in blob *b* — the nonce is recomputable, so the
//! envelope carries no nonce field at all.
//!
//! # The nonce is derived from the AAD, and that is the whole point
//!
//! [`chunk_nonce`] hashes `blob_key || chunk_aad(blob_id, chunk_idx,
//! chunk_count)`. It does **not** take the three values separately and
//! reassemble them, because that is the shape the defect had: the nonce was
//! `blob_key || u32_be(chunk_idx)` and `chunk_count` was bound in the AAD
//! alone.
//!
//! AAD does not enter the keystream. XChaCha20-Poly1305 derives its stream from
//! `(key, nonce)` and nothing else, so two *different chunkings of the same
//! plaintext* under one `blob_key` — say the same bytes sealed once as "0 of 1"
//! and once as "0 of 2" — produced two ciphertexts over one keystream. Their
//! XOR is the XOR of the two plaintexts: classic two-time-pad, confidentiality
//! of both messages gone, and the Poly1305 key with it. The AAD caught the
//! *replay* of one chunk as another but could not prevent the *reuse*, because
//! it never reached the cipher.
//!
//! Feeding the AAD bytes themselves — rather than a hand-rolled copy of the
//! same three fields — is what keeps the two in step. Anything that becomes
//! part of what distinguishes one chunking from another has to be added to
//! [`chunk_aad`], and adding it there now changes the nonce by construction.
//! A future field bound in the AAD and forgotten in the nonce is not
//! expressible.
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

use crate::aead::{
    aead_open_xchacha, aead_seal_xchacha, AeadError, AEAD_KEY_LEN, AEAD_NONCE_LEN, AEAD_TAG_LEN,
};
use crate::blake3_kdf::derive_key;
use thiserror::Error;

/// Plaintext bytes per chunk: 256 KiB. The last chunk of a blob may be
/// shorter; every other chunk is exactly this long.
pub const CHUNK_PLAINTEXT_LEN: usize = 256 * 1024;

/// Ciphertext bytes a *full* chunk occupies: the plaintext plus the AEAD tag.
///
/// The envelope carries no nonce — [`chunk_nonce`] derives it — so a sealed
/// chunk is exactly its plaintext with sixteen bytes appended, and every chunk
/// of a blob but the last is exactly this long.
///
/// That is what makes a concatenated blob splittable again. `GET /blobs/{id}`
/// streams the committed chunks back as one body with no framing between them,
/// and the reader has to hand [`open_chunk`] the same boundaries the sealer
/// used: the AAD binds `chunk_idx` and `chunk_count`, so a split at the wrong
/// offset does not decrypt. Fixed-width chunks mean the boundaries are
/// arithmetic rather than a length prefix the relay would have to be trusted
/// not to rewrite.
pub const SEALED_CHUNK_LEN: usize = CHUNK_PLAINTEXT_LEN + AEAD_TAG_LEN;

/// KDF context for the per-chunk nonce. Unique to this purpose, per
/// `docs/03-crypto/primitives.md`.
///
/// `v2` because the key material changed: `v1` hashed `blob_key ||
/// u32_be(chunk_idx)`, which left the chunking unbound to the keystream. The
/// context string moves with the derivation so a `v1` and a `v2` sealer cannot
/// silently produce different nonces under the same name.
const NONCE_CONTEXT: &str = "sunrise.blob_chunk_nonce.v2";

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

/// The nonce for one chunk:
/// `BLAKE3.derive_key(ctx, blob_key || chunk_aad(blob_id, idx, count))`.
///
/// Takes the whole chunking rather than the index alone. See this module's
/// header for why the AAD bytes are the key material: the nonce has to
/// distinguish everything the AAD distinguishes, and reusing the AAD builder
/// is the only version of that which cannot fall out of step.
///
/// The material is a fixed 32-byte key followed by a self-delimiting CBOR map,
/// so no two `(blob_key, blob_id, chunk_idx, chunk_count)` tuples share an
/// encoding.
#[must_use]
pub fn chunk_nonce(
    blob_key: &[u8; AEAD_KEY_LEN],
    blob_id: &[u8; 16],
    chunk_idx: u32,
    chunk_count: u32,
) -> [u8; AEAD_NONCE_LEN] {
    let aad = chunk_aad(blob_id, chunk_idx, chunk_count);
    let mut material = Vec::with_capacity(AEAD_KEY_LEN + aad.len());
    material.extend_from_slice(blob_key);
    material.extend_from_slice(&aad);
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
    let nonce = chunk_nonce(blob_key, blob_id, chunk_idx, chunk_count);
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
    let nonce = chunk_nonce(blob_key, blob_id, chunk_idx, chunk_count);
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

/// BLAKE3 over the concatenated **ciphertext** — the name the relay stores a
/// blob under.
///
/// Distinct from [`content_hash`], which covers the plaintext, and the two are
/// not interchangeable. `POST /blobs/finalize` re-hashes what it has on disk
/// and content-addresses the committed blob at the first sixteen bytes of this
/// value, so it is the only thing that names the blob in
/// `GET /blobs/{blob_id}`. A replica holding the metadata and not the bytes
/// cannot compute it — it would need the ciphertext it is trying to fetch — so
/// it travels on the attachment op instead.
///
/// Takes the sealed chunks in order rather than one slice, because the caller
/// that has them has them chunked and concatenating first would double the
/// peak memory of a 100 MB attachment for nothing.
#[must_use]
pub fn ciphertext_hash<'a, I>(sealed_chunks: I) -> [u8; 32]
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut hasher = CiphertextHasher::new();
    for chunk in sealed_chunks {
        hasher.update(chunk);
    }
    hasher.finish()
}

/// [`ciphertext_hash`] for a caller that seals one chunk at a time.
///
/// The sealing loop writes each chunk to the blob store and drops it, so it
/// never holds the whole ciphertext — which for a 100 MB attachment is the
/// difference between one copy in memory and two. It still needs the hash over
/// all of it, so it accumulates here.
///
/// Kept in this module rather than reached for as a bare `blake3::Hasher` by
/// the caller: what is hashed, in what order, and over the *sealed* rather than
/// the plaintext bytes is part of the blob format, and a second implementation
/// of it would be a second thing to keep in step with the relay's `finalize`.
#[derive(Debug, Clone)]
pub struct CiphertextHasher(blake3::Hasher);

impl CiphertextHasher {
    /// A hasher with nothing absorbed.
    #[must_use]
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Absorb one sealed chunk. Call in chunk order; BLAKE3 is not commutative
    /// and `finalize` re-hashes the concatenation the relay stored.
    pub fn update(&mut self, sealed_chunk: &[u8]) {
        self.0.update(sealed_chunk);
    }

    /// The blob's ciphertext hash.
    #[must_use]
    pub fn finish(&self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

impl Default for CiphertextHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a concatenated blob body back into the chunks it was sealed as.
///
/// `chunk_count - 1` chunks of exactly [`SEALED_CHUNK_LEN`], then whatever
/// remains. `None` when `body` cannot be that: too short for the full chunks
/// that must precede the last, a last chunk that is empty or longer than one,
/// or a zero `chunk_count`.
///
/// Rejecting rather than truncating matters because the body arrives from the
/// relay. A short read that this accepted would reach [`open_chunk`], fail its
/// tag, and be reported as corrupt ciphertext — which is true but useless,
/// since the fault is the transfer and not the bytes.
#[must_use]
pub fn split_sealed(body: &[u8], chunk_count: u32) -> Option<Vec<&[u8]>> {
    if chunk_count == 0 {
        return None;
    }
    let full = (chunk_count as usize).checked_sub(1)?;
    let head_len = full.checked_mul(SEALED_CHUNK_LEN)?;
    if body.len() <= head_len || body.len() > head_len + SEALED_CHUNK_LEN {
        return None;
    }
    let mut out = Vec::with_capacity(chunk_count as usize);
    for i in 0..full {
        out.push(&body[i * SEALED_CHUNK_LEN..(i + 1) * SEALED_CHUNK_LEN]);
    }
    out.push(&body[head_len..]);
    Some(out)
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
    fn the_nonce_is_unique_per_chunking_and_stable_across_calls() {
        assert_eq!(
            chunk_nonce(&key(), &blob(), 0, 3),
            chunk_nonce(&key(), &blob(), 0, 3)
        );
        assert_ne!(
            chunk_nonce(&key(), &blob(), 0, 3),
            chunk_nonce(&key(), &blob(), 1, 3)
        );
        // A different key gives a different nonce at the same index, so two
        // attachments never share a (key, nonce) pair even at chunk 0.
        let mut other = key();
        other[0] ^= 1;
        assert_ne!(
            chunk_nonce(&key(), &blob(), 0, 3),
            chunk_nonce(&other, &blob(), 0, 3)
        );
        // And every other coordinate of the chunking moves it too.
        let mut elsewhere = blob();
        elsewhere[0] ^= 1;
        assert_ne!(
            chunk_nonce(&key(), &blob(), 0, 3),
            chunk_nonce(&key(), &elsewhere, 0, 3)
        );
        assert_ne!(
            chunk_nonce(&key(), &blob(), 0, 3),
            chunk_nonce(&key(), &blob(), 0, 4)
        );
    }

    /// The defect this derivation exists to close: two *different chunkings* of
    /// one blob under one key must not share a keystream.
    ///
    /// Asserted on the keystream rather than on the nonce, because the nonce is
    /// only the mechanism — what matters is that the cipher output for the
    /// same plaintext differs. Sealing identical plaintext twice makes the
    /// comparison direct: with a reused `(key, nonce)` the two ciphertexts are
    /// byte-identical, and any difference at all is proof the keystreams
    /// diverged.
    ///
    /// Before the fix this XOR was `plaintext_a XOR plaintext_b` for *any* two
    /// plaintexts, which is a two-time pad. `blob_key` being fresh per
    /// `attach_file` made it unreachable in this build; that was a property of
    /// the caller, and this is a property of the primitive.
    #[test]
    fn two_chunkings_of_one_blob_under_one_key_do_not_share_a_keystream() {
        let plaintext = b"the very same bytes, chunked two different ways";
        let as_one = seal_chunk(&key(), &blob(), 0, 1, plaintext).expect("seal as 1 of 1");
        let as_two = seal_chunk(&key(), &blob(), 0, 2, plaintext).expect("seal as 1 of 2");
        assert_ne!(
            as_one, as_two,
            "identical plaintext under one key at two chunk counts must not seal identically"
        );
        // Stronger than `!=`: not one byte of the ciphertext may agree by
        // construction, so check the keystream difference is not the
        // (all-zero) plaintext difference anywhere in the body.
        let body = plaintext.len();
        assert!(
            as_one[..body]
                .iter()
                .zip(&as_two[..body])
                .any(|(a, b)| a != b),
            "the two ciphertext bodies are identical: the keystream was reused"
        );
    }

    /// The same statement one level down: two chunkings differing only in
    /// `chunk_count` derive different nonces, so no `(key, nonce)` pair is
    /// shared. This is what makes the ciphertext test above true rather than
    /// lucky.
    #[test]
    fn chunk_count_is_bound_into_the_nonce_and_not_only_the_aad() {
        let a = chunk_nonce(&key(), &blob(), 0, 1);
        let b = chunk_nonce(&key(), &blob(), 0, 2);
        assert_ne!(a, b, "chunk_count does not reach the nonce derivation");
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

    /// A full chunk is its plaintext plus the tag, and nothing else.
    /// [`split_sealed`]'s arithmetic is that constant, so a change to the
    /// envelope that broke it would otherwise surface as an undecryptable
    /// download rather than as a failing test here.
    #[test]
    fn a_full_sealed_chunk_is_exactly_sealed_chunk_len() {
        let sealed = seal_chunk(&key(), &blob(), 0, 2, &vec![0u8; CHUNK_PLAINTEXT_LEN])
            .expect("seal a full chunk");
        assert_eq!(sealed.len(), SEALED_CHUNK_LEN);
    }

    /// Seal, concatenate the way the relay stores and streams them, split, and
    /// open: the round trip the download path walks.
    #[test]
    fn a_concatenated_blob_splits_back_into_the_chunks_it_was_sealed_as() {
        let plaintext: Vec<u8> = (0..CHUNK_PLAINTEXT_LEN * 2 + 13)
            .map(|i| (i % 251) as u8)
            .collect();
        let count = chunk_count_for(plaintext.len() as u64);
        assert_eq!(count, 3);

        let sealed: Vec<Vec<u8>> = plaintext
            .chunks(CHUNK_PLAINTEXT_LEN)
            .enumerate()
            .map(|(i, piece)| seal_chunk(&key(), &blob(), i as u32, count, piece).expect("seal"))
            .collect();
        let body: Vec<u8> = sealed.concat();

        let split = split_sealed(&body, count).expect("a well-formed body splits");
        assert_eq!(split.len(), 3);
        let opened: Vec<u8> = split
            .iter()
            .enumerate()
            .flat_map(|(i, piece)| {
                open_chunk(&key(), &blob(), i as u32, count, piece).expect("open")
            })
            .collect();
        assert_eq!(opened, plaintext);
    }

    /// The bodies a relay could hand back that are not this blob. Each is
    /// refused before any decrypt is attempted, so the caller learns the
    /// transfer was short or long rather than that the ciphertext is bad.
    #[test]
    fn a_body_that_is_not_the_right_length_does_not_split() {
        // Two chunks claimed, exactly one full chunk of body: the last chunk
        // would be empty, and no sealed chunk is.
        assert!(split_sealed(&vec![0u8; SEALED_CHUNK_LEN], 2).is_none());
        // One chunk claimed, more than one chunk of body.
        assert!(split_sealed(&vec![0u8; SEALED_CHUNK_LEN + 1], 1).is_none());
        // No chunks at all is not a blob.
        assert!(split_sealed(b"anything", 0).is_none());
        // An empty body is never a blob: every chunk carries at least a tag.
        assert!(split_sealed(b"", 1).is_none());
    }

    /// The hash the relay content-addresses by is over the sealed bytes, and a
    /// chunked walk of them must equal a single pass over their concatenation
    /// — which is what the relay's own `finalize` computes.
    #[test]
    fn the_ciphertext_hash_does_not_depend_on_how_the_bytes_are_handed_over() {
        let chunks: [&[u8]; 3] = [b"aaa", b"bbbb", b"cc"];
        assert_eq!(
            ciphertext_hash(chunks),
            *blake3::hash(b"aaabbbbcc").as_bytes()
        );
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
