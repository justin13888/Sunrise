---
status: accepted
---

# Notes

Notes are rich-text bodies attached to a parent entity (Task, Stream, Block). Notes do not exist as standalone entities.

## Why constrained rich text (not Markdown)

- Markdown editors invite comparison to Obsidian, Bear, etc.; we are not building an editor product.
- A constrained schema is testable, CRDT-mergeable, and renders consistently across all clients (especially TUI).
- We export to Markdown; we don't store as Markdown.

## Schema

```cddl
NoteBody = [* Block]

Block =
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
ListItem  = {inline: [* Inline], children?: [* Block]}
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

## CRDT mapping

A `NoteBody` is stored as a Loro `RichText` doc. Concurrent edits merge character-level; concurrent block-structure edits use list semantics.

## Editor surface

| Platform | Editor |
|---|---|
| Desktop / Web | Tiptap or ProseMirror, schema-locked to NoteBody |
| iOS | Native textview with custom toolbar |
| Android | Native EditText with custom toolbar |
| TUI | Vim-style modal editing; export-to-`$EDITOR` for long edits |

All editors emit / consume the same `NoteBody` JSON (or its CRDT equivalent). The TUI's "open in $EDITOR" round-trips through Markdown via a lossy converter; the converter logs warnings on lossy elements.

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

A reference whose target is soft-deleted renders as `{kind: "redacted", reason: "deleted_entity", placeholder_text: "(removed)"}` for the local user as well. The original reference id is preserved in the local CRDT state so a user-initiated undelete restores the link automatically.

## Length

Soft limit: 64KB per NoteBody. Beyond that, performance degrades; UI nudges the user toward splitting. Hard limit: 1MB.
