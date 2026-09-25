---
status: accepted
---

# Stream

A long-lived bucket of related work. Streams are the **primary axis** of multi-stream operation.

> **Amended** by [ADR-0046](../11-adr/0046-optional-stream.md): a Task, Block
> or routine template may have **no** stream, the Inbox is a view rather than
> a stream, and deleting a stream re-homes everything it held. Also amended by
> [ADR-0044](../11-adr/0044-per-field-ops.md) (per-field merge) and
> [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) (`paused_until`
> is an `stime`).

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

```cddl
Stream = {
    id:           tstr .regexp "str_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:   timestamp,
    updated_at:   timestamp,
    name:         text<128>,
    description?: NoteBody,
    color:        StreamColor,             ; required; from the fixed palette below
    icon?:        tstr,                    ; free-form icon id; omitted when unset
    parent_id?:   entity-ref,              ; str_ ref; one-level nesting allowed
    sort_order:   tstr,                    ; fractional-index string
    archived:     bool,
    paused:       bool,                    ; routines pause; due-date warnings suppressed
    paused_until?: stime,                  ; pause ends at this time; see time.md
    review_cadence: ReviewCadence,         ; required, not optional
    default_context?: entity-ref,          ; ctx_ ref; assigned to new tasks in this stream
    reminder_lead_s?: uint,                ; seconds; Stream default for its Tasks
    deleted:      bool,
    rehomed_to?:  entity-ref,              ; str_ ref; written only by DeleteStream (MoveTo)
    unknown-fields,                        ; see overview.md
}

StreamColor  = "slate" / "rose" / "amber" / "emerald"
             / "sky" / "indigo" / "violet" / "pink" / tstr   ; unknown values preserved
ReviewCadence = "weekly" / "biweekly" / "monthly" / "none" / tstr
```

- `icon` is a plain string, not a closed `StreamIcon` enum. There is no fixed
  icon set on the wire; which ids a client can render is a client concern.
- `reminder_lead_s` is the middle rung of the notification lead-time hierarchy:
  a Task's own `reminder_lead_s` wins, this is the Stream default, and the
  device's global default is the floor. See
  [`../08-features/notifications.md`](../08-features/notifications.md).
- **There is no `integrations` map**, and no `IntegrationKey` /
  `IntegrationConfig` type. Earlier revisions declared one; nothing has ever
  serialized it, and the field was deleted rather than deferred. Integration
  credentials are **account-scoped**, not Stream-scoped: they belong to
  [ADR-0025](../11-adr/0025-integration-account-entity.md)'s
  `IntegrationAccount` entity, because one Google authorization backs many
  Streams and rotating one Stream's key must not orphan an unrelated calendar.
  See [`../09-integrations/overview.md`](../09-integrations/overview.md).

### `description`, `default_context` and `icon` — the write and persist contract

These three fields are the ones whose plumbing is shortest, so the contract is
stated explicitly rather than left to be inferred from the CDDL:

- **`description` and `default_context` are persisted.** They are columns on
  the `streams` table and `read_stream` in `crates/sunrise-core/src/engine/stream.rs`
  reads them back. Anything less was data loss under full-state ops, where a
  device that re-emits a Stream without a field it did not materialize erases
  that field everywhere; per-field ops ([ADR-0044](../11-adr/0044-per-field-ops.md))
  remove that failure mode, and persisting every field is still required.
- **All three are writable.** `StreamDraft` sets them at create and
  `StreamPatch` changes them at update, which is what makes `icon` — a field
  that already round-trips through storage and the wire — reachable from a
  command rather than only from a test writing the row directly.

### No stream, and the private key domain

There is **no built-in pseudo-stream**. A Task, Block or routine template with
no `stream_id` belongs to no stream, and its ops are sealed under the vault's
**private key domain** ([ADR-0046](../11-adr/0046-optional-stream.md) §2):

- Its 16-byte id is `00 00 00` followed by ASCII `sunrise.inbox`
  (`crates/sunrise-domain/src/inbox.rs`), the bytes the
  former Inbox pseudo-stream used, kept so that every existing op, epoch and
  relay route carries over unchanged. The ASCII is historical. The bytes are
  unreachable by ULID generation, whose first six bytes are a timestamp, and
  they are deliberately **not** the vault-meta stream's sixteen zero bytes
  ([ADR-0024](../11-adr/0024-key-hierarchy.md)).
