---
status: accepted
---

# Attachments

Files attached to Tasks, Notes, or Streams. The file content lives in the encrypted blob store; metadata lives in the op log.

## Fields

```cddl
Attachment = {
    id:            tstr .regexp "att_[A-Z0-9]{26}",
    created_at:    tdate,
    parent:        EntityRef,           ; task, stream, note (only on a NoteBody-bearing entity)
    filename:      text<256>,
    mime_type:     text<128>,
    size_bytes:    uint .size 8,
    sha256:        bstr .size 32,       ; of plaintext, for dedup and integrity
    blob_ref:      BlobRef,             ; opaque pointer into blob store (per-blob key wrapped)
    width?:        uint,                ; image only
    height?:       uint,
    thumbnail_ref?: BlobRef,             ; image only; max edge 512 px; AVIF preferred
    duration_ms?:  uint,                ; audio/video
    deleted:       bool,
}
```

## Storage

- **Plaintext** is never on the wire and never stored unencrypted at rest.
- Each attachment has a per-blob symmetric key, generated client-side.
- Plaintext is encrypted with that key (AEAD; chunked so streaming partial reads work).
- The wrapped key is stored in `BlobRef` along with the blob ID and chunking parameters.
- The encrypted blob is uploaded to the server (or kept LAN-local in T3 topologies).

## Size policy

- Hard limit per attachment: 100 MB on managed cloud (configurable on self-host).
- Recommended UX: nudge user toward external storage links (Drive, Dropbox) for files > 25 MB.
- Total per-vault quota: tiered (see [`../06-server/billing.md`](../06-server/billing.md)).

## Lazy fetch

Attachments are not pre-fetched on sync. Each device pulls on first view, decrypts, caches locally with an LRU. Cache size is configurable per device.

- Default auto-fetch threshold: **10 MiB**. Smaller attachments fetch silently on first view.
- Larger attachments show an inline placeholder with file name, size, and a "Download" button. The "Cancel" button during transfer aborts and marks the attachment `partial: true` in cache.
- Re-tapping a `partial: true` attachment retries from byte 0 (chunks are 1 MiB each; the cache uses chunk granularity but resume-from-partial is not implemented in v1).
- Cellular vs Wi-Fi: per-platform setting `auto_fetch_on_cellular: bool = false`.

## TUI / web limitations

- TUI cannot render most attachments. It shows metadata and can save-to-disk.
- Web in private-browsing mode cannot persist large attachment caches; falls back to per-session memory cache.

## Deletion

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
- Stored as a separate small blob with the same per-blob key as the original (no extra key envelope).
- The thumbnail's `BlobRef` is recorded in a `thumbnail_ref` field on the Attachment metadata op.

Generating on receivers is rejected for v1: it would require every receiver to fetch the full ciphertext just to thumbnail, defeating the lazy-fetch policy.
