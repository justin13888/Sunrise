# 0052 — Search indexes every entity kind twice (words and trigrams), parses an operator grammar in Rust, and stays inside the vault

**Status:** accepted

**Amends** [ADR-0008](./0008-search-strategy.md). Its decision — all search runs
locally, over an FTS5 index inside the encrypted vault database — stands. This
record adds what ADR-0008 left open: which kinds are indexed, how text that
FTS5's word tokenizer cannot split is found, where the query language is
parsed, how an index follows a language change, and what it may cost in memory.

**Specified in** [`../08-features/search.md`](../08-features/search.md).
**Budget in** [`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)
§Search. **Tracked by** [#345](https://github.com/justin13888/Sunrise/issues/345). Saved searches ride on the `SavedView` entity
of [#341](https://github.com/justin13888/Sunrise/issues/341).

## Context

What ships is free-text search over tasks, and nothing else.

- **One index, one kind written.** `search_idx`
  (`crates/sunrise-storage/migrations/0013_baseline.sql:475`) uses
  `porter unicode61 remove_diacritics 2`, fixed at vault creation, and only
  tasks are written to it
  (`crates/sunrise-core/src/engine/task.rs:729#ftsr_upsert_task`). Streams,
  blocks, notes, context names, attachment filenames and places cannot be found.
- **Search is a literal AND.** `query_search`
  (`crates/sunrise-core/src/engine/query.rs:280#query_search`) matches
  `kind = 'task'` and orders by `bm25`. `sanitize_fts_query`
  (`crates/sunrise-core/src/engine/query.rs:329#sanitize_fts_query`) quotes
  every whitespace token, which is what makes hostile input safe and also why
  `stream:work deploy` searches for the literal token `stream:work`.
- **No CJK and no substrings.** `unicode61` treats a run of CJK text as one
  token, so `東京` does not find `東京出張`, and nothing finds `pass` inside
  `passport`.
- **English stemming for everyone.** Porter is applied whatever language the
  user writes in, and there is no path to rebuild the index differently.
- **The operator grammar, grouping by kind and saved searches** were specified
  in `search.md` and never built.

Power users search constantly and with precision. They need one field that
finds anything by name, narrows by structure (`#work is:late`), and behaves the
same on every device, so a saved search means the same thing on a phone as on a
Mac.

## Decision

**1. Every user-visible entity kind is indexed.** Tasks, notes (the plain-text
projection of `NoteBody`), streams, blocks, contexts (by name), attachment
filenames, places, saved views (by name), external calendar events
([ADR-0049](./0049-calendar-integrations-per-device-oauth.md)) and people (by
display name). Every writer runs inside the transaction that applies the op,
and a tombstone removes its row.

**2. Two FTS5 indexes, both inside the SQLCipher vault database.**

- **Words:** `unicode61 remove_diacritics 2`, with `porter` in front **only
  when the vault's content language is English**. This is the ranked index.
- **Trigrams:** the `trigram` tokenizer, over text the core has normalized
  (NFKD, combining marks removed, case-folded) before writing, because the SQLite
  bundled with SQLCipher 4.5.3 (3.39.4) has no `remove_diacritics` option for
  `trigram`. It covers titles and names in full, and body and note text up to
  its first 4 KiB. It finds CJK text and substrings.

**3. The query language is parsed in Rust, by a pure total parser.**
`sunrise_domain::search::parse(&str) -> SearchQuery` never fails: an unknown
operator, a malformed value, or anything past the size limits becomes literal
text, with a `ParseNotice` the client may show. `SearchQuery` has a canonical
`Display`, and `parse(display(q)) == q`. Structured operators compile to SQL
predicates over entity tables; only free text reaches FTS5, still quoted term by
term, so no input can produce an FTS5 syntax error. The grammar is in the
feature spec.

**4. Results are grouped by kind, in a fixed order, with highlights computed in
Rust.** Each hit carries match ranges as Unicode-scalar offsets into the text it
returns, so no client re-tokenizes to highlight.

**5. Operators are ASCII English keywords on every locale.** `stream:`, `is:late`
and `has:attachment` mean the same thing on every device, so a saved search
syncs without translation. Clients localize the autocomplete descriptions, not
the keywords.

**6. The tokenizer follows the vault's content language, not the device's UI
language.** The content language is a synced preference key,
`search.content_language`, in the Preferences entity
([ADR-0050](./0050-preferences-and-day-schedule.md)), defaulting to the creating
device's language at vault creation; every device builds its
index the same way and a saved search finds the same things everywhere.
Changing it rebuilds both indexes in the background into shadow tables and swaps
them in one transaction; search answers from the old index until then.

**7. Saved searches are `SavedView` rows whose spec is a search.** The spec
stores the canonical query *text* and the grammar version, not the parsed tree,
so a device with an older grammar still parses it (unknown operators become
literal text) and a newer one reads it exactly.

**8. Honest limits, decided now.**

- **No typo tolerance.** `pasport` does not find `passport`. Trigram matching
  gives substring robustness (`pass`, `sport`) at bounded cost; edit-distance
  search at 100k rows in the budget needs another index and another few tens of
  megabytes on a phone.
- **No search inside attachment contents.** Blobs are fetched lazily, and
  indexing them would mean fetching everything and parsing PDFs and office files
  with decoders the app otherwise never runs. Filenames are indexed.
- **No semantic search.** ADR-0008's embedding note stays a note.
- **No stemming outside English.** `unicode61` alone for every other language.
- **Very short CJK queries scan.** A one- or two-character CJK term cannot use a
  trigram index; it runs a bounded `LIKE` scan over titles and names only.
- **Long bodies are only partly substring-searchable.** Past 4 KiB a body is
  found by whole words only.

**9. A memory budget with a bench.** Query p95 ≤ 100 ms over 10k tasks and
≤ 250 ms over 100k, and the index's resident memory counts against the per-
platform ceilings (a 100k-task vault must not push iOS past 200 MB), per
[`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)
§Search. The bench measures both sizes and records index bytes and peak RSS.

## Alternatives considered

| Option | Why not |
|---|---|
| **One index with an ICU tokenizer** | ICU word-breaking for CJK is the right linguistic answer, and it means shipping ICU (tens of megabytes) or a custom tokenizer compiled into SQLCipher on every platform. The trigram tokenizer is already in the bundled SQLite and needs no native code. |
| **Trigram only** | Finds everything, ranks badly (no word boundaries, no stemming), costs three to four times the text size on disk, and cannot match terms shorter than three characters. |
| **Tokenizer by device UI language** | Two devices would return different hits for one saved search. |
| **Parse operators in each client** | Every client would re-implement the grammar and drift, and the core would have to trust a client's SQL. |
| **Localized operator keywords** | A saved search written in German would not parse on an English device. |
| **Store saved searches as a parsed tree** | Ties the stored form to one grammar version; text plus a version is readable forever. |
| **A separate `SavedSearch` entity** | A saved search is a saved view whose filter is a query; two entities would need two sync paths, two menus and two parity rows for one idea. |
| **Fuzzy matching now** | Real cost on phones, and it lowers precision for the users who type exactly. Revisit when the budget allows it (below). |

## Consequences

- `Query::Search` changes shape: it takes the query text, `now` and the reader's
  zone (date operators resolve in it), and returns grouped, highlighted results
  instead of `QueryResult::Tasks`.
- Index writers appear in every engine module that applies an op for an indexed
  kind. A kind added later is indexed by registering a projection, not by
  editing the search code.
- **No `vault_requires` feature id.** Both indexes are device-local
  projections that never sync, and the parser adds no op kind or field, so
  nothing here gates a wire or data change under
  [ADR-0045](./0045-schema-identity-and-feature-gating.md) §7. The two synced
  pieces ride on features that already carry them: `search.content_language`
  is a key in the Preferences entity (`preferences.entity`), and saved searches
  are `SavedView` rows ([#341](https://github.com/justin13888/Sunrise/issues/341)).
- The index is rebuildable from entity rows at any time, which the
  migration-rigor work ([#327](https://github.com/justin13888/Sunrise/issues/327)) relies on: a failed `quick_check` on the index
  tables rebuilds them instead of failing the vault.
- The Apple clients' search view becomes grouped and highlighted, gains operator
  autocomplete and ⌘S to save, and prefills a scope operator from the current
  view.

## What would force revisiting this

1. **The trigram index blows the phone budget** on real 100k vaults. First
   lever: shorten the 4 KiB body window; second: trigram only titles and names.
2. **A typo-tolerant index that fits the budget**, or measured demand for one.
3. **An SQLCipher release carrying SQLite ≥ 3.45**, which adds
   `remove_diacritics` to `trigram`, and makes the core's own folding optional.
