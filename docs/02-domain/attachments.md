---
status: accepted
---

# Attachments

Files attached to Tasks. The file content lives in the encrypted blob store;
metadata lives in the op log.

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

This is the **v1 shape** — what a device actually writes and signs today:

```cddl
Attachment = {
    id:            tstr .regexp "att_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:    timestamp,
    updated_at:    timestamp,
    parent:        entity-ref,          ; TASKS ONLY in v1; validated, not merely conventional
    filename:      text<256>,
    mime_type:     text<128>,
    size_bytes:    uint,                ; 1 .. 100_000_000; see §Size policy
    blob_key:      bstr .size 32,       ; per-blob symmetric key; sealed inside the op envelope
    blob_id:       bstr .size 16,       ; assigned by the creating device
    chunk_count:   uint,                ; >= 1
    content_hash:  bstr .size 32,       ; BLAKE3 of plaintext; end-to-end integrity
    deleted:       bool,
    unknown-fields,                     ; see overview.md
}
```

`parent` is **Task-only in v1** and `AttachmentDraft::validate` rejects any
other kind with `attachment.parent / task_only_in_v1`. Earlier revisions of
this spec allowed Stream and Note parents; a Note has no command path at all
([`notes.md`](./notes.md)), so a Note-parented attachment could never have been
created.

### Specified but not modelled

The thumbnail slice. Not on the wire today; `unknown-fields` round-trips them
through a build that lacks them:

```cddl
; Not yet modelled. Landing these is a DOC_SCHEMA_V bump, not a break.
width?:         uint        ; image only
height?:        uint
thumbnail_ref?: BlobRef     ; image only; max edge 512 px; AVIF preferred
duration_ms?:   uint        ; audio/video
```

The `BlobRef` in earlier drafts is flattened into the three fields that make
one up — `blob_key`, `blob_id`, `chunk_count` — rather than nested, so the
op-envelope shape has no sub-record to version separately. The digest is
BLAKE3, not SHA-256, matching the hash used everywhere else in the system
including the relay's finalize check
([`../06-server/api.md`](../06-server/api.md) §Blobs).

## Write-once metadata

An Attachment is created and thereafter only tombstoned. Every field but
`deleted` describes one specific run of ciphertext identified by
`content_hash`, so changing one would be describing different bytes: there is
no update op and no patch type. Re-attaching an edited file is a new
attachment, which is what content addressing already implies.

The two commands are `AttachFile` and `DetachFile`; the read is
`Query::TaskAttachments`.

`blob_key` is generated **client-side, before upload** — the file has to be
sealed under it to be uploadable at all, so the core cannot mint it after the
fact. It is the one secret the metadata carries, and losing it orphans the
blob on every other device, so it rides in the op envelope like the rest of
the entity.

## Storage

- **Plaintext** is never on the wire and never stored unencrypted at rest.
- Each attachment has a per-blob symmetric key, generated client-side.
- Plaintext is encrypted with that key (AEAD; chunked so streaming partial reads work).
- The key is carried on the Attachment itself (`blob_key`) alongside the blob id and chunk count, sealed inside the op envelope.
- The encrypted blob is uploaded to the server. There is no LAN-local option: [`../01-architecture/deployment-topologies.md`](../01-architecture/deployment-topologies.md) defines two topologies, neither of which is LAN-only.

## Size policy

- Hard limit per attachment: 100 MB on managed cloud (configurable on self-host).
- Recommended UX: nudge user toward external storage links (Drive, Dropbox) for files > 25 MB.
- No per-vault quota. The only ceiling is the fixed 100 MB per attachment above (`crates/sunrise-server/src/api/blobs.rs:61`); v1 has no per-account accounting to charge a vault total against ([ADR-0027](../11-adr/0027-v1-self-host-first.md)).

## Lazy fetch

Attachments are not pre-fetched on sync. Each device pulls on first view, decrypts, caches locally with an LRU. Cache size is configurable per device.

- Default auto-fetch threshold: **10 MiB**. Smaller attachments fetch silently on first view.
- Larger attachments show an inline placeholder with file name, size, and a "Download" button. The "Cancel" button during transfer aborts and marks the attachment `partial: true` in cache.
- Re-tapping a `partial: true` attachment retries from byte 0 (chunks are 256 KiB each, per [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md) §blob-chunks; the cache uses chunk granularity but resume-from-partial is not implemented in v1).
- Cellular vs Wi-Fi: per-platform setting `auto_fetch_on_cellular: bool = false`.

> **Lazy fetch is not reachable in v1.** There is no client-side uploader:
> `Core::attach_file` seals chunks into the *local* vault's `BlobStore` and the
> metadata op syncs, but nothing drives the relay's
> `init` → `PUT` → `finalize` flow, which is implemented and tested
> server-side. So a paired device receives an Attachment's metadata and cannot
> fetch its bytes. `Core::attachment_is_local` is the query that distinguishes
> the two cases. Everything above describes the fetch policy for when the
> uploader lands.

## Client limitations

- `sunrise-cli` has no attachment surface at all — no subcommand reaches
  `AttachFile`, `DetachFile` or `TaskAttachments`. Attachments are macOS-only
  in v1 (`apps/apple/Sunrise/Tasks/AttachmentsModel.swift`).
- Web in private-browsing mode cannot persist large attachment caches; falls back to per-session memory cache.

## Deletion

`Command::DetachFile` performs the logical delete and is all of this that
belongs in the core. The GC below needs the device-cursor quorum and the grace
period, both of which are the relay's; `DELETE /api/v1/blobs/<id>` is
correspondingly not implemented yet.

Logical delete sets `deleted=true`. The encrypted blob is **garbage-collected** when:

1. All active devices have acknowledged the tombstone, AND
2. A grace period of 30 days has elapsed.

Acknowledgement is **explicit** via a `device_op_cursor` op emitted by each device every 24 hours and on shutdown. The op carries `(device_id, max_op_seq_observed_per_stream)`. A blob is GC-eligible when:

```
all_active_devices.every(d => d.cursor[stream_id] >= tombstone_op.seq)
  AND now - tombstone_op.ts_ms >= 30 days
```

`active_device` = a device that has emitted a cursor op in the last 30 days. A device silent for > 30 days is considered abandoned and excluded from the quorum; re-pairing or re-syncing produces a fresh cursor that takes effect immediately.

The server runs the GC; only the **blob** is GC'd, never the metadata, since metadata reveals nothing without the wrapped key.

## Image previews (thumbnails)

Image attachments include a thumbnail generated on the **source device** at attach time:

- Max edge: 512 px; format: AVIF (fallback JPEG for platforms without AVIF encode).
- Stored as a **separate blob with its own fresh random `blob_key` and its own
  `blob_id`**. It MUST NOT reuse the original's key: the chunk nonce is derived
  from `blob_key ‖ u32_be(chunk_idx)` and carries no randomness, so one key over
  two different plaintexts is a nonce reuse
  ([`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
  §Blob chunks). Earlier revisions of this file specified the key sharing
  explicitly, on the grounds that it saved a key envelope; the saving is not
  worth the property it destroys.
- *Target state.* `crates/sunrise-domain/src/attachment.rs` models no thumbnail:
  there is no `thumbnail_ref` field, no second `blob_key`, and no generation
  path. Whatever shape it lands in has to carry both a key and an id, not a
  reference alone.

Generating on receivers is rejected for v1: it would require every receiver to fetch the full ciphertext just to thumbnail, defeating the lazy-fetch policy.
