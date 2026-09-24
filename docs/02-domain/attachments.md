---
status: accepted
---

# Attachments

Files attached to Tasks and Notes. The file content lives in the encrypted blob
store; metadata lives in the op log. Thumbnails, native rendering, the local
cache and the cellular gate are decided in
[ADR-0053](../11-adr/0053-attachment-thumbnails-and-native-rendering.md).

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

What a device writes and signs today:

```cddl
Attachment = {
    id:            tstr .regexp "att_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:    timestamp,
    updated_at:    timestamp,
    parent:        entity-ref,          ; Task today; Task or Note per §Parents
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

### Thumbnail and dimensions (specified, not yet modelled)

Additive fields, registered through the entity registry, gated by the feature
id `attachment.thumbnail` in `vault_requires`, and preserved by an older build
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md),
[ADR-0053](../11-adr/0053-attachment-thumbnails-and-native-rendering.md)).
`crates/sunrise-domain/src/attachment.rs:23#Attachment` has none of them yet
([#346](https://github.com/justin13888/Sunrise/issues/346)):

```cddl
; All optional; all written with the attachment and never changed.
width?:                  uint,            ; original, px, when the source knows it
height?:                 uint,
thumbnail_blob_id?:      bstr .size 16,
thumbnail_blob_key?:     bstr .size 32,   ; fresh key; never the original's
thumbnail_mime?:         "image/jpeg" / "image/png",
thumbnail_size_bytes?:   uint,            ; <= 262_144 (one chunk)
thumbnail_content_hash?: bstr .size 32,   ; BLAKE3 of the thumbnail plaintext
```

The five `thumbnail_*` fields are present together or not at all. A thumbnail
is always one chunk, so it has no `chunk_count`. The blob reference is
flattened into fields rather than nested, so the op-envelope shape has no
sub-record to version separately. The digest is BLAKE3, matching the hash used
everywhere else, including the relay's finalize check
([`../06-server/api.md`](../06-server/api.md) §Blobs).

## Parents

An attachment's `parent` is a **Task** or a **Note**.

- **Task** is accepted today. `AttachmentDraft::validate` rejects every other
  kind (the `attachment.parent` constraint).
- **Note** is accepted once the `Note` entity has a command path
  ([`notes.md`](./notes.md)): a note is where a document about a Stream or a
  Block belongs, so attaching to a note covers them without a parent kind per
  entity. Until then, validation keeps refusing it.
- **Streams, Blocks and Routines are not parents.** A routine's attachment would
  repeat on every occurrence; a stream or block attachment is a note with one
  attachment.

## Write-once metadata

An Attachment is created and thereafter only tombstoned. Every field but
`deleted` describes one specific run of ciphertext identified by
`content_hash`, so changing one would be describing different bytes: there is
no update op and no patch type. Re-attaching an edited file is a new
attachment, which is what content addressing already implies. The same holds
for the thumbnail: it is written with the attachment or never, and a later
device does not add one.

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

- Hard limit per attachment: **100 MB, fixed** (`MAX_BLOB_BYTES`, `crates/sunrise-server/src/api/blobs.rs:61`). It is a compile-time constant, not an operator setting and not a plan tier. The constant's own doc comment still describes it as "100 MB on managed cloud, configurable on self-host", which [ADR-0027](../11-adr/0027-v1-self-host-first.md) retires; correcting that comment is a code change.
- Recommended UX: nudge the user toward external storage links (Drive, Dropbox) for files > 25 MB.
- No per-vault quota. The only ceiling is the fixed 100 MB per attachment above; there is no per-account accounting to charge a vault total against ([ADR-0027](../11-adr/0027-v1-self-host-first.md)).

## Image previews (thumbnails)

The **source device** makes the thumbnail at attach time, with its platform's
own APIs (ImageIO and QuickLook Thumbnailing on Apple), for whatever the
platform can thumbnail: images, the first page of a PDF, a video's poster frame,
documents where the OS offers one.

- **Format:** JPEG (quality 0.8) when the source has no alpha, PNG when it has.
  Never AVIF, HEIC or WebP, because the receiver must decode it and every
  platform decodes JPEG and PNG natively.
- **Size:** longest edge at most 512 px, never upscaled, orientation applied,
  sRGB.
- **No metadata:** EXIF, GPS and XMP are stripped, so a thumbnail never leaks a
  photo's location to a device that did not fetch the photo.
- **One chunk:** at most 256 KiB. Over the limit, a JPEG is re-encoded at 0.6
  then 0.4, a PNG is downscaled to 384 px then 256 px, and if it still does not
  fit the attachment has no thumbnail.
- **Its own blob, with its own fresh random `blob_key` and `blob_id`.** It MUST
  NOT reuse the original's key: the chunk nonce is derived from
  `blob_key ‖ u32_be(chunk_idx)` and carries no randomness, so one key over two
  different plaintexts is a nonce reuse
  ([`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
  §Blob chunks).
- **Always auto-fetched**, whatever the size threshold, except on a network the
  OS marks constrained.

Generating thumbnails on receivers is rejected: every receiver would have to
fetch the full ciphertext just to make one, defeating lazy fetch.

## Rendering

Clients render only what their platform decodes natively, and hand everything
else to the system.

| Platform | Inline preview | Otherwise |
|---|---|---|
| macOS | `Image` (ImageIO), PDFKit, and `QLPreviewView` for every type QuickLook supports | **Open in…** (`NSWorkspace` open-with) |
| iOS / iPadOS | `Image`, PDFKit, `QLPreviewController` | **Open in…** (share sheet) |
| Android | `ImageDecoder`, `PdfRenderer` | **Open in…** (`ACTION_VIEW` chooser) |
| Windows, Linux | the platform image and PDF viewers' components | **Open in…** (system default handler) |

Clients never bundle a third-party decoder. Decrypted plaintext handed to a
previewer or to "Open in…" goes to a per-launch temporary directory, is deleted
when the preview closes and at the next launch, and is not counted as cache.

## Lazy fetch

Attachments are not pre-fetched on sync. Each device pulls on first view,
verifies and keeps the sealed chunks in its local blob store.

- **Auto-fetch threshold: 10 MiB** (`crates/sunrise-core/src/blob_sync.rs:164`).
  Smaller attachments fetch without asking on first view.
- Larger attachments show an inline placeholder with file name, size and a
  **Download** button. **Cancel** during transfer aborts and marks the request
  `partial: true`.
- Re-tapping a `partial: true` attachment retries from byte 0. Chunks are
  256 KiB each ([`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
  §Blob chunks); resume from a partial transfer is not part of the design,
  because a cancelled transfer's chunks are discarded.
- **Cellular:** the client reports the network class (`unmetered`, `cellular`,
  `constrained`) and the Rust fetch drain decides. The device-local preference
  `attachments.auto_fetch_on_cellular` (default **off**) gates unasked fetches on
  cellular. An explicit Download always proceeds; over the threshold on
  cellular, it first confirms the size.

**What is built** ([#176](https://github.com/justin13888/Sunrise/issues/176)).
The byte path is: `Core::attach_file` seals the chunks into the local
`BlobStore` and queues the blob in `blob_uploads`; `sunrise_core::sync_driver`
drains that queue over the relay's `init` → `PUT` → `finalize`; and a device
that holds an attachment's metadata and not its chunks fetches them with
`GET /blobs/{blob_id}`, checking the AEAD tag on every chunk and the
`content_hash` on the whole before anything is written.
`crates/sunrise-core/src/blob_sync.rs` carries the reasoning; the end-to-end
proof is `crates/sunrise-e2e/tests/attachment_bytes_round_trip.rs`.

The 10 MiB threshold bounds only what is fetched *unasked*
([#227](https://github.com/justin13888/Sunrise/issues/227)).
`Core::fetch_attachment` (`crates/sunrise-core/src/blob_fetch.rs:207#fetch_attachment`)
asks for one named attachment whatever its size: it writes a `blob_fetches`
row, the sync driver drains it with no size predicate, and the call returns
when the chunks are here. `Core::cancel_attachment_fetch`
(`crates/sunrise-core/src/blob_fetch.rs:306#cancel_attachment_fetch`) is the
Cancel button, and it marks the request `partial` without needing the relay to
agree, because a stalled transfer is what people cancel. Both are on the seam,
behind the pane's Download and Cancel controls
(`apps/apple/Sunrise/Views/AttachmentsView.swift:99`) and behind
`sunrise attachment get` / `cancel`.

`partial: true` lives on the local request row rather than on the attachment,
because it is a fact about this device's cache, and the attachment row is a
replicated write-once entity.

**Not built** ([#346](https://github.com/justin13888/Sunrise/issues/346)): thumbnails, QuickLook, the cache below, and the
cellular gate. Today a fetched blob is kept forever.

## Local cache

The Rust core owns a least-recently-used cache over the sealed chunks in the
local blob store. It is device-local: nothing about it syncs.

- **Size:** default **1 GB** on desktop and **200 MB** on phone and tablet
  (the client passes its device class), configurable per device from 100 MB to
  50 GB (`attachments.cache_limit_bytes`, device-local).
- **Recency:** each preview or open stamps a last-access time in a local table.
- **Eviction** runs after each fetch and at launch, least recently used first,
  until usage is under the limit. It never evicts:
  - a blob with an upload pending or unfinished (this device may be its only
    holder);
  - a thumbnail;
  - a blob open in a preview right now.
- **A re-fetch after eviction** is the ordinary lazy-fetch path.
- **Settings → Storage** shows usage, the limit, **Clear cache** (evicts every
  evictable blob) and the cellular toggle.

## Client limitations

- `sunrise-cli` reads attachments and downloads them — `sunrise attachments
  <task-id>`, `sunrise attachment get <id> [path]`, `sunrise attachment cancel
  <id>` — and cannot *create* one: no subcommand reaches `AttachFile` or
  `DetachFile`, because attaching starts from a file the user picked and that
  is a GUI act. `attachment` is one of only two verbs that start a sync driver
  (the other is `sync`), since a download with no driver is a request nothing
  can service.
- The attachments *pane* is Apple-only today, and it is *shared* rather than
  macOS-only: `apps/apple/Sunrise/Views/AttachmentsView.swift` and
  `apps/apple/Sunrise/Tasks/AttachmentsModel.swift` compile into both targets
  and are reached from the task editor on each
  (`apps/apple/Sunrise/Views/TaskEditorView.swift:93`).
- Web in private-browsing mode cannot persist large attachment caches; it falls
  back to a per-session memory cache.

## Deletion

`Command::DetachFile` performs the logical delete and is all of this that
belongs in the core. The GC below needs the device-cursor quorum and the grace
period, both of which are the relay's; the relay's blob `DELETE` route is
correspondingly not implemented yet ([#359](https://github.com/justin13888/Sunrise/issues/359)).

Logical delete sets `deleted=true`. The encrypted blob — and its thumbnail blob —
is **garbage-collected** when:

1. All active devices have acknowledged the tombstone, AND
2. A grace period of 30 days has elapsed.

Acknowledgement is **explicit** via a `device_op_cursor` op emitted by each device every 24 hours and on shutdown. The op carries `(device_id, max_op_seq_observed_per_stream)`. A blob is GC-eligible when:

```
all_active_devices.every(d => d.cursor[stream_id] >= tombstone_op.seq)
  AND now - tombstone_op.ts_ms >= 30 days
```

`active_device` = a device that has emitted a cursor op in the last 30 days. A device silent for > 30 days is considered abandoned and excluded from the quorum; re-pairing or re-syncing produces a fresh cursor that takes effect immediately.

The server runs the GC; only the **blobs** are GC'd, never the metadata, since metadata reveals nothing without the wrapped key.
