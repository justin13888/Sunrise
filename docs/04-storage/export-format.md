---
status: accepted
---

# Export Format

A full export writes everything a vault holds, decrypted, as a directory of
plain files any tool can read: JSON lines, Markdown, iCalendar and the
attachments' own bytes, with a manifest that names the versions and vouches
for every file with a digest. It is the promise in
[`../00-product/vision.md`](../00-product/vision.md) §What success looks like:
"A user can export everything (decrypted) at any time, in a format that makes
sense outside Sunrise." Issue
[#350](https://github.com/justin13888/Sunrise/issues/350) built it.

It is the data, not the log: tombstones, the op log, sync state and keys are
not in it.

## Taking one

| Surface | Call |
|---|---|
| CLI | `sunrise export all <dir> [--no-fetch]` |
| UniFFI | `SunriseCore.export_all(dir, fetch)`, returning a `FullExportSummary` |

Both call `crates/sunrise-client-core/src/export.rs#export_all`, so they write
the same bytes. What goes in each file is decided in
`crates/sunrise-core/src/export.rs`, which touches no filesystem; the records
are read in one consistent read (`Core::export_records`).

The destination must not exist yet, or be an empty directory. Anything else is
refused before a byte is written. `manifest.json` is written last, so a
directory without one is an export that did not finish.

## Layout

```text
<dir>/
  manifest.json
  entities/<kind>.jsonl         one per stored entity kind, always present
  notes/<entity id>.md          one per non-empty note
  calendar.ics                  every Block
  attachments/<attachment id>/<filename>
```

### `entities/<kind>.jsonl`

One file per entity kind with a table of its own in the registry
(`crates/sunrise-id/src/registry.rs`), named by its registry tag: `task`,
`stream`, `context`, `routine`, `block`, `attachment`, `focus_session`,
`review_snapshot`, `preferences` and `saved_view` (#341). A file is written even when it holds no
row, so a reader can tell "none" from "not exported". A test fails when a kind
gains a table and no file
(`crates/sunrise-core/src/export/tests.rs#every_stored_kind_is_exported_and_nothing_else`).

Each line is one live record in its **document-schema JSON**: the same field
names and shapes the records have on the wire (`docs/02-domain/`, pinned by
`schemas/` at the manifest's `doc_schema_v`). So:

- Times keep their kind (`docs/10-cross-cutting/time.md`), as
  `{"kind":"floating","civil":"2026-01-02T09:00:00"}`,
  `{"kind":"zoned","civil":"…","tz":"Europe/Berlin"}`,
  `{"kind":"all_day","date":"2026-01-02"}` or `{"kind":"instant","at":"…Z"}`.
  The civil and instant strings are RFC 9557. A floating time is not resolved
  into the exporting device's zone; it stays floating.
- Fields a newer build wrote and this one does not model are kept, inline, as
  they arrived, exactly as the wire carries them.
- A note body (`body`, `description`) is its canonical CBOR, as a JSON array
  of byte values. Its readable form is `notes/<id>.md`.
- Each line decodes back into the record it came from (tested for every kind
  but `attachment`).

Lines are ordered by id. Every object's keys are sorted. Tombstoned rows are
left out. The Inbox has no line in `stream.jsonl`, because it is not a Stream
entity: a task whose `stream_id` is the manifest's `inbox_stream` is in the
Inbox.

An **attachment** line is its record without `blob_key` and
`thumbnail_blob_key`. These keys open the encrypted blob, so they stay in the
vault, and the export holds the plaintext bytes instead. Nothing else in any
record is key material.

Devices and the account identity are membership, not data, and have no file.
Notes and people have no table of their own: a note is a task's, stream's or
routine's body.

### `notes/<entity id>.md`

The Markdown projection of each non-empty task body, stream description and
routine-template body. It is the same projection `sunrise note` prints, and
it ends with a newline.

### `calendar.ics`

Every live Block, as RFC 5545, through the same mapping and writer that
`sunrise ical export` uses. Each `UID` is the Block's id, so importing the file
back lands on the same Blocks.

### `attachments/<attachment id>/<filename>`

The original plaintext bytes, verified against the attachment's content hash
on the way out. The id directory keeps two files with the same name apart. The
filename is the stored one, made safe as one path component
(`crates/sunrise-core/src/export.rs#safe_filename`):

- separators, control characters and `:*?"<>|` become `_`;
- leading dots, and trailing dots and spaces, are dropped;
- the result is cut to 200 bytes;
- an empty result is `file`.

So a stored name of two dots, a slash and `quote.txt` becomes `_quote.txt`. The stored name is still in
`attachment.jsonl`.

## Bytes that are not on this device

An attachment's metadata syncs everywhere, but its bytes are fetched lazily
([`blob-store.md`](./blob-store.md)). So before writing, the export asks for
every attachment that is not here and that some device uploaded. It does this
one at a time, through `Core::fetch_attachment`. This needs sync running,
which is how a client already downloads one attachment. `--no-fetch` (or
`fetch: false`) skips the fetch.

An attachment whose bytes are still not here is written to the manifest as
`missing`, with a reason, and has no file:

| `missing_reason` | Meaning |
|---|---|
| `not_fetched` | On the relay, but the export was asked not to fetch. |
| `never_uploaded` | No device uploaded the bytes, so nothing can fetch them. |
| `fetch_failed` | A fetch was asked for and did not bring them, for example because sync was not running or the relay was unreachable. |

A missing attachment never fails the export. The rest of the vault is still
worth having, and the manifest says exactly what is absent. The CLI prints one
note per missing attachment on stderr.

## `manifest.json`

```json
{
  "attachments": [
    {
      "filename": "../quote.txt",
      "id": "att_…",
      "mime_type": "text/plain",
      "missing_reason": null,
      "parent": "tsk_…",
      "path": "attachments/att_…/_quote.txt",
      "size_bytes": 6,
      "state": "present"
    }
  ],
  "doc_schema_fingerprint": "…64 hex…",
  "doc_schema_v": 14,
  "exported_at": "2026-01-01T00:00:00Z",
  "exported_at_ms": 1767225600000,
  "files": [
    { "blake3": "…64 hex…", "path": "calendar.ics", "size": 123 }
  ],
  "format": "sunrise-export",
  "format_v": 1,
  "inbox_stream": "str_…",
  "storage_v": 38,
  "zone": "America/New_York"
}
```

- `format` is always `sunrise-export`, and `format_v` is the integer version
  of this document. It is raised only when a file's meaning or layout changes
  in a way an existing reader would misread. A field added to a record is not
  such a change, because records follow `doc_schema_v`.
- `doc_schema_v` and `doc_schema_fingerprint` name the document schema the
  records are written in (ADR-0045 §2, #323). `storage_v` is the vault's
  storage version, for comparing exports of one vault over time.
- `exported_at` (RFC 9557) and `exported_at_ms` are the core's injected clock.
  `zone` is the reader's zone. It is informational: no file depends on it,
  because every time keeps its own kind.
- `files` lists every file but the manifest itself, by path, with its size and
  BLAKE3 digest in lowercase hex. `attachments` lists every live attachment by
  id, whether its bytes are present or not.
- Keys are sorted, the indent is two spaces, and the file ends with a newline.

## Determinism

The same vault exported at the same instant gives the same bytes, file for
file: rows are ordered by id, keys are sorted, files and attachments are listed
by path and id, and the only time in the output is the injected clock's
(`crates/sunrise-client-core/tests/export.rs#the_same_vault_exports_the_same_bytes`).

## Not yet

- **A `.zip` of the directory.** A reader can zip it. A zip writer is a
  dependency the directory does not need.
- **Import.** Reading an export back into a vault is a separate feature. The
  records are already in the shape an import would replay.
- **Progress and cancel across the seam.** The fetch goes one attachment at a
  time, and each one can already be cancelled with `cancel_attachment_fetch`.
  A progress stream for the whole export is the macOS "Export Everything…"
  sheet's to add.
