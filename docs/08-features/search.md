---
status: accepted
---

# Search (Feature)

One field that finds anything in the vault by name or text, narrows by
structure, and behaves the same on every device.
[ADR-0052](../11-adr/0052-search-v2.md) is the decision (it amends
[ADR-0008](../11-adr/0008-search-strategy.md)); this page is the contract. The
cryptographic side — the index lives inside the SQLCipher vault and never
leaves the device — is in
[`../03-crypto/encrypted-search.md`](../03-crypto/encrypted-search.md).
[#345](https://github.com/justin13888/Sunrise/issues/345) builds it.

> **Status: what ships is free-text search over tasks only.** One FTS5 table,
> `search_idx` (`crates/sunrise-storage/migrations/0013_baseline.sql:475`),
> tokenized `porter unicode61 remove_diacritics 2`, written only for tasks
> (`crates/sunrise-core/src/engine/task.rs:729#ftsr_upsert_task`).
> `query_search` (`crates/sunrise-core/src/engine/query.rs:280#query_search`)
> returns tasks by `bm25`, and `sanitize_fts_query`
> (`crates/sunrise-core/src/engine/query.rs:329#sanitize_fts_query`) quotes
> every whitespace token, so every operator on this page is searched for as
> literal text today. §[What ships today](#what-ships-today) describes the
> build; everything else on this page is the contract [#345](https://github.com/justin13888/Sunrise/issues/345) delivers.

## What is indexed

| Kind | Indexed text | Opens |
|---|---|---|
| Task | title, body (plain text), context names | the task |
| Note | plain-text projection of the `NoteBody` | the note's parent |
| Stream | name, description | the stream |
| Block | title, notes, location | the block in the calendar |
| Context | name | the context's list |
| Attachment | filename | the parent, with the attachment selected |
| Place | name | the place's task list |
| Saved view | name | the view |
| Calendar event | title, location ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)) | the event in the calendar |
| Person | display name | the person |

Routines are found through the tasks they generate and by `is:recurring`.
Attachment *contents* are never indexed.

## Two indexes

| Index | Tokenizer | Covers | Used for |
|---|---|---|---|
| Words (`search_words`) | `unicode61 remove_diacritics 2`, with `porter` only when the vault's content language is English | every indexed field in full | ranking, word and prefix matches, stemming |
| Trigrams (`search_tri`) | `trigram`, over text the core normalizes first (NFKD, combining marks removed, case-folded) | titles and names in full; bodies and notes up to their first 4 KiB | CJK, substrings |

Both live in the vault database, are written in the transaction that applies
the op, and drop a row on its tombstone. A free-text term matches a row if
either index matches it. Word-index hits rank above trigram-only hits, and
within each by `bm25` with column weights: title and name 10, location and
filename 5, body 1.

A term of one or two characters that contains CJK cannot use the trigram
index. It runs a bounded `LIKE` scan over titles and names only, capped at the
first 500 matches.

## Query language

Parsed in Rust by `sunrise_domain::search::parse`, which is **total**: every
string parses. What it cannot read as an operator it reads as text, and it
reports why with a `ParseNotice` (shown as a subtle underline in the field).

### Grammar

```ebnf
query     = [ ws ] [ and_expr ] [ ws ] ;
and_expr  = or_expr { ws or_expr } ;                 (* juxtaposition is AND *)
or_expr   = unary { ws "OR" ws unary } ;             (* OR binds tighter than AND *)
unary     = [ "-" ] atom ;                           (* "-" negates *)
atom      = "(" [ ws ] and_expr [ ws ] ")"
          | filter | shorthand | phrase | term ;
filter    = key cmp value ;
key       = "stream" | "context" | "state" | "kind" | "is" | "has"
          | "priority" | "planned" | "target" | "hard" | "due"
          | "created" | "done" | "place" ;
cmp       = ":" | "<" | "<=" | ">" | ">=" ;
value     = quoted | bare | date ".." date | bare { "," bare } ;
shorthand = "#" name | "@" name | "!" digit ;         (* = stream: / context: / priority: *)
phrase    = quoted ;
quoted    = '"' { any char except '"' } '"' ;
term      = bare ;
bare      = 1*( any char except ws, '"', "(", ")" ) ;
```

- `OR` is an operator only in capitals. `a b OR c` means `a AND (b OR c)`.
- Limits: 1024 characters, 32 clauses, nesting depth 8. Past a limit, the rest
  of the input is read as text and a notice says so.
- An unknown key (`colour:red`) is text, not an error.
- While typing, the last bare term also matches as a prefix (`pass` finds
  `passport` by word). Saved searches and the CLI do not add the prefix.

### Operators

| Operator | Matches | Example |
|---|---|---|
| `stream:` / `#` | tasks and blocks in that stream **or any stream nested under it** | `#work deploy`, `stream:"Side project"` |
| `context:` / `@` | tasks carrying that context | `@errands milk` |
| `state:` | task state: `todo`, `in_progress`, `done`, `cancelled`, or `open` (= `todo` or `in_progress`) | `state:done retro` |
| `kind:` | restrict to kinds: `task`, `note`, `stream`, `block`, `context`, `attachment`, `place`, `view`, `event`, `person` | `kind:note,task budget` |
| `is:` | `late` (soft or hard), `late-hard`, `stale`, `blocked`, `recurring`, `unplanned` (no `planned_at`), `inbox` (no stream and untriaged), `fixed`, `flexible` (blocks) | `is:late #work` |
| `has:` | `attachment`, `note`, `estimate`, `deadline`, `place`, `dependency` | `has:attachment invoice` |
| `priority:` / `!` | priority 1–5, the capture syntax's scale; comparators allowed | `!1`, `priority<=2` |
| `planned` | `planned_at` | `planned:today`, `planned<tomorrow` |
| `target` | `target_at` (soft deadline) | `target:this-week` |
| `hard` | `hard_due_at` (hard deadline) | `hard<+3d` |
| `due` | the earlier of `target_at` and `hard_due_at` | `due<friday` |
| `created`, `done` | `created_at`, `completed_at` | `done:last-week` |
| `place:` | tasks requiring that Place ([ADR-0051](../11-adr/0051-places.md)) | `place:office` |
| `-` | negation of any atom | `-@waiting`, `-"draft"` |
| `"…"` | exact phrase | `"quarterly report"` |
| `OR` | either side | `@home OR @errands` |

Deadline and state names come from
[ADR-0047](../11-adr/0047-deadlines-and-lateness.md); `is:late` and `is:stale`
read the same `lateness` function the triage queue reads, so search and triage
never disagree.

**Names** resolve case-insensitively: an exact name wins, then a unique prefix.
A name that resolves to nothing matches nothing and raises a notice ("No stream
named ‘wrok’"); it is not reinterpreted as text. Several values separated by
commas are OR'd (`context:home,errands`).

**Dates** resolve in the reader's zone at query time, with the week starting on
the `week_start` preference ([ADR-0050](../11-adr/0050-preferences-and-day-schedule.md)):

| Value | Means |
|---|---|
| `today`, `tomorrow`, `yesterday` | that civil day |
| `mon` … `sun`, or `monday` … `sunday` | the next such day, today included |
| `this-week`, `next-week`, `last-week`, `this-month`, `next-month`, `last-month` | that period |
| `+3d`, `-2w`, `+1m` | today plus or minus days, weeks or months |
| `2026-10-01`, `2026-10` | that day or month |
| `a..b` | from the start of `a` to the end of `b` |

With `:` a date value means "within that day or period"; `<` means before its
start, `>` after its end, and `<=`/`>=` include it. A task whose deadline is
`Floating` or `AllDay` is compared as a civil date in the reader's zone
([`../10-cross-cutting/time.md`](../10-cross-cutting/time.md)), never through
the UTC-as-civil index.

Keywords are ASCII English on every locale. The field autocompletes keys, values
and names, and each suggestion carries a localized description.

## Results

```rust
pub struct SearchResults {
    pub groups: Vec<SearchGroup>,     // fixed kind order; empty groups omitted
    pub notices: Vec<ParseNotice>,
    pub canonical: String,            // Display of the parsed query
}

pub struct SearchGroup {
    pub kind: SearchKind,
    pub hits: Vec<SearchHit>,         // up to the per-group limit, by rank
    pub total: u32,                   // before the limit
}

pub struct SearchHit {
    pub entity: EntityRef,
    pub opens: EntityRef,             // what activating the hit opens
    pub title: Highlighted,
    pub snippet: Option<Highlighted>, // ≤ 160 scalars around the first body match
}

pub struct Highlighted {
    pub text: String,
    pub ranges: Vec<(u32, u32)>,      // Unicode-scalar offsets, half-open
}
```

- **Group order is fixed**: Tasks, Notes, Calendar events, Blocks, Streams,
  Contexts, Places, Attachments, Saved views, People. A fixed order builds
  muscle memory; ranking reorders hits within a group, never the groups.
- **Per-group limit**: 50 tasks and 10 of every other kind, each with "Show all
  N" which re-runs the query with `kind:` set.
- **Highlights** are computed in Rust. Clients draw `ranges`; they never
  re-tokenize.

## Scope

Search is always over the whole vault, and scope is just an operator. Invoked
from a view, the field is prefilled with that view's scope as an editable token
(from a stream, `#work `; from a context, `@errands `), which one Backspace
removes. `⌘K` opens the field empty; `⌘F` keeps the current query.

## Saved searches

`⌘S` in the search view saves the query as a `SavedView` ([#341](https://github.com/justin13888/Sunrise/issues/341)) whose spec
is:

```rust
ViewSpec::Search { query: String /* canonical text */, grammar_v: u16 }
```

The result is recomputed on each device. Because the tokenizer follows the
vault's content language and not the device's, a saved search returns the same
hits on every device that holds the same data. A device with an older grammar
parses what it knows and reads the rest as text.

## Content language and re-indexing

The tokenizer is chosen by the synced preference `search.content_language`
([ADR-0050](../11-adr/0050-preferences-and-day-schedule.md)). Both indexes are
rebuilt, in the background, when:

- the content language changes;
- the index schema version stored in the vault differs from the build's;
- an integrity check on the index tables fails.

A rebuild streams entity rows in batches of 500, each its own transaction, into
shadow tables, then swaps them in one transaction. Search keeps answering from
the old index until the swap, and Settings shows progress. A rebuild is
resumable after a crash.

## Performance and memory

Budgets live in
[`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)
§Search: p95 ≤ 100 ms over 10k tasks and ≤ 250 ms over 100k, and the index's
resident memory counts against each platform's ceiling (iOS 200 MB). The
Criterion bench measures both vault sizes with notes, blocks and events in
proportion, records index bytes and peak RSS, and joins the baseline set.

Phone constraints this design respects:

- The rebuild's working set is bounded by its batch, not by vault size.
- App extensions (share sheet, widgets), whose memory limits are far below the
  app's, never open the search index.
- SQLite's page cache for the vault connection is not raised for search; FTS5
  reads through the same cache as everything else.

## Honest limits

- **No typo tolerance.** `pasport` does not find `passport`; `pass` and `sport`
  do.
- **No search inside attachments.** Filenames only.
- **No semantic search.**
- **No stemming outside English.**
- **Past 4 KiB, a body is found by whole words only**, not by substring.
- **One- and two-character CJK terms** search titles and names, not bodies.
- **Nothing across users.** A shared stream is searchable only in the copy this
  device holds.

## Privacy

- Search runs locally. No query string leaves the device, and no relay route
  takes one.
- Search history is not persisted. A saved search is persisted because the user
  saved it, and it syncs encrypted like any other entity.

## Clients

Every client of a device class offers the same search surface.

- **Desktop:** the grouped result list with highlights; ↑/↓ step through hits
  across groups, ⌘↓/⌘↑ jump between groups, Return opens, ⌘Return opens in a
  new window where the platform has windows. Operator autocomplete. ⌘S saves.
  Every action shows its shortcut hint ([`keyboard.md`](./keyboard.md)).
- **Phone and tablet:** the same groups, with "Show all" per group; the
  keyboard's search key submits; a hardware keyboard gets the desktop bindings.
- **CLI:** `sunrise search <query>` prints groups in the same order, one line
  per hit, with `--kind` and `--json`.

Empty, loading and error states follow the three-state contract in
[`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#three-state-view-contract).
The empty state names the operators most likely to help ("Try `#stream` or
`is:late`").

## What ships today

- **Index:** tasks only, in `search_idx`, with context *ids* (not names) in the
  `contexts` column.
- **Query:** `Query::Search { text, limit }` returns `QueryResult::Tasks` in
  relevance order. Whitespace-only input returns nothing without touching FTS.
- **Apple clients:** `SearchView` and `SearchModel` are shared by macOS (a pane,
  reached by ⌘F, ⌘K and `/` in vim mode) and iOS (a search tab). The model
  debounces at 150 ms and limits results to 200, and the rows are the same
  `TaskRows` Today and Inbox use, so every row action has one implementation.
- **CLI:** `sunrise search <query>...`, limit 100, tasks only.
- **Bench:** `crates/sunrise-bench/benches/fts.rs` measures 10k tasks and feeds
  `fts_query_10k_p99_ms`; it runs nightly and does not gate.
