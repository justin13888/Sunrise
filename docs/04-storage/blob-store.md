---
status: accepted
---

# Blob Store

Blob storage holds attachment ciphertext.

## Layout

```
$VAULT/blobs/<00..ff>/<plaintext_blake3_hex[2..]>.<chunk_idx>
```

- First two hex chars of the *plaintext* `BLAKE3(plaintext_chunk, 32)` are used as a fanout dir (256 buckets).
- Filename is the remaining hex of the plaintext hash plus the chunk index (e.g. `9a7c3f...e1.0`).
- Each file is one encrypted chunk (see envelope in [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)).

Naming by *plaintext* hash is **local-only** — it never reaches the network. The server stores blobs by an opaque `BlobChunkId` it generates at upload (see "Server-side storage" below); it cannot compute the plaintext hash because it never sees plaintext. The plaintext-hash file layout enables content-addressable dedup within a single vault. Cross-vault dedup is impossible by design — the server's ciphertext bytes differ for identical plaintexts because per-blob keys differ.

## Chunking

- Chunk size: **256 KiB (262 144 bytes)**, per `crates/sunrise-domain/src/attachment.rs` and
  [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md).
  This file said 1 MiB until the two were reconciled; note that **no shared
  constant exists** — the size is written out in each place that needs it,
  which is why they drifted apart in the first place and will again.
- An attachment of N bytes produces ⌈N / 256 KiB⌉ chunks.
- Chunks are streamed to/from the server independently.

## Local plaintext-hash dedup

- Local dedup is **automatic**: a `BLAKE3(plaintext_chunk, 32)` is computed during chunking. If a row in `attachment` already has the same plaintext hash, the new attachment shares its `blob_id` and chunk encryption keys (re-encrypted under a fresh nonce per chunk).
- Plaintext hashes never leave the device.
- Dedup applies across Streams owned by the same vault. Cross-vault dedup is impossible (per-blob keys differ).

## Server-side storage

The server stores the same chunks (uploaded), keyed by an opaque `BlobChunkId` that is unrelated to the plaintext hash (the server cannot compute the plaintext hash). The mapping `(blob_id, chunk_idx) → BlobChunkId` lives in the metadata op for the attachment.

This means: one logical blob = one metadata op + N upload requests for chunks. The metadata op carries the wrapped key.

## Lazy fetch

A device decides per-attachment when to fetch. Default policies:

| Class | Auto-fetch? |
|---|---|
| Image referenced in a Note currently rendering | Yes |
| Attachment on Today's tasks | Yes |
| Older attachment | On click |
| Mobile on metered network | "On click" only by default |

## Cache eviction

Per-device LRU. Default cap: 1 GB on desktop, 200 MB on mobile, 50 MB in browser. Evicted blobs can be re-fetched; eviction does not affect the metadata or the user's logical "this attachment exists" state.

A chunk's cache row has a `state` field: `absent | downloading | cached | partial | evict_pending`.

- Eviction targets `cached` rows oldest-first; `downloading` rows are never evicted.
- If a `cached` row would be evicted but a fetch references it, eviction is skipped and the row stays.
- A `downloading` row's bytes are kept on disk until either completion (→ `cached`) or cancellation (→ `partial`); a partial row may be GC'd after 7 days unreferenced.

## Server-side chunk GC

The server retains a chunk while:

- Any unrevoked metadata op in any non-compacted Stream references its `BlobChunkId`.
- OR within 30 days of upload (regardless of references).

A cron job runs every 4 hours; chunks not meeting either condition are deleted. Self-hosters can override `[blob] gc_grace_days = 30` and `[blob] gc_interval_hours = 4`.

## Integrity model

Chunk metadata (carried in the metadata op) is:

```cddl
BlobMeta = {
    blob_id:    bstr .size 16,
    size:       uint,
    mime:       tstr,
    chunk_size: uint,                    ; bytes; 1048576 = 1 MiB in v1
    chunks:     [+ {
                    chunk_id:  bstr,     ; opaque server-side id; ≤ 64 bytes
                    hash:      bstr .size 32,  ; BLAKE3(plaintext_chunk, 32)
                  }],
    blob_hash:  bstr .size 32,           ; BLAKE3 over concatenated plaintext chunk hashes, 32
}
```

Each chunk carries **one** hash: `BLAKE3(plaintext_chunk, 32)`, used for local dedup and end-to-end integrity. Per-chunk integrity at decrypt time is provided by the AEAD (ChaCha20-Poly1305) tag on the ciphertext envelope. The server validates upload integrity via TLS plus the storage backend's own ETag (S3 returns a content hash; for the local-disk backend the server computes `BLAKE3(ciphertext, 32)` on receipt). There is no separate `enc_hash` field in metadata.

After download and decrypt, the client verifies each chunk's `BLAKE3(plaintext_chunk, 32)` against the metadata; on full assembly it verifies `blob_hash`. A mismatch triggers re-fetch from a different replica or surfaces an error.

## CLI / web limitations

- CLI: no attachment commands. `sunrise` is one-shot and does not fetch or
  write blobs; attachments are reachable only from the macOS client.
- Web: uses OPFS for chunk storage when available; falls back to per-session in-memory cache otherwise. Disclosed in onboarding.
