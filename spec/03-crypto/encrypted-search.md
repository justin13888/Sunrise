---
status: draft
---

# Encrypted Search

Search is a hard requirement (multi-stream operators search constantly). Plaintext search on plaintext data is straightforward. Plaintext search on E2EE data needs care.

## Decision: client-side full-text search

All search runs locally against the decrypted index. The server never sees queries or results.

- **Index:** SQLite FTS5 inside the encrypted vault DB. The vault DB is opened with SQLCipher; FTS5 lives inside it, fully encrypted at rest.
- **Sources:** task title/body/notes, stream name/description, context names, person display name, attachment filenames, note content.
- **Tokenizer:** unicode61 + porter stemming for English. Per-locale tokenization configured at vault creation.
- **Refresh:** on every committed op affecting an indexed field, the index is updated transactionally.

## Why not "encrypted search on the server" (SSE)?

- Searchable Symmetric Encryption schemes leak access patterns (which document was opened) and have weaker security than plain AEAD.
- The full-data fits comfortably on each device for our user's data shape (10k tasks ≈ tens of MB).
- Mobile devices have ample disk and CPU for FTS5.

## Cross-device consistency

Each device builds and maintains its own index from its own decrypted op log. Indexes are not synced.

## Search affordances

- **Free text.** Default tokenized FTS.
- **Facets.** `stream:`, `context:`, `priority:`, `state:`, `due:before:`, `created:after:`, etc., parsed at the UI layer into a structured query.
- **Saved searches.** Saved search queries are themselves CRDT-synced (just the query string, not the results).

## Cold-start

When a new device pairs in:

1. Pull encrypted op log.
2. Decrypt and apply ops to local state.
3. Build search index incrementally as ops are applied.

For a 10k-task vault this should complete in **≤30 seconds on phones**, **≤10 seconds on desktops**.

## Web client constraints

The web client uses `wa-sqlite` (WASM SQLite) with FTS5 on top of OPFS. OPFS provides per-origin persistence; private browsing falls back to memory (search works but is rebuilt each session — disclosed).

## Attachments and search

- Filename and user-set caption are indexed.
- Document text extraction (PDF, etc.) is **not** in v1. Tracked as v2; if added, extraction runs on-device and produces additional ops.

## Audit

Search in a privacy-sensitive context can leak via observation by an attacker with screen access. Out of scope; we are not designing against that.
