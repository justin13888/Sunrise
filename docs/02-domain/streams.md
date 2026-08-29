---
status: accepted
---

# Stream

A long-lived bucket of related work. Streams are the **primary axis** of multi-stream operation.

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
    paused_until?: timestamp,              ; auto-unpause time
    review_cadence: ReviewCadence,         ; required, not optional
    default_context?: entity-ref,          ; ctx_ ref; assigned to new tasks in this stream
    reminder_lead_s?: uint,                ; seconds; Stream default for its Tasks
    deleted:      bool,
    unknown-fields,                        ; see overview.md
}

StreamColor  = "slate" / "rose" / "amber" / "emerald"
             / "sky" / "indigo" / "violet" / "pink"
ReviewCadence = "weekly" / "biweekly" / "monthly" / "none"
```

- `icon` is a plain string, not a closed `StreamIcon` enum. There is no fixed
  icon set on the wire; which ids a client can render is a client concern.
- `reminder_lead_s` is the middle rung of the notification lead-time hierarchy:
  a Task's own `reminder_lead_s` wins, this is the Stream default, and the
  device's global default is the floor. See
  [`../08-features/notifications.md`](../08-features/notifications.md).
- **There is no `integrations` map**, and no `IntegrationKey` /
  `IntegrationConfig` type. Earlier revisions declared one; nothing has ever
  serialized it. Calendar integration is not wired to a Stream field in v1 —
  see [`../09-integrations/overview.md`](../09-integrations/overview.md).

Built-in pseudo-stream:

- `str_INBOX0000000000000000000000` — fixed ID; not user-creatable, not deletable, not editable. New unassigned tasks live here.

## Why one-level nesting only

We considered:

- **Flat list of streams** (no nesting). Forces every Stream to be top-level — fine for "Work A / Work B / Family / Travel" but bad for users with sub-projects under a job.
- **Arbitrary tree.** Causes UI complexity, weak filtering, deep-nesting drift toward a project-management tool we don't want to be.

**Decision: one-level nesting** (parent → child). Rationale: a job has projects; a project has tasks. No project-of-project-of-project.

## Stream behavior

- **Pausing.** A paused Stream's:
  - Routines do not generate occurrences.
  - Due-date overdue states do not surface in default views.
  - Tasks remain editable.
- **Archived.** A stream and its tasks remain queryable but are hidden from default views. Routines stop. Sharing with others is paused.
- **Color and icon.** Used as visual primitives across all platforms — color for legibility (also calendar block tint), icon for glanceable identity.

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
  deterministic, because under [ADR-0014](../11-adr/0014-entity-level-lww-merge.md)
  two concurrent reorders do not both survive whatever their keys are — see
  §Merge mapping.
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

The string is bounded by a defrag rule:

- A device that observes a `sort_order` string ≥ 64 bytes triggers a defrag op for the affected list.
- Defrag is a single op `stream.list.defrag` carrying the new index for every entry. It is idempotent — concurrent defrag from two devices produces the same output (lex-sort the entries by `(ts_ms, device_id_lex)` and re-assign indexes evenly across `[A, Z]`).
- The trigger is per-device throttled to once per list per hour to prevent thrash.

> **Defrag is not implemented in v1.** The op-kind registry in
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

The whole Stream is one last-writer-wins unit on `(hlc, device_id, seq)`
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md),
[ADR-0016](../11-adr/0016-hlc-timestamps.md)). Per-field LWW registers are the
target state, not the shipped one.

`sort_order` is worth calling out: under entity LWW, two devices reordering
the same list concurrently produce one survivor rather than an interleave. The
fractional index still does its job — it keeps a *single* reorder from
rewriting every sibling — but it does not make concurrent reorders merge.

This is behaviour, not a caveat: `concurrent_stream_reorders_converge_on_one_arrangement_not_a_merge`
in `crates/sunrise-core/src/engine.rs` has two devices drag the same stream at
the same instant and asserts that both land on one of the two keys and never on
a third. Two rows can also end up *sharing* a key that way — a device can only
lose a reorder wholesale, so it can lose it onto a key a sibling already holds —
and `Query::StreamList` breaks that tie by name then id, identically on every
replica.

## Validation

- `name` MUST be non-empty.
- `parent_id` MUST refer to a non-archived, non-deleted Stream **at the time the `parent_id` is set or changed**. The validation is enforced on `parent_id` mutations only, not retroactively (see "Child-stream lifecycle on parent delete" below).
- Cycles are impossible by structure (one-level limit + no parent self-reference).

## Child-stream lifecycle on parent delete

Soft-delete of a parent Stream **orphans** child Streams: their `parent_id` becomes invalid but the children themselves persist.

- On read, the UI displays orphaned children at the top level with a `(was: <parent name>)` annotation, read from the parent's tombstone (which retains `name` for ≤ 30 days post-delete).
- The user MAY re-parent the orphan, in which case the new `parent_id` is validated as usual.
- Hard-delete of the parent (after compaction removes the tombstone) drops the annotation; orphans become regular top-level Streams.
