---
status: accepted
---

# Glossary

Shared vocabulary used throughout these specs. When a term in this glossary is used in a spec, it carries the meaning defined here — not its colloquial sense.

| Term | Definition |
|---|---|
| **Task** | The unit of intent. Has a title, optional notes, optional due/scheduled time, optional duration, and zero-or-more contexts. See [`02-domain/tasks.md`](../02-domain/tasks.md). |
| **Stream** | A long-lived bucket of related work (e.g. "Work: Acme", "Family", "Travel: Japan 2026", "Side: Sunrise"). Tasks belong to exactly one stream. Streams are the primary axis of multi-stream operation. |
| **Context** | A cross-cutting tag that applies to tasks across streams (e.g. `@deep-work`, `@errand`, `@waiting-on:carlos`). Not a stream. Many-to-many with tasks. |
| **Routine** | A recurring task or habit, defined by a recurrence rule and an optional "skip if not done" policy. Routines materialize into per-occurrence tasks. |
| **Block** | A scheduled time range, optionally bound to one or more tasks. Blocks live next to tasks but are not tasks. |
| **Note** | Free text (constrained rich text) attached to a Task, Stream, or Block. Not standalone. |
| **Inbox** | The set of tasks not yet assigned to a stream. New captures land here by default. |
| **Today** | A computed view: scheduled-today + due-today + manually-promoted tasks. Not a list the user edits directly except by promote/demote. |
| **Promote** | Moving a task from Inbox to a Stream, or from a Stream's backlog to Today. |
| **Defer** | Moving a task's scheduled date forward, with the system tracking that it was deferred (for review surfacing). |
| **Routine occurrence** | A specific materialization of a routine for a given date/cadence. |
| **Stream filter** | A saved view that narrows by stream, context, time range, or other facets. |
| **Identity** | The user's cryptographic identity — a long-lived keypair (see [`03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md)). One identity, many devices. |
| **Device** | A specific install (phone, laptop, browser profile). Has its own keypair derived from the identity. |
| **Pairing** | The process of authorizing a new device to join an identity. |
| **Recovery code** | A user-held secret allowing identity reconstruction with no other device available. |
| **Op** | A CRDT operation — the atomic unit of sync. See [`05-sync/crdt-design.md`](../05-sync/crdt-design.md). |
| **Op log** | The append-only sequence of ops on a given device. |
| **Relay** | The server's role in passing encrypted ops between devices. The relay cannot read ops. |
| **Vault** | The local encrypted store on a device. |
| **Shared document** | A subgraph of state shared between two or more identities (a stream, a task, etc.) via cross-identity sharing. |
| **Stream of work** | Synonym for Stream. Used in user-facing copy where "stream" alone might be ambiguous. |
