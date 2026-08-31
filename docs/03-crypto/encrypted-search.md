---
status: accepted
---

# Encrypted Search

Search runs **entirely on-device** against the decrypted vault. The server never sees queries, query strings, results, or index contents. There is no Searchable Symmetric Encryption (SSE) component in v1.

## Decision

- **Index:** SQLite FTS5 inside the encrypted vault DB (which is itself encrypted by SQLCipher; see [`../04-storage/local-database.md`](../04-storage/local-database.md) for the chosen cipher and KDF). FTS5 inherits the vault's at-rest protection.
- **Sources indexed:** task title, task body, task notes, stream name and description, context names, person display name, attachment filename and user-set caption, free-form note bodies.
- **Tokenizer:** `unicode61 remove_diacritics 2` plus Porter stemming for English. Per-locale tokenizer is configured at vault creation; switching locales triggers a full re-index.
- **Refresh:** on every committed op affecting an indexed field, the FTS row is updated in the same SQL transaction that applies the op.

### Locale switch & re-index

Re-indexing on locale change is a **foreground task**:

- Search is disabled during the run; the search field shows `"Re-indexing search… <progress %>"`.
- Expected duration: < 5 s for ≤ 10 000 entities on a 2022-class device.
- Re-index runs the full source set through the new tokenizer in a single SQL transaction batched by 1 000 rows.

Cross-device divergence due to mismatched locales is acknowledged: when the local locale differs from the locale recorded in any sibling device's most recent vault-meta announcement, the user sees a banner: `"Search index uses <locale>; some other devices are configured differently and may show different results."`

## Why not server-side encrypted search

- Searchable Symmetric Encryption schemes leak access patterns (which document was opened) and are weaker than plain AEAD.
- The full vault fits on each device for the user shape we target (10k tasks ≈ tens of MB).
- Mobile devices have ample CPU and disk for FTS5.

## Cross-device consistency

Each device builds and maintains its own FTS5 index from its own decrypted op log. **Indexes are not synced.** Search results may differ briefly across devices while ops are propagating; this is acceptable.

## Search affordances

- **Free-text** — default tokenized FTS over indexed fields.
- **Facets** — `stream:`, `context:`, `priority:`, `state:`, `due:before:`, `created:after:`, etc., parsed at the UI layer into structured filters that compose with the FTS match.
- **Saved searches** — the saved query string is itself synced (in vault-meta); results are re-computed locally.

## Cold-start budgets

When a new device pairs in:

1. Pull encrypted op log from the relay.
2. Decrypt and apply ops to local state.
3. Build the FTS index incrementally as ops are applied (single SQL transaction per op batch).

For a 10 000-task vault on the platform performance baselines defined in [`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md):

- Mobile (Pixel 7 / iPhone 13 baseline): ≤ 30 s wall time end-to-end.
- Desktop (M1 / Ryzen 5 5600 baseline): ≤ 10 s.

## Web client constraints

The web client uses `wa-sqlite` (WASM SQLite) with FTS5 against OPFS for persistence. Private-browsing sessions fall back to in-memory storage; the FTS index is rebuilt each session. This is disclosed in onboarding.

## Attachments and search

- Filename and user-set caption are indexed.
- Document text extraction (PDF, OCR, etc.) is **not in v1**. If shipped later, extraction runs on-device and produces additional ops that flow through the normal indexing path; nothing about extraction touches the server.

## Privacy boundary

- Search history is not persisted by default. A "remember recent searches" preference defaults to off.
- A user export ("export my data") includes vault content; it does NOT include FTS index files. Search history if enabled is included only when explicitly chosen in the export dialog.

## Out of scope

Side-channel observation by an attacker with screen access (over-the-shoulder, screen recording) is not a defense target.
