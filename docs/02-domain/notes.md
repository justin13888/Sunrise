---
status: accepted
---

# Notes

Notes are rich-text bodies attached to a parent entity (Task, Stream, Block). Notes do not exist as standalone entities.

> **Status: the `Note` entity is unreachable in v1.** `crates/sunrise-domain/src/note.rs`
> defines the struct and `0013_baseline.sql` creates a `notes` table, and
> nothing in between exists: **zero ops, zero commands, zero queries, zero
> writers** — no `InnerOp` variant, no `Command`, no `Query`, no UniFFI
> surface, and nothing that writes the table. `Query::EntityById` refuses
> `EntityKind::Note` explicitly. What *is* live is the `body` **field** on
> Task, Stream and Routine — which is a `NoteBody`, a different thing from a
> `Note`. [ADR-0020](../11-adr/0020-v1-must-demotions.md) §(c) deferred the
> free-standing entity while keeping notes-as-a-field a v1 MUST, and kept the
> struct and the table deliberately rather than deleting them. See also
> [`../implementation/overview.md`](../implementation/overview.md).

## Why constrained rich text (not Markdown)

- Markdown editors invite comparison to Obsidian, Bear, etc.; we are not building an editor product.
- A constrained schema is testable, mergeable, and renders consistently across every client.
- We export to Markdown; we don't store as Markdown.

## Wire shape

```cddl
; What is actually serialized, today, everywhere a NoteBody appears.
NoteBody = bstr
```

`NoteBody` is an **opaque byte string** on the wire. The core neither parses
nor validates its contents; the grammar below is the contract editors and
renderers agree on *inside* those bytes, not a shape the CBOR codec enforces.
That distinction matters for two reasons: a body that fails the grammar still
round-trips and still syncs, and adding a block kind is not a `DOC_SCHEMA_V`
change.

## Body grammar (renderer contract, not wire shape)

```cddl
; The decoded interior of a NoteBody's bytes. `NoteBlock` is deliberately not
; called `Block` — that name is taken by the time-block entity in
; time-blocks.md, and the two are unrelated.
NoteBodyContent = [* NoteBlock]

NoteBlock =
      Paragraph
    / Heading
    / List
    / Checklist
    / CodeBlock
    / Quote
    / Divider

Paragraph = {kind: "p", inline: [* Inline]}
Heading   = {kind: "h", level: 1..3, inline: [* Inline]}
List      = {kind: "ul" / "ol", items: [+ ListItem]}
ListItem  = {inline: [* Inline], children?: [* NoteBlock]}
Checklist = {kind: "task", items: [+ ChecklistItem]}
ChecklistItem = {checked: bool, inline: [* Inline]}
CodeBlock = {kind: "code", language?: text, content: text}
Quote     = {kind: "quote", inline: [* Inline]}
Divider   = {kind: "hr"}

Inline =
      {text: text, marks?: [* Mark]}
    / {kind: "link", href: text, label: text}
    / {kind: "ref", ref: tstr}        ; in-app entity link
    / {kind: "mention", person: tstr}

Mark = "bold" / "italic" / "underline" / "strike" / "code"
```

Explicit non-features: tables, embedded images inline, custom styles, fonts, colors. Images attach via [`attachments.md`](./attachments.md).

## The `Note` entity

Specified for completeness. **Nothing writes this today** — see the banner at
the top of this file.

```cddl
Note = {
    id:         tstr .regexp "not_[0-9A-HJKMNP-TV-Z]{26}",
    created_at: timestamp,
    updated_at: timestamp,
    parent:     entity-ref,   ; Task, Stream or Block
    body:       NoteBody,     ; opaque bstr
    deleted:    bool,
    unknown-fields,           ; see overview.md
}
```

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

## Merge mapping

A `NoteBody` is a byte string that merges as part of its owning entity's row:
one last-writer-wins unit on `(hlc, device_id, seq)`
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)). **Two people typing in
the same body produce one survivor, not a character-level merge** — that needs
a text CRDT, and the workspace ships no CRDT library. ADR-0014 §What we give
up names this file specifically. Character-level merge is the target state.

## Editor surface

| Platform | Editor |
|---|---|
| macOS | SwiftUI text editing, schema-locked to NoteBody |
| Web (deferred, [ADR-0012](../11-adr/0012-web-wasm-deferred.md)) | Tiptap or ProseMirror, schema-locked to NoteBody |
| iOS (deferred) | Native textview with custom toolbar |
| Android (deferred) | Native EditText with custom toolbar |
| `sunrise` CLI | Plain text only; no structured-body editing |

All editors emit and consume the same `NoteBody` bytes.

## In-app references

`{kind: "ref", ref: "tsk_…"}` renders as the target entity's title; clicking navigates. References are **scrubbed at egress** when sharing the parent Stream with someone who doesn't have access to the referenced entity (becomes `{kind: "redacted"}`).

The redacted form is:

```cbor
{
  kind: "redacted",
  reason: "private_ref" | "external_account" | "deleted_entity",
  placeholder_text: "(redacted)"   ; used by editors that need a visible token
}
```

The original target id is **not** preserved in the redacted form sent to a recipient who shouldn't see it.

A reference whose target is soft-deleted renders as `{kind: "redacted", reason: "deleted_entity", placeholder_text: "(removed)"}` for the local user as well. The original reference id is preserved in the local state so a user-initiated undelete restores the link automatically.

## Length

Soft limit: 64KB per NoteBody. Beyond that, performance degrades; UI nudges the user toward splitting. Hard limit: 1MB.
