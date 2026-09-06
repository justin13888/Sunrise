---
status: accepted
---

# Search (Feature)

The user-facing experience of search. Implementation is in [`../03-crypto/encrypted-search.md`](../03-crypto/encrypted-search.md).

> **Status: what ships is free-text search over Tasks, and nothing else on this
> page's original specification survived contact with the code.** The structured
> operator grammar, the negation syntax, the by-kind result grouping and the
> saved-search chord were specified here and never built. They are preserved
> below under [Specified, not built](#specified-not-built) rather than deleted,
> because the design is still the one to build; what is corrected is this page's
> claim to be describing the product. [#28](https://github.com/justin13888/Sunrise/issues/28)
> tracks the gap.

## What the index holds

One FTS5 virtual table, `search_idx`, created by
`crates/sunrise-storage/migrations/0013_baseline.sql`:

```sql
CREATE VIRTUAL TABLE search_idx USING fts5 (
    kind, id UNINDEXED, stream_id UNINDEXED, title, body, contexts,
    tokenize = 'porter unicode61 remove_diacritics 2'
);
```

The `kind` column exists so the table can hold more than one entity kind, and
**only `'task'` is ever written to it.** `ftsr_upsert_task` and
`ftsr_delete_task` in `crates/sunrise-core/src/engine.rs` are the only inserts
and deletes, they run inside the same transaction that applies the op, and a
deleted task is removed from the index rather than left as a tombstone. A
task's `contexts` column carries its context ids as text, so a context id is a
searchable token; a context *name* is not, because no row for a Context is ever
indexed. Streams, Blocks, Notes and People have no writer at all.

## Free-text search

`Query::Search { text, limit }` (`crates/sunrise-core/src/queries.rs`) is the
whole query surface. `query_search` (`engine.rs`) matches against `search_idx`
with `kind = 'task'`, orders by `bm25(search_idx)`, applies the caller's
`limit`, reads each hit back as a full Task and drops any that is deleted. It
returns `QueryResult::Tasks` — so search returns tasks, in relevance order,
with no other kind mixed in and no scoring exposed to the caller.

**Every query is a literal AND of quoted terms.** `sanitize_fts_query` splits
the input on whitespace, strips embedded `"` and control characters from each
token, wraps each token in double quotes and joins them with spaces. Quoting
each token makes it a literal FTS5 phrase, which is what defuses hostile input:
`AND`, `OR`, `NOT`, `NEAR`, `*`, `(`, `)` and `:` are all matched as characters
rather than parsed as syntax, so no query can produce a MATCH syntax error. It
is also why every operator in [Specified, not built](#specified-not-built) would
be searched for verbatim today — `stream:work deploy` looks for tasks
containing the literal token `stream:work`. Whitespace-only input returns an
empty result without touching FTS at all.

Stemming is Porter English, fixed at vault creation by the baseline schema.
There is no per-locale tokenizer selection and no re-index path;
[`../03-crypto/encrypted-search.md`](../03-crypto/encrypted-search.md)'s
locale-switch flow describes a target.

## The Apple clients

`SearchView` and `SearchModel` are shared: macOS reaches them as a window pane,
iOS as a `role: .search` tab, and both render the same field and the same rows.

On macOS the surface is reached three ways and is always the same one: `⌘F` from
the list, `/` in vim mode, and `⌘K`. `⌘F` carries the current query over — "keep
looking" — and `⌘K` clears the field first. On iOS both `searchInView` and
`searchGlobal` simply select the search tab. There is no view scoping on either:
every path lands on `Destination.search`, which issues `Query::Search` over the
whole vault, so "search in this list" and "search everywhere" are the same
search. The limit is `TaskListKind.searchLimit`, **200**.

`SearchModel` debounces at **150 ms** — long enough that "passport" is one query
rather than eight, short enough that the list feels attached to the keyboard —
and tracks the text the results on screen actually answer separately from the
text in the field, so an empty-state message never names a word the query has
not run for yet.

`SearchView` is a field, a one-line match count, and `TaskRows` — the same row
view Today and Inbox use. That is deliberate rather than incidental: a task
found by searching completes, defers, edits and deletes through exactly the same
path as one found anywhere else, so those five actions have one implementation
instead of two. The count says `"N matches"`, or `"First 200 matches — narrow it
further"` when the limit was reached, which is the only signal that results were
truncated. `↓` and `↩` move focus from the field into the rows.

## CLI

`sunrise search <query>...` runs the same `Query::Search` one-shot with a limit
of **100** and prints the matching tasks. An empty query is refused with
`"search needs a query"`. There is no interactive search mode: the terminal
client that had one was removed by
[ADR-0019](../11-adr/0019-swiftui-macos-client.md).

## Search and privacy

- Search runs locally only; no query strings ever leave the device. There is no
  route on the relay that takes one.
- Search history is not persisted anywhere. `SearchModel.text` lives for the
  life of the window and is not written to the vault.

## Performance

- p95 < 100 ms result render for ≤ 10 000 indexed Tasks on 2022-class laptops.
- Local FTS5 query; no network.
- `crates/sunrise-bench/benches/fts.rs` measures `Query::Search` over a
  10k-task vault and feeds `fts_query_10k_p99_ms` in `bench/baseline.json`. It
  runs nightly and **does not gate**: on shared runners the same binary reports
  large swings against its own baseline from noise alone.

## What we don't do

- Cloud search across users / shared streams from a single query (we can search shared streams *we hold locally* but not "everything in the universe").
- AI-powered fuzzy semantic search in v1 (tracked; requires a local embedding model that meets perf/privacy targets).

## States

Empty / loading / error states follow the three-state contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#three-state-view-contract).

## Specified, not built

> **None of this section exists.** It is the design, kept because it is still
> the design; it is not a description of any build.
> [#28](https://github.com/justin13888/Sunrise/issues/28) tracks it. Every
> operator below is currently searched for as a literal term, per
> [Free-text search](#free-text-search) above.

### Searchable entity surface

The target indexes Tasks, Streams, Blocks, Notes (NoteBody plain-text
projection), and People (display name only). Routines are not searched directly
— search by stream and tag instead. Today only Tasks are indexed, and the
`kind` column that would carry the rest is the seam to build on.

### Structured search

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

Building this means a parser *in front of* `sanitize_fts_query` rather than a
change to it: the operators have to be lifted out and turned into SQL predicates
before the remaining free text is quoted, because the quoting is what makes the
query safe and it cannot be relaxed selectively.

### Result presentation

- Grouped by entity kind: Tasks, Streams, Notes, Blocks, People.
- Inline match highlighting.
- Keyboard navigable (arrow keys to step; Enter to open).

The third of these ships; the first two do not, and the first cannot until
more than one kind is indexed.

### Scoped search

Search scoped to the current view by default (Stream, Today, etc.), with
"search everywhere" as an explicit widening. `Query::Search` takes no scope, and
the client offers no way to express one.

### Saved searches

`Cmd-S` after running a search saves it (prompts for a name). Saved searches
store the **query spec**, not results. Each open of a saved search re-runs the
FTS query. Sync: the query spec syncs as an ordinary entity, merged by
entity-level LWW.

There is no such action: `AppAction`
(`apps/apple/Sunrise/Keyboard/Keymap.swift`) has no case for it, so there is no
chord, no palette entry and no menu item. **Saved *views* are a different
feature that does ship** — `SavedViewsModel` and the Saved Views menu — and they
save a view's filters, not a search query.

### Mobile search

- Pull-to-search on Today and Stream views (consistent with iOS/Android idioms).
  iOS reaches search through a tab instead; there is no pull gesture.
- Voice query supported (input goes through the same parser).
