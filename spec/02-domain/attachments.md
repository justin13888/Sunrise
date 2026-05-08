---
status: draft
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

## TUI / web limitations

- TUI cannot render most attachments. It shows metadata and can save-to-disk.
- Web in private-browsing mode cannot persist large attachment caches; falls back to per-session memory cache.

## Deletion

Logical delete sets `deleted=true`. The encrypted blob is **garbage-collected** when:

1. All devices have acknowledged the tombstone, AND
2. A configurable grace period (default 30 days) has elapsed.

The server runs the GC; only the **blob** is GC'd, never the metadata, since metadata reveals nothing without the wrapped key.

## Open

> **Open:** Image previews require thumbnails. Generate on the source device and store as a separate (small) blob with its own key, or generate on-demand per viewer? Defer; both have tradeoffs.
