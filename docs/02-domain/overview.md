---
status: accepted
---

# Domain Model — Overview

The domain is small on purpose. Every entity below earns its keep against [`../00-product/core-workflows.md`](../00-product/core-workflows.md).

```
                      ┌────────┐
                      │ Person │ (user identities, including the local user
                      └────┬───┘  and any contacts referenced in tasks)
                           │
        ┌──────────────────┼────────────────────┐
        ▼                  ▼                    ▼
   ┌────────┐        ┌──────────┐         ┌──────────┐
   │ Stream │◀───────│   Task   │────────▶│ Context  │
   └───┬────┘        └────┬─────┘         └──────────┘
       │                  │  (M:N)
       │                  │
       ▼                  ▼
   ┌────────┐        ┌──────────┐
   │  Note  │◀──────▶│  Block   │ (time-block; may bind 0..N tasks)
   └────────┘        └──────────┘
       ▲
       │
   ┌───┴────────┐
   │ Attachment │
   └────────────┘

   ┌──────────┐
   │ Routine  │── materializes ─▶ Task (per occurrence)
   └──────────┘
```

## Entity summary

| Entity | Cardinality (typical) | Spec |
|---|---|---|
| Person | 1 self + few contacts | [`people-and-sharing.md`](./people-and-sharing.md) |
| Stream | 5–20 | [`streams.md`](./streams.md) |
| Task | 100s–10ks | [`tasks.md`](./tasks.md) |
| Context | 5–30 | [`contexts-and-tags.md`](./contexts-and-tags.md) |
| Routine | 5–50 | [`routines-and-recurrence.md`](./routines-and-recurrence.md) |
| Block | 5–500/week (mostly transient) | [`time-blocks.md`](./time-blocks.md) |
| Note | 0..N per Task/Stream/Block | [`notes.md`](./notes.md) |
| Attachment | rare; capped per vault | [`attachments.md`](./attachments.md) |
| FocusSession | 1–20/day; append-only, never edited | [`../08-features/focus-mode.md`](../08-features/focus-mode.md), [ADR-0013](../11-adr/0013-focus-session-op-representation.md) |

## Hard rules

- A **Task** belongs to exactly **one Stream** at a time. Tasks not yet assigned live in a special pseudo-stream `Inbox`.
- A **Task** can be tagged with **0..N Contexts**.
- A **Block** can bind to **0..N Tasks**. Tasks can have **0..N Blocks** scheduled.
- **Notes** are children of an entity; they cannot float free.
- **Routines** generate Tasks; once a generated task exists, edits to the task do not retroactively affect future occurrences (unless the user explicitly chooses "edit series").
- A **FocusSession** belongs to exactly one Task and is **append-only**: a `start` record and, later, a separate `end` record sharing one id. It is never edited and never deleted, and a `start` with no `end` means the session is still running.
- **People** are first-class identities, including the local user. A Task assigned to a non-self Person is a "watching/waiting" annotation in v1, not a delegation primitive.
- A **Task** or **Routine** may carry **scheduling constraints** — requirement windows (time-of-day / days-of-week / date-range, each `hard` or `soft`) restricting when it should be scheduled. These are a *value type*, not an entity: they mint no ID (see [`scheduling-constraints.md`](./scheduling-constraints.md)) and add no prefix to [`identifiers.md`](./identifiers.md).

## Identity vs. identity

We use "identity" two ways and need to keep them straight:

- **Cryptographic identity** (one keypair, one user across many devices) — [`../03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md).
- **Person entity** in the domain — a node in the graph that *may or may not* correspond to a cryptographic identity. A note about "Mom" who isn't on Sunrise creates a Person with no linked cryptographic identity.

When sharing, only Persons with linked cryptographic identities can be granted access.

## Relationship to CRDT shape

Each domain entity maps to a CRDT subtree. See [`../05-sync/crdt-design.md`](../05-sync/crdt-design.md). Briefly:

- Entities are *maps* keyed by ID.
- Each entity is itself a map of fields.
- Lists (e.g. an ordered child-task list) are CRDT lists (RGA-flavored).
- Sets (e.g. contexts on a task) are observed-remove sets.
- Counters (e.g. routine streak) are PN-counters.

This maps directly into Loro's data model (see [`../11-adr/0003-crdt-loro-vs-automerge.md`](../11-adr/0003-crdt-loro-vs-automerge.md)) — **target state**. ADR-0003 is superseded by [ADR-0014](../11-adr/0014-entity-level-lww-merge.md): v1 merges at entity granularity with LWW in SQLite and ships no CRDT library.
