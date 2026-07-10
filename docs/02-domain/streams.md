---
status: accepted
---

# Stream

A long-lived bucket of related work. Streams are the **primary axis** of multi-stream operation.

## Fields

```cddl
Stream = {
    id:           tstr .regexp "str_[A-Z0-9]{26}",
    created_at:   tdate,
    updated_at:   tdate,
    name:         text<128>,
    description?: NoteBody,
    color:        StreamColor,             ; from a fixed palette
    icon?:        StreamIcon,              ; from a fixed icon set
    parent_id?:   tstr,                    ; one-level nesting allowed
    sort_order:   tstr,                    ; fractional-index string
    archived:     bool,
    paused:       bool,                    ; routines pause; due-date warnings suppressed
    paused_until?: tdate,                  ; auto-unpause time
    review_cadence?: ReviewCadence,        ; weekly | biweekly | monthly | none
    default_context?: tstr,                ; assigned to new tasks in this stream
    integrations: { * IntegrationKey => IntegrationConfig },
    deleted:      bool,
}

ReviewCadence = "weekly" / "biweekly" / "monthly" / "none"
```

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

`sort_order` is a fractional-index string (see e.g. `fractional-indexing` algorithm). New streams default between the last and "end." Reorders are constant work; no list-shifting.

The string is bounded by a defrag rule:

- A device that observes a `sort_order` string ≥ 64 bytes triggers a defrag op for the affected list.
- Defrag is a single op `stream.list.defrag` carrying the new index for every entry. It is idempotent — concurrent defrag from two devices produces the same output (lex-sort the entries by `(ts_ms, device_id_lex)` and re-assign indexes evenly across `[A, Z]`).
- The trigger is per-device throttled to once per list per hour to prevent thrash.

## Sharing

A Stream is the **unit of sharing** with another identity. Sharing a Stream:
- Grants the recipient access to the Stream entity, all its Tasks, attached Notes, and Blocks bound to those Tasks.
- Does **not** grant access to other Streams' content, even if a Task in this Stream is `blocked_by` a Task in another (the linked task surfaces only by ID + redacted title).

See [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md).

## CRDT mapping

- Stream is a map.
- Scalars: LWW-register.
- `integrations` is a map keyed by integration kind; values are LWW-registers of opaque (per-integration) JSON.

## Validation

- `name` MUST be non-empty.
- `parent_id` MUST refer to a non-archived, non-deleted Stream **at the time the `parent_id` is set or changed**. The validation is enforced on `parent_id` mutations only, not retroactively (see "Child-stream lifecycle on parent delete" below).
- Cycles are impossible by structure (one-level limit + no parent self-reference).

## Child-stream lifecycle on parent delete

Soft-delete of a parent Stream **orphans** child Streams: their `parent_id` becomes invalid but the children themselves persist.

- On read, the UI displays orphaned children at the top level with a `(was: <parent name>)` annotation, read from the parent's tombstone (which retains `name` for ≤ 30 days post-delete).
- The user MAY re-parent the orphan, in which case the new `parent_id` is validated as usual.
- Hard-delete of the parent (after compaction removes the tombstone) drops the annotation; orphans become regular top-level Streams.
