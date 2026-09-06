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
| ReviewSnapshot | 1/week; append-only, never edited | [`../08-features/reviews-and-stats.md`](../08-features/reviews-and-stats.md) |

## Common CDDL types

Every entity spec in this directory writes its fields as CDDL. The rules below
are shared by all of them and are defined once here rather than repeated.

```cddl
; ---------------------------------------------------------------------------
; Identifiers. A typed reference is a 4-char prefix plus a 26-char Crockford
; base32 ULID: 30 chars total. See identifiers.md for the prefix registry.
; ---------------------------------------------------------------------------
entity-ref = tstr .regexp "[a-z]{3}_[0-9A-HJKMNP-TV-Z]{26}"

; ---------------------------------------------------------------------------
; Time. Nothing in the domain emits a CBOR tag, so these are plain text
; strings, NOT the prelude's tagged `tdate`. An instant is a jiff
; `Timestamp` (RFC 3339, always UTC, `Z`-suffixed, fractional seconds only
; when non-zero); the civil types are jiff `civil::*` values in their full
; seconds form. See ../11-adr/0011-datetime-jiff.md.
; ---------------------------------------------------------------------------
timestamp      = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?Z"
civil-datetime = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]+)?"
civil-date     = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}"
civil-time     = tstr .regexp "([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]"

; A few append-only records (FocusStart, FocusEnd, ReviewSnapshot) carry
; integer epoch milliseconds under an explicit `_ms` key instead. That is the
; exception, and the key name is what marks it.
epoch-ms = int

; ---------------------------------------------------------------------------
; Rich text. v1 stores a NoteBody as an opaque byte string; the structured
; block grammar in notes.md is the *rendering* contract, not the wire shape.
; ---------------------------------------------------------------------------
NoteBody = bstr

; ---------------------------------------------------------------------------
; Forward compatibility. EVERY persisted entity below carries this: fields a
; newer DOC_SCHEMA_V wrote that this build does not model are preserved
; verbatim at the top level of the entity map and re-emitted byte-for-byte.
; It is spelled as a serde `flatten`, so the keys sit alongside the modelled
; ones rather than nested under a container. See
; ../10-cross-cutting/protocol-versioning.md §7.
;
; The one exception is Interruption (focus-mode), whose whole value is its
; primary key; it declares no unknown-fields.
; ---------------------------------------------------------------------------
unknown-fields = ( * tstr => any )
```

`text<N>` below means a `tstr` of at most N **characters after trim** (not
bytes) — the rule `validate_title` enforces.

## Hard rules

- A **Task** belongs to exactly **one Stream** at a time. Tasks not yet assigned live in a special pseudo-stream `Inbox`.
- A **Task** can be tagged with **0..N Contexts**.
- A **Block** can bind to **0..N Tasks**. Tasks can have **0..N Blocks** scheduled.
- **Notes** are children of an entity; they cannot float free.
- **Routines** generate Tasks; once a generated task exists, edits to the task do not retroactively affect future occurrences (unless the user explicitly chooses "edit series").
- A **FocusSession** belongs to exactly one Task and is **append-only**: a `start` record and, later, a separate `end` record sharing one id. It is never edited and never deleted, and a `start` with no `end` means the session is still running.
- A **ReviewSnapshot** is **append-only in the strong sense**: `review.snapshot`
  is the only op family it has — there is no update op and no delete op, and no
  patch type. The consequence is user-visible and worth stating plainly: the
  free-text `note` a user writes during a weekly review **cannot be edited
  afterwards**, and a snapshot saved by mistake **cannot be removed**. Unlike
  the `Attachment` case below, nothing about the entity makes that necessary —
  a review note is prose the user typed, not a description of one specific run
  of bytes — so this is a gap rather than a decision.
- An **Attachment** is **write-once by design**, and that one is defensible.
  Every field but `deleted` describes one specific run of ciphertext identified
  by `content_hash`, so changing any of them would be describing different
  bytes; re-attaching an edited file is a new attachment, which is what content
  addressing already implies. `AttachFile` and `DetachFile` are the only two
  commands, and there is deliberately no patch type. See
  [`attachments.md`](./attachments.md) §Write-once metadata.
- **People** are first-class identities, including the local user. A Task assigned to a non-self Person is a "watching/waiting" annotation in v1, not a delegation primitive.
- A **Task** or **Routine** may carry **scheduling constraints** — requirement windows (time-of-day / days-of-week / date-range, each `hard` or `soft`) restricting when it should be scheduled. These are a *value type*, not an entity: they mint no ID (see [`scheduling-constraints.md`](./scheduling-constraints.md)) and add no prefix to [`identifiers.md`](./identifiers.md).

## Identity vs. identity

We use "identity" two ways and need to keep them straight:

- **Cryptographic identity** (one keypair, one user across many devices) — [`../03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md).
- **Person entity** in the domain — a node in the graph that *may or may not* correspond to a cryptographic identity. A note about "Mom" who isn't on Sunrise creates a Person with no linked cryptographic identity.

When sharing, only Persons with linked cryptographic identities can be granted access.

## Relationship to CRDT shape

> **Target state, not v1.** Everything in this section describes the deferred
> per-field merge design. v1 merges each entity as one unit by last-writer-wins
> ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)); each entity spec's
> §Merge mapping is the shipped behaviour.

**Target state, none of it implemented.** ADR-0003 is superseded by
[ADR-0014](../11-adr/0014-entity-level-lww-merge.md): v1 merges at entity
granularity with LWW in SQLite and ships **no CRDT library**, so no entity maps
to a CRDT subtree today and none of the per-field types below exists. The
deferred design is [`../05-sync/crdt-design.md`](../05-sync/crdt-design.md)
(`proposed`); the rules actually in force are
[`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md). Read
the list in the conditional:

- Entities would be *maps* keyed by ID.
- Each entity would itself be a map of fields.
- Lists (e.g. an ordered child-task list) would be CRDT lists (RGA-flavored).
- Sets (e.g. contexts on a task) would be observed-remove sets.
- Counters (e.g. routine streak) would be PN-counters.

That model is Loro's (see [`../11-adr/0003-crdt-loro-vs-automerge.md`](../11-adr/0003-crdt-loro-vs-automerge.md)), and `loro` is in no `Cargo.toml` in the workspace.
