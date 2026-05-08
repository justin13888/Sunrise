---
status: draft
---

# Search (Feature)

The user-facing experience of search. Implementation is in [`../03-crypto/encrypted-search.md`](../03-crypto/encrypted-search.md).

## Free-text search

- `/` opens search from any view.
- Tokenized FTS5 query, scoped to the current view by default (Stream, Today, etc.).
- "Search everywhere" expands to all Streams.

## Structured search

Inline operators in the query:

| Operator | Example |
|---|---|
| `stream:` | `stream:work-acme deploy` |
| `context:` | `context:@errands milk` |
| `priority:` | `priority:1` |
| `state:` | `state:done last quarter retro` |
| `due:before:` / `due:after:` | `due:before:friday` |
| `created:after:` | `created:after:2024-01-01` |
| `assignee:` | `assignee:@carlos` |
| `is:routine` | only routine occurrences |
| `has:attachment` | tasks with attachments |
| `-term` | exclude term |

Mix-and-match. Free text without operators searches title + body + notes.

## Result presentation

- Grouped by entity kind: Tasks, Streams, Notes, Blocks, People.
- Inline match highlighting.
- Keyboard navigable (arrow keys to step; Enter to open).

## Saved searches

`Cmd-S` after running a search saves it (prompts for a name). Saved searches sync.

## Performance

- Sub-100ms p95 result render for 10k tasks.
- Local FTS5 query; no network.

## Search and privacy

- Search runs locally only; no query strings ever leave the device.
- Logs: search history is **not** persisted by default. Recent searches are kept in-memory for the session unless the user opts in to persistent history.

## TUI search

- `/` enters search mode.
- Results live-update as you type (incremental query).
- Same operators.

## Mobile search

- Pull-to-search on Today and Stream views (consistent with iOS/Android idioms).
- Voice query supported (input goes through the same parser).

## What we don't do

- Cloud search across users / shared streams from a single query (we can search shared streams *we hold locally* but not "everything in the universe").
- AI-powered fuzzy semantic search in v1 (tracked; requires a local embedding model that meets perf/privacy targets).
