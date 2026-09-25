# 0053 — The source device makes a JPEG or PNG thumbnail as its own blob, clients render only what their platform decodes natively, and a core-owned LRU bounds the local copy

**Status:** accepted

**Amends** [`../02-domain/attachments.md`](../02-domain/attachments.md): its
§Image previews specified AVIF with a JPEG fallback; this replaces it. It also
widens `Attachment.parent` from tasks to tasks and notes.

**Depends on** [ADR-0045](./0045-schema-identity-and-feature-gating.md) (new
fields are registered and older builds preserve them) and
[ADR-0050](./0050-preferences-and-day-schedule.md) (device-local preferences).

**Tracked by** [#346](https://github.com/justin13888/Sunrise/issues/346). Server-side blob GC is out of scope here and travels
with account deletion ([#359](https://github.com/justin13888/Sunrise/issues/359)).

## Context

The byte path works end to end: chunks are sealed on attach, uploaded by the
sync driver, and fetched on demand
(`crates/sunrise-core/src/blob_fetch.rs:207#fetch_attachment`); anything under
`AUTO_FETCH_MAX_BYTES` (`crates/sunrise-core/src/blob_sync.rs:164`,
10 MiB) is fetched unasked. What is missing is everything that makes
attachments pleasant and bounded:

- **No thumbnails.** `Attachment` (`crates/sunrise-domain/src/attachment.rs:23#Attachment`)
  has no thumbnail fields, so a receiving device sees an image only after
  fetching the whole blob. The spec named **AVIF**, which not every platform can
  *encode* natively (the source device must encode), and which older decoders
  cannot read.
- **Previews are hand-picked.** The Apple pane previews images and PDFs inline
  (`apps/apple/Sunrise/Views/AttachmentsView.swift`) and sends every other type
  to "Open" via a temp file. QuickLook, which previews dozens of types
  out-of-process, is unused.
- **Nothing bounds the local copy.** A fetched blob is kept forever; there is no
  cache limit and no cellular gate, although `auto_fetch_on_cellular` was
  specified.
- **Tasks only.** `AttachmentDraft::validate` refuses every parent but a task.

## Decision

**1. The source device makes the thumbnail, at attach time, with platform
APIs.** On Apple, ImageIO and QuickLook Thumbnailing; on Android,
`ImageDecoder`/`PdfRenderer`/`MediaMetadataRetriever`; on Windows, the shell
thumbnail provider; on Linux, the desktop thumbnailer. It covers whatever the
platform can thumbnail: images, PDFs (first page), videos (a poster frame) and
documents where the OS offers one.

- **Format:** **JPEG** (quality 0.8) when the source has no alpha, **PNG** when
  it does. Never AVIF, HEIC or WebP: every platform *decodes* JPEG and PNG
  natively, and the receiver is the one that must.
- **Size:** longest edge at most **512 px**, never upscaled, orientation
  applied, colour converted to sRGB.
- **Metadata stripped:** no EXIF, GPS, XMP or ICC beyond sRGB. A thumbnail must
  not leak a photo's location to a device that never fetched the photo.
- **One chunk:** at most 256 KiB, the blob chunk size. A JPEG over the limit is
  re-encoded at quality 0.6, then 0.4; a PNG over the limit is downscaled to
  384 px, then 256 px; if it still does not fit, the attachment has no
  thumbnail.

**2. A thumbnail is its own blob, with its own key.** Fresh `blob_key` and
`blob_id`, because the chunk nonce is derived from `blob_key ‖ chunk_idx` and a
key must never seal two plaintexts
([`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md)
§Blob chunks). `Attachment` gains additive fields: `width` and `height` (of the
original, when known), and `thumbnail_blob_id`, `thumbnail_blob_key`,
`thumbnail_mime` (`image/jpeg` or `image/png` only), `thumbnail_size_bytes`,
`thumbnail_content_hash`. They are written with the attachment, in the same
`AttachFile` op, and are write-once like every other attachment field. An
attachment made by a device that could not thumbnail it has no thumbnail; a
later device does not add one.

**3. Thumbnails always auto-fetch**, whatever the size threshold and on any
network except one the OS marks constrained (Low Data Mode, metered with data
saver on).

**4. Clients render only what their platform decodes natively.** A client
previews a type inline if a system framework renders it — on Apple, `Image`
via ImageIO, PDFKit, and QuickLook (`QLPreviewView` on macOS,
`QLPreviewController` on iOS) for everything else QuickLook supports. Anything
the platform cannot render gets **Open in…** (the system's open-with or share
sheet). Clients never bundle a third-party decoder. A received thumbnail whose
`thumbnail_mime` is not JPEG or PNG is ignored.

**5. A core-owned LRU cache bounds the local copy.** The Rust core tracks the
sealed chunks it keeps in the local blob store, with a last-access time stamped
on each preview or open (a local table, never synced). Defaults: **1 GB on
desktop, 200 MB on phone and tablet**, configurable per device from 100 MB to
50 GB (`attachments.cache_limit_bytes`, in bytes). Eviction runs after each fetch and at launch, least recently used first,
and never evicts:

- a blob with an upload pending or unfinished (this device may be its only
  holder);
- a thumbnail (they are small and they are what makes the list usable offline);
- a blob open in a preview right now.

Decrypted plaintext handed to QuickLook or "Open in…" is written to a
per-launch temporary directory, is deleted when the preview closes and at the
next launch, and is not counted as cache.

**6. Cellular is a device-local gate, decided in Rust.** The client reports the
network class (`unmetered`, `cellular`, `constrained`); the fetch drain decides.
`attachments.auto_fetch_on_cellular` (default **off**) gates unasked fetches on
cellular. An explicit Download always proceeds, after a confirmation that names
the size when it is over the auto-fetch threshold on cellular.

**7. Parents are tasks and notes.** A note is where a document belongs when it
is about a stream or a block rather than a task, so attaching to a note covers
those without a parent kind per entity. Streams, blocks and routines are not
parents: a routine's attachment would repeat on every occurrence, and a stream
or block attachment is a note with one attachment. Note parents are accepted
once the `Note` entity has a command path ([`../02-domain/notes.md`](../02-domain/notes.md));
until then validation still refuses them.

## Alternatives considered

| Option | Why not |
|---|---|
| **AVIF (the previous spec)** | Smaller, and not natively *encodable* on every platform the source might be (older Apple OS versions, Windows without the extension, Linux). The receiver's decode path also varies. JPEG/PNG at 512 px fit in one 256 KiB chunk anyway, so the size win buys little. |
| **HEIC on Apple sources** | Native on Apple, absent or licensed elsewhere. |
| **Receivers make thumbnails** | Every receiver would fetch the full blob to make a preview, defeating lazy fetch. |
| **Thumbnail under the original's key** | Nonce reuse, as above. |
| **Inline thumbnail bytes in the op** | Bloats every op replay and snapshot with image bytes, and puts image data into the path the relay's op-size limits govern. A blob keeps ops small and reuses the fetch path. |
| **Bundle decoders (libheif, a PDF engine, office parsers)** | Large, and each is a decoder of attacker-controlled bytes running in-process. QuickLook and the platform frameworks run out-of-process and are patched by the OS. |
| **A cache owned by each client** | Each client would re-implement accounting and eviction, and none could see pending uploads, which only the core knows. |
| **Attachments on any entity** | More parent kinds for the same user need; a note already carries rich text and is the natural home for a document. |

## Consequences

- `Attachment` gains seven optional fields through the entity registry, gated
  by a new feature id, `attachment.thumbnail`, in `vault_requires` (ADR-0045).
  An older build round-trips them unchanged.
- `AttachFile` takes an optional thumbnail the client produced; the core seals
  it as a second blob and queues both uploads.
- The Apple pane replaces its hand-picked image/PDF branches with QuickLook for
  everything QuickLook supports.
- Settings → Storage shows cache usage, a size limit, **Clear cache** (which
  evicts everything evictable) and the cellular toggle.
- The relay sees one more blob per thumbnailed attachment, of at most 256 KiB.

## What would force revisiting this

1. **A platform that cannot encode JPEG or PNG natively**, which none today is.
2. **Users needing search inside attachments.** It would need decoders, and
   this record's no-bundled-decoder rule would be re-taken there, not here.
3. **Cache pressure on phones below 200 MB** measured in the field.
