---
status: draft
---

# CRDT Design

## Choice of CRDT library

**Loro** (Rust, with WASM bindings). See [`../11-adr/0003-crdt-loro-vs-automerge.md`](../11-adr/0003-crdt-loro-vs-automerge.md).

Reasons:

- Native Rust; cleanly bindable to mobile and WASM with the same crate.
- Supports map, list, text, counter, movable list — a superset of what we need.
- Compact binary encoding.
- Active development; reasonable benchmarks.

Automerge was the alternative; rejected for v1 due to slower mobile performance and a heavier op encoding.

## Document layout

We do **not** use one giant CRDT doc. Instead:

- One Loro doc per **Stream**.
- Plus one "vault meta" doc per identity for: device certs, stream registry, contexts, person registry, settings.
- Plus per-attachment metadata is part of the parent Stream's doc.

Why per-Stream:

- **Sharing maps cleanly.** One Stream → one doc → one set of access keys.
- **Sync per-stream is independent.** A heavily-edited Stream doesn't drag a quiet one.
- **Compaction is per-stream.** No global rewrite events.

## Mapping domain entities to CRDT shapes

| Domain field | CRDT type |
|---|---|
| Scalar (title, state, due_at, …) | LWW-register, with `(timestamp, device_id)` tiebreak |
| `contexts` (Set<ContextId>) | Observed-Remove Set |
| `tasks` on a Block (Set<TaskId>) | Observed-Remove Set |
| `body` (NoteBody) | Loro RichText |
| `stream_order` (parent → ordered children) | Loro List with fractional indices |
| `streak_counter` | PN-counter |
| `deferred_count` | PN-counter |

## Concurrent writes — concrete cases

| Scenario | Result |
|---|---|
| Device A renames task; Device B renames task | LWW; later timestamp wins; if equal, lexicographic device_id wins |
| Device A adds context X; Device B removes context X | OR-Set semantics; the **add** wins if the remove's "observed-add" reference is to an *earlier* add; otherwise remove wins |
| Device A marks done; Device B edits title | Both apply; task is done with new title |
| Device A deletes task; Device B edits same task | Task remains deleted (delete is "all-fields-tombstoned"); B's edit becomes effectively no-op once tombstone is observed |
| Two devices independently complete a routine occurrence | Both completions counted? PN-counter is increment-only here; we model "completed" as a state, not a counter. So: LWW on `state=done`, with one `completed_at`; the streak counter increments by 1 (idempotent on op id) |

## Op encoding

We do *not* use Loro's native binary directly on the wire. Instead, we wrap Loro ops inside our own envelope:

- Loro op produced by domain mutation.
- Serialized via Loro's encoding to bytes.
- Wrapped in a Sunrise op envelope (encrypted, signed) — see [`../03-crypto/data-encryption-format.md`](../03-crypto/data-encryption-format.md).
- Stored and transported as the envelope.

Receivers unwrap, verify, decrypt, then feed the inner Loro op to the local Loro doc.

## Snapshots

For sync efficiency on cold-start and for compaction, we periodically emit Loro doc snapshots wrapped in a snapshot op (see [`../04-storage/compaction.md`](../04-storage/compaction.md)).

## CRDT properties tested

- **Convergence.** Property test: random op sequences applied in any order across N simulated devices produce identical final state.
- **Causal preservation.** Receivers never expose state derived from an op whose deps haven't been applied.
- **Determinism.** Same op set, same final state, byte-for-byte.
