---
status: draft
---

# Contexts (a.k.a. Tags)

Cross-cutting facets applied to Tasks across Streams.

## Why two concepts (Stream and Context)?

- **Stream** answers "which line of work?" — exclusive, primary axis.
- **Context** answers "what enables doing it?" — non-exclusive, cross-cutting.

GTD uses contexts (`@home`, `@phone`, `@errand`). Sunrise extends the idea: contexts also denote *energy* (`@deep-work`, `@shallow`), *location* (`@office`, `@home`), *status* (`@waiting-on:carlos`), or anything the user wants. The system does not interpret context names except for two reserved prefixes (below).

## Fields

```cddl
Context = {
    id:         tstr .regexp "ctx_[A-Z0-9]{26}",
    created_at: tdate,
    updated_at: tdate,
    name:       text<64>,             ; user-shown label, no leading "@"
    color?:     ContextColor,
    description?: text<280>,
    archived:   bool,
    deleted:    bool,
}
```

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
- Deleting a Context **removes it from all Tasks** (handled as a multi-op transaction in the CRDT — observed-remove set semantics make this safe under concurrent edits).

## CRDT mapping

- Context entity itself: map with LWW-register fields.
- Membership (Task ↔ Context) lives on the Task side as an OR-set.
