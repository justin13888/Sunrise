# 0008 — Local FTS over server-side search

**Status:** accepted

## Context

Search is a core flow for multi-stream operators. The server can't see plaintext, but a usable productivity app must search across thousands of items in <100ms.

## Decision

**All search runs locally.** Each device maintains a SQLite FTS5 index of decrypted titles, bodies, and metadata, inside the encrypted vault DB.

## Alternatives considered

| Option | Why rejected |
|---|---|
| Server-side plaintext search | Breaks E2EE. Disqualified. |
| Server-side searchable encryption (SSE) | Leaks access patterns; weaker than plain AEAD; complex protocol; immature library support |
| Server-side blind index (one-way mapping per query term) | Useful for exact match, weak for full-text; significant complexity for marginal benefit |
| **Client-side FTS over local index** | Fast, simple, complete; perfect privacy; data fits on the device |

## Consequences

- Initial sync time scales with vault size (each device builds its own index from decrypted state). Benchmarks confirm acceptable times for our targets (<60s on web for 10k tasks).
- The web client must use `wa-sqlite` + OPFS; private-browsing mode falls back to memory and is documented.
- Saved searches sync as queries, not as result sets.
- Future feature: on-device semantic search (embeddings). The architecture supports adding a vector index next to FTS5 without disturbing anything else; the embedding model would run on-device with size/perf budgets to be defined later.

## Risks

- Cold-start time for a new device with a large vault. Mitigated by snapshots (compaction) and an incremental indexing strategy that prioritizes the most recent and most-used Streams.
- Mobile storage cost. Quantified in performance budgets; well within typical app footprints.
