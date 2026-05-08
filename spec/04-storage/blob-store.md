---
status: accepted
---

# Blob Store

Blob storage holds attachment ciphertext.

## Layout

```
$VAULT/blobs/<00..ff>/<sha256[2..]>.<chunk_idx>
```

- First two hex chars of the *plaintext* SHA-256 are used as a fanout dir (256 buckets).
- Filename is the rest of the plaintext SHA-256 plus the chunk index (e.g. `9a7c3f...e1.0`).
- Each file is one encrypted chunk (see envelope in [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)).

Naming by *plaintext* SHA-256 is **local-only** — it never reaches the network. The server stores blobs by an opaque `BlobChunkId` it generates at upload (see "Server-side storage" below); it cannot compute the plaintext SHA-256 because it never sees plaintext. The plaintext-SHA-256 file layout enables content-addressable dedup within a single vault. Cross-vault dedup is impossible by design — the server's ciphertext bytes differ for identical plaintexts because per-blob keys differ.

## Chunking

- Chunk size: 256 KiB.
- An attachment of N bytes produces ⌈N / 256K⌉ chunks.
- Chunks are streamed to/from the server independently.

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

## Integrity check

After download and decrypt, the chunk's plaintext is hashed; the per-chunk hash list (stored in the metadata op) is verified. A mismatch triggers re-fetch from a different replica or surfaces an error.

## TUI / web limitations

- TUI: stores blobs in the same dir; offers "save attachment to /tmp" command.
- Web: uses OPFS for chunk storage when available; falls back to per-session in-memory cache otherwise. Disclosed in onboarding.
