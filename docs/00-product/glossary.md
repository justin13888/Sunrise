---
status: accepted
---

# Glossary

Shared vocabulary used throughout these specs. When a term in this glossary is used in a spec, it carries the meaning defined here — not its colloquial sense.

Amended by [ADR-0046](../11-adr/0046-optional-stream.md),
[ADR-0047](../11-adr/0047-deadlines-and-lateness.md),
[ADR-0050](../11-adr/0050-preferences-and-day-schedule.md) and
[ADR-0051](../11-adr/0051-places.md), which change the Stream, Inbox, Today and
Defer entries and add the rest of the time, triage, settings and place terms.

| Term | Definition |
|---|---|
| **Task** | The unit of intent. Has a title, optional notes, an optional plan time and soft and hard deadlines, optional duration, and zero-or-more contexts. See [`02-domain/tasks.md`](../02-domain/tasks.md). |
| **Stream** | A long-lived bucket of related work (e.g. "Work: Acme", "Family", "Travel: Japan 2026", "Side: Sunrise"). A task belongs to at most one stream; a task with none is stream-less, not in a hidden stream ([ADR-0046](../11-adr/0046-optional-stream.md)). Streams are the primary axis of multi-stream operation. |
| **Private key domain** | The per-vault key domain that seals stream-less tasks and blocks. It rotates like a stream key and is never a stream, never listed and never shared ([ADR-0046](../11-adr/0046-optional-stream.md)). |
| **Context** | A cross-cutting tag that applies to tasks across streams (e.g. `@deep-work`, `@errand`, `@waiting-on:carlos`). Not a stream. Many-to-many with tasks. |
| **Routine** | A recurring task or habit, defined by a recurrence rule and an optional "skip if not done" policy. Routines materialize into per-occurrence tasks. |
| **Block** | A scheduled time range, optionally bound to one or more tasks. Blocks live next to tasks but are not tasks. |
| **Note** | Free text (constrained rich text) attached to a Task, Stream, or Block. Not standalone. |
| **Inbox** | A view, not a place: open tasks with no stream, no plan time, no deadline and no explicit triage mark. New captures land here by default ([`02-domain/tasks.md` §Inbox](../02-domain/tasks.md#inbox)). |
| **Today** | A computed view over the planner day containing now: tasks planned or due in it, late tasks, and manually-promoted tasks. Not a list the user edits directly except by promote/demote. |
| **Planner day** | The span of time one day covers, set by the day schedule's wake and sleep times in the reader's zone; the civil day when no schedule is set ([`02-domain/day-schedule.md`](../02-domain/day-schedule.md)). |
| **Day schedule** | The user's usual wake and sleep times, per weekday, per day of month and per date. It defines the planner day and the wind-down reminder ([`02-domain/day-schedule.md`](../02-domain/day-schedule.md)). |
| **Preferences** | The user's settings: one vault-synced typed entity plus a device-local overlay, with each key's scope declared ([`02-domain/preferences.md`](../02-domain/preferences.md)). |
| **Place** | A user-entered named location (coordinates and radius) that a task can require. Presence is evaluated on the device and never stored ([`02-domain/places.md`](../02-domain/places.md)). |
| **Promote** | Moving a task from Inbox to a Stream, or from a Stream's backlog to Today. |
| **Defer** | Moving a task's plan time (`planned_at`) later, with the system counting the deferral (for review surfacing). Never moves a deadline. |
| **Lateness** | A task's derived state: on track, late (soft), late (hard) or stale. Computed at read time, never stored ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)). |
| **Triage queue** | The view of late and stale tasks awaiting a decision: defer, already done, drop or keep ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)). |
| **Routine occurrence** | A specific materialization of a routine, identified by its intended civil start (the occurrence key). |
| **Stream filter** | A saved view that narrows by stream, context, time range, or other facets. |
| **Identity** | The user's cryptographic identity — a long-lived keypair (see [`03-crypto/identity-and-device-keys.md`](../03-crypto/identity-and-device-keys.md)). One identity, many devices. |
| **Device** | A specific install (phone, laptop, browser profile). Has its own keypair derived from the identity. |
| **Pairing** | The process of authorizing a new device to join an identity. |
| **Recovery code** | A user-held secret allowing identity reconstruction with no other device available. |
| **Op** | The atomic unit of sync: one encrypted, signed mutation of one entity. Merged per field — registers, OR-sets and counters — by [ADR-0044](../11-adr/0044-per-field-ops.md), which supersedes the entity-level last-writer-wins of [ADR-0014](../11-adr/0014-entity-level-lww-merge.md); the rules in force are [`05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md). |
| **Op log** | The append-only sequence of ops on a given device. |
| **Relay** | The server's role in passing encrypted ops between devices. The relay cannot read ops. |
| **Vault** | The local encrypted store on a device. |
| **Shared document** | A **Stream** (and all its descendant entities) shared between two or more identities. The Stream is the unit; there is no per-Task ACL ([`03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md)). Not built yet ([ADR-0027](../11-adr/0027-v1-self-host-first.md)). |
| **Stream of work** | Synonym for Stream. Used in user-facing copy where "stream" alone might be ambiguous. |
