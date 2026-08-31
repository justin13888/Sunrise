---
status: accepted
---

# Contexts (a.k.a. Tags)

Cross-cutting facets applied to Tasks across Streams.

## Why two concepts (Stream and Context)?

- **Stream** answers "which line of work?" — exclusive, primary axis.
- **Context** answers "what enables doing it?" — non-exclusive, cross-cutting.

GTD uses contexts (`@home`, `@phone`, `@errand`). Sunrise extends the idea: contexts also denote *energy* (`@deep-work`, `@shallow`), *location* (`@office`, `@home`), *status* (`@waiting-on:carlos`), or anything the user wants. The system does not interpret context names except for two reserved prefixes (below).

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

```cddl
Context = {
    id:         tstr .regexp "ctx_[0-9A-HJKMNP-TV-Z]{26}",
    created_at: timestamp,
    updated_at: timestamp,
    name:       text<64>,             ; user-shown label, no leading "@"
    description?: tstr,               ; shown in the picker
    archived:   bool,
    deleted:    bool,
    unknown-fields,                   ; see overview.md
}
```

**There is no `color` field, and no `ContextColor` type.** Contexts are
rendered as text; colour is a Stream affordance, not a Context one
([`streams.md`](./streams.md)). Earlier revisions of this spec declared one and
nothing ever serialized it.

`description` is not length-capped in the domain — only `name` goes through
`validate_title`, at 64 chars after trim. A client MAY impose its own limit.

## Reserved prefixes

The UI may interpret these prefixes specially when rendering or sorting:

| Prefix | Meaning | UI treatment |
|---|---|---|
| `waiting-on:` | Task blocked on a person/thing | Surface in weekly review; auto-suggest follow-up |
| `energy:` | One of `low`, `med`, `high` | Filter/sort affordance in Today view |

These are conventions, not enforced types. A user can ignore them.

## Capture syntax

In quick capture, `@context-name` adds an existing context (creating it if absent, with a confirmation if the trailing whitespace fired the parse on an unintended word).

## Usage rules

- A Task can have many contexts.
- A Context cannot have child contexts (no nesting).
- Archiving a Context does not strip it from existing Tasks; it just hides it from the picker.
- Deleting a Context **removes it from all Tasks**.

## Merge mapping

The Context entity is one last-writer-wins unit on `(hlc, device_id, seq)`
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)). Membership
(Task ↔ Context) lives on the Task side, in `Task.contexts`, and merges with
the rest of the Task row — so a concurrent add on one device and remove on
another resolves by timestamp, **not** add-wins. An observed-remove set is the
target state; see [`tasks.md` §Merge mapping](./tasks.md#merge-mapping).

Deleting a Context emits **one** op (`context.delete`), not one Task op per
membership. Each device, applying that op, purges the Context from its own
`task_contexts` projection and refreshes the affected Tasks' search rows — so
the effect is deterministic on every replica without a multi-op transaction.
A later `task.update` that still names the deleted Context re-adds the
membership row, because a Task op carries its whole context set. Readers
therefore treat a `ctx_` ref with no live Context as absent rather than as an
error.