- Its key lifecycle is a Stream key's: random per epoch, wrapped, distributed by
  `key_envelope`, rotated on every revocation.
- It is **not a Stream**: it has no row, no name, no colour, no sort position;
  it never appears in `Query::StreamList`; no stream command can name it; and it
  is never shared.
- The **Inbox** is a view over stream-less, untriaged tasks
  ([`tasks.md` §Inbox](./tasks.md#inbox)), not a stream.

Until [#332](https://github.com/justin13888/Sunrise/issues/332) lands, the tree still models this as a fixed-id Stream,
`str_0000076XBEE9MQ6S9ED5Q64VVR`, that commands refuse to delete.

## Why one-level nesting only

We considered:

- **Flat list of streams** (no nesting). Forces every Stream to be top-level — fine for "Work A / Work B / Family / Travel" but bad for users with sub-projects under a job.
- **Arbitrary tree.** Causes UI complexity, weak filtering, deep-nesting drift toward a project-management tool we don't want to be.

**Decision: one-level nesting** (parent → child). Rationale: a job has projects; a project has tasks. No project-of-project-of-project.

## Stream behavior

- **Pausing.** A paused Stream's:
  - Routines do not generate occurrences.
  - Tasks do not enter the triage queue, and their lateness is not surfaced in
    default views ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)).
  - Tasks remain editable.

  A stream is paused while `paused` is true and, when `paused_until` is set,
  `now` is before it (an `all_day` value ends at the start of that planner
  day). The pause ends by that comparison at read time; nothing writes
  `paused = false` automatically.
- **Archived.** A stream and its tasks remain queryable but are hidden from default views. Routines stop. Sharing with others is paused.
- **Color and icon.** Used as visual primitives across all platforms — color for legibility (also calendar block tint), icon for glanceable identity. Stream-less tasks and blocks use the neutral palette colour.

## Sort order

`sort_order` is a fractional-index string. New streams default between the last
and "end." Reorders are constant work; no list-shifting.

The encoding is `crates/sunrise-domain/src/sort_order.rs`: a base-26 fraction
over the digits `A`..`Z`, chosen so that **lexicographic order on the strings is
numeric order on the fractions**. `Query::StreamList` therefore sorts with
`ORDER BY sort_order` and decodes nothing, and a client compares two keys with
a plain string comparison.

- **A key never ends in `A`.** `A` is the digit zero, so `AB` and `ABA` are one
  number spelled two ways, and two rows holding two spellings would be
  un-orderable by the string comparison everything above rests on. The rule
  makes each number's spelling unique. Keys arriving from a peer are read
  leniently; keys this build *writes* are validated.
- **No jitter.** Fractional-index libraries usually append random digits so two
  devices inserting at one position get different keys. This one is
  deterministic, and two rows that end up with the same key tie-break by name
  then id, identically on every replica — see §Merge mapping.
- **The empty string is not a key.** It is the column default for a Stream that
  has never been ordered, meaning "no position" rather than "first position".
  It sorts first, and `''` rows tiebreak by name.

Writing it:

- `StreamPatch.sort_order` / `StreamEdit.sortOrder` carry a new position. A
  client computes the key from the two rows the dragged one landed between —
  `sort_order::between(after, before)`, exported across the FFI seam as
  `stream_sort_key_between` so no client reimplements the arithmetic — and
  sends it. **One row is rewritten; no sibling is touched.**
- A key outside `A`..`Z` is rejected at validation rather than repaired: it has
  no defined place in the list, and one such write is permanent, because
  nothing over the alphabet sorts after it.
- `sunrise streams move <id|name> before <id|name>` / `… last` is the CLI verb.
- On macOS it is a drag: the sidebar's stream section is a `ForEach` with
  `.onMove`, which computes the key from the same seam helper and submits
  `UpdateStream`. The Inbox sits outside that `ForEach` because it is a view,
  not a stream, and has no position in the order.
- The column is `streams.sort_order`, added by
  `crates/sunrise-storage/migrations/0014_stream_sort_order.sql` at
  `STORAGE_V = 14`, since superseded by 0015 and 0016. `DOC_SCHEMA_V` did **not** move for it: `sort_order` was
  already a required `tstr` on the wire, carrying `"a0"`, so a real value went
  into a field that already existed rather than a field being added.

The string is bounded by a defrag rule:

- A device that observes a `sort_order` string ≥ 64 bytes triggers a defrag op for the affected list.
- Defrag is a single op `stream.list.defrag` carrying the new index for every entry. It is idempotent — concurrent defrag from two devices produces the same output (lex-sort the entries by `(ts_ms, device_id_lex)` and re-assign indexes evenly across `[A, Z]`).
- The trigger is per-device throttled to once per list per hour to prevent thrash.

> **Defrag is not implemented.** The op-kind registry in
> `crates/sunrise-core/src/inner_op.rs` has `stream.create` / `stream.update` /
> `stream.delete` and no `stream.list.defrag`, so nothing observes the 64-byte
> bound and nothing defrags. The index really does grow unbounded under
> pathological reordering — repeatedly dropping a row into the same gap adds
> roughly a digit per 26 insertions, without limit — and
> `sort_order::DEFRAG_THRESHOLD_BYTES` records the bound that nothing reads
> yet. Adding the op is a `DOC_SCHEMA_V` bump, not a breaking change.
>
> The *write path* is implemented, and that is a change from an earlier
> revision of this section, which claimed `sort_order` was "written and read"
> while every construction site hardcoded `"a0"`.

## Sharing

A Stream is the **unit of sharing** with another identity. Sharing a Stream:
- Grants the recipient access to the Stream entity, all its Tasks, attached Notes, and Blocks bound to those Tasks.
- Does **not** grant access to other Streams' content, even if a Task in this Stream is `blocked_by` a Task in another (the linked task surfaces only by ID + redacted title).

See [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md).

## Merge mapping

Every Stream field is a per-field LWW register on `(hlc, device_id, seq)`
([ADR-0044](../11-adr/0044-per-field-ops.md),
[ADR-0016](../11-adr/0016-hlc-timestamps.md)). Renaming a stream on one device
and recolouring it on another both survive. The tree still merges the whole
Stream as one full-state unit until [#319](https://github.com/justin13888/Sunrise/issues/319) lands.

`sort_order` is worth calling out: it is one register per stream, so two
devices moving the *same* stream concurrently produce one survivor, and two
devices moving *different* streams both keep their move. The fractional index
keeps a single reorder from rewriting every sibling.

Under today's full-state merge the test
`concurrent_stream_reorders_converge_on_one_arrangement_not_a_merge`
in `crates/sunrise-core/src/engine/tests.rs` has two devices drag the same stream at
the same instant and asserts that both land on one of the two keys and never on
a third. Two rows can also end up *sharing* a key that way — a device can only
lose a reorder wholesale, so it can lose it onto a key a sibling already holds —
and `Query::StreamList` breaks that tie by name then id, identically on every
replica.

## Validation

- `name` MUST be non-empty.
- `parent_id` MUST refer to a non-archived, non-deleted, top-level Stream **at the time the `parent_id` is set or changed**. A `parent_id` that later names a tombstone reads as top-level (see §Deleting a stream).
- `DeleteStream` with `MoveTo` MUST name a live stream other than the one being deleted.
- Cycles are impossible by structure (one-level limit + no parent self-reference).

## Deleting a stream

`Command::DeleteStream { id, disposition }` removes the stream and **nothing
else** ([ADR-0046](../11-adr/0046-optional-stream.md) §4). In one transaction:

1. Every live Task, Block and routine template that names the stream is
   re-homed: to no stream (`Detach`) or to another live stream (`MoveTo`). Each
   is a normal per-field write sealed under its new key domain.
2. Every child Stream gets `parent_id = null` and becomes top-level.
3. The Stream is tombstoned, with `rehomed_to` set for `MoveTo`.

Nothing is deleted with the stream: not its tasks, not its blocks, not its
routines.

**Late arrivals.** A task created in the stream on another device after the
delete resolves at read time through the tombstone:

```
effective_stream(ref) = None                                   if ref is None
                      = ref                                    if the stream is live
                      = effective_stream(tombstone.rehomed_to) if it is a tombstone
                      = None                                   on a chain longer than 8, or a cycle
```

Every view, routine generation and "which domain seals the next write" reads
`effective_stream`, and the next write to such a task persists the resolved
value. So no reader ever sees a live entity in a deleted stream.

A child whose parent is deleted concurrently with a re-parent on another device
also resolves at read time: a `parent_id` naming a tombstone reads as top-level.

Until [#332](https://github.com/justin13888/Sunrise/issues/332) lands, `delete_stream` tombstones the stream row only
(`crates/sunrise-core/src/engine/stream.rs#delete_stream`), and its dependents
keep naming it.
