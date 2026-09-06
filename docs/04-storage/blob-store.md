---
status: accepted
---

# Blob Store

Blob storage holds attachment ciphertext.

## One blob identity

A blob has exactly one identity, and both sides derive it from the same bytes:

```
blob_id = first 16 bytes of BLAKE3(chunk_ciphertext_0 || … || chunk_ciphertext_{n-1})
```

The creating device computes it while sealing
([`../02-domain/attachments.md`](../02-domain/attachments.md)`:27`, "assigned by
the creating device"). The relay re-derives it at `finalize` from what is
actually on disk rather than trusting the client's claim
(`crates/sunrise-server/src/api/blobs.rs:222-247`), takes the same first 16
bytes as the storage key (`blob_key`, `blobs.rs:416-425`), and returns it as
`blb_` + 32 lowercase hex (`blobs.rs:272`). The two values are equal by
construction; if they were not, the upload would have failed the hash check.

That one 16-byte value is `Attachment.blob_id`
(`crates/sunrise-domain/src/attachment.rs:42-44`), the local chunk-path key, the
relay's storage key, and the `{blob_id}` path segment of `GET /api/v1/blobs/{blob_id}`.
**There is no second id and no mapping table.**

## Layout

```
$VAULT/blobs/<blob_id_hex[..2]>/<blob_id_hex>/<chunk_idx>.bin
```

Per `crates/sunrise-storage/src/blob_store.rs:4,37-44`: the first two hex
characters of the 16-byte `blob_id` are a fanout directory (256 buckets), then a
directory named by the full hex, then one file per chunk index. Each file is one
sealed chunk — opaque ciphertext, no header (see
[`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
§Blob chunks).

The relay uses the *same* `BlobStore` type and therefore the same layout, rooted
per account (§Server-side storage below).

## Chunking

- Chunk size: **256 KiB (262 144 bytes)**, per `crates/sunrise-domain/src/attachment.rs` and
  [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md).
  This file said 1 MiB until the two were reconciled; note that **no shared
  constant exists** — the size is written out in each place that needs it,
  which is why they drifted apart in the first place and will again.
- An attachment of N bytes produces ⌈N / 256 KiB⌉ chunks.
- Chunks are streamed to/from the server independently.
- **Two different numbers, often confused.** 256 KiB is the *plaintext* chunking
  unit. 1 MiB is the relay's ceiling on a single *ciphertext* chunk body
  (`MAX_CHUNK_BYTES`, `crates/sunrise-server/src/api/blobs.rs:53`), alongside a
  4096-chunk cap (`:57`) and a 100 MB per-blob cap (`:61`). A 256 KiB plaintext
  chunk seals to 256 KiB + a 16-byte tag, comfortably inside the ceiling; the
  ceiling is an anti-DoS bound on a request body, not a chunk size.

## No key-sharing dedup

> **A `blob_key` MUST NOT seal two different byte sequences.** The chunk nonce is
> derived from `blob_key ‖ u32_be(chunk_idx)` and carries no randomness
> ([`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
> §Blob chunks), so reusing a key across two distinct plaintexts at the same
> `chunk_idx` reuses an XChaCha20-Poly1305 nonce — which forfeits confidentiality
> of both messages and leaks the Poly1305 authentication key. Every sealed byte
> sequence — every attachment, every thumbnail, every re-attach of the same
> file — gets a fresh 32-byte random `blob_key`.

Consequently there is **no dedup by key sharing**, at any scope. Two attachments
of the same file are two blobs with two keys and two `blob_id`s. Earlier
revisions of this file specified exactly the forbidden thing (a second
attachment adopting the first's `blob_id` and keys); that section is deleted, not
qualified.

What this buys, beyond not being broken: content-addressing over ciphertext never
collides across attachments, which is what makes
[`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)`:52`
true — the relay learns "these two uploads are the same ciphertext", never "these
two attachments are the same file".

Dedup that *does* survive is dedup of the identical *ciphertext*: one account's
devices re-uploading the same sealed bytes converge on one stored copy, because
the id is that ciphertext's hash. That is a storage optimisation on the relay,
visible to no one else.

## Server-side storage

The relay stores the same sealed chunks under the same `blob_id`, in the same
`BlobStore` layout, rooted **per account**:

```
<blob_root>/committed/<blake3(account_id)[..16] hex>/blobs/<blob_id_hex[..2]>/<blob_id_hex>/<idx>.bin
<blob_root>/committed/<blake3(account_id)[..16] hex>/manifests/<blob_id_hex>
```

The per-account root is deliberate and load-bearing (`blobs.rs:29-36,340-350`):
content addressing without it would be a cross-tenant read primitive, since one
account could name another's blob by its hash. A grantee naming an owner's
`blb_…` therefore gets `404 BLOB_NOT_FOUND` — "No committed blob under that id
**for this account**" ([`../06-server/api.md`](../06-server/api.md) §Blobs). This
is one of the reasons cross-user sharing is post-v1
([ADR-0027](../11-adr/0027-v1-self-host-first.md)).

The manifest (`"<chunk_count> <size_bytes>"`) is written **after** every chunk
(`blobs.rs:254-266`), so a crash mid-commit leaves an invisible partial rather
than a short read: a reader that finds no manifest sees no blob.

Upload is three calls, not one: `POST /blobs/init` reserves an `up_…` id and
hands back one URL per chunk, `PUT /blobs/{upload_id}/{idx}` uploads each sealed
chunk, `POST /blobs/finalize` verifies and commits. The per-blob key never goes
near any of them — it rides inside the attachment op envelope.

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

- Any unrevoked attachment op in any non-compacted Stream references its `blob_id`.
- OR within 30 days of upload (regardless of references).

A cron job runs every 4 hours; chunks not meeting either condition are deleted. Self-hosters can override `[blob] gc_grace_days = 30` and `[blob] gc_interval_hours = 4`.

## Integrity model

There is no `BlobMeta` structure. The metadata a blob has is the `Attachment`
entity itself — `{blob_key, blob_id, chunk_count, content_hash, size_bytes,
mime_type, filename}`, specified at
[`../02-domain/attachments.md`](../02-domain/attachments.md)`:17-33` and modelled
at `crates/sunrise-domain/src/attachment.rs:38-50`. Earlier revisions of this
file specified a parallel CDDL with a per-chunk `chunk_id`; both the structure
and the second id are gone.

Integrity comes from three facts, each at a different layer:

1. **`Attachment.content_hash` is the BLAKE3 of the concatenated *plaintext*.**
   The client checks it after reassembly and decrypt. It is the end-to-end
   guarantee, and it is the only hash the relay cannot compute.
2. **`FinalizeRequest.chunk_hashes` are BLAKE3 of each *ciphertext* chunk**, and
   `FinalizeRequest.content_hash` is BLAKE3 of the concatenated ciphertext
   (`blobs.rs:93-97`). The relay re-hashes what is on disk and refuses on
   mismatch (`blobs.rs:236-247`, `sunrise_blob_hash_mismatch_total`), which
   catches a chunk that never arrived, arrived truncated, or arrived corrupted —
   at upload time, rather than months later on another device.
3. **The XChaCha20-Poly1305 tag inside each sealed chunk** catches any
   modification of stored bytes at open time, whoever made it.

Note that `content_hash` means two different digests in the two places it
appears: a **plaintext** digest on the `Attachment` (rule 1) and a **ciphertext**
digest in `FinalizeRequest` (rule 2). They are not interchangeable and neither is
derivable from the other.

## CLI / web limitations

- CLI: no attachment commands. `sunrise` is one-shot and does not fetch or
  write blobs; attachments are reachable only from the macOS client.
- Web: uses OPFS for chunk storage when available; falls back to per-session in-memory cache otherwise. Disclosed in onboarding.
