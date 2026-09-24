# 0046 — A task's stream is optional, stream-less content lives in a private key domain, and Inbox is a view

**Status:** accepted

**Amends** [ADR-0024](./0024-key-hierarchy.md): the key hierarchy gains one
reserved, per-vault key domain that is not a Stream. It adds no primitive and
no new key type. Every rule ADR-0024 states for a Stream key (random per epoch,
wrapped, distributed by `key_envelope`, rotated on revocation) applies to it
unchanged.

**Amends** [`../02-domain/tasks.md`](../02-domain/tasks.md),
[`../02-domain/streams.md`](../02-domain/streams.md),
[`../02-domain/time-blocks.md`](../02-domain/time-blocks.md) and
[`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md).

**Depends on** [ADR-0044](./0044-per-field-ops.md) (per-field ops) and ADR-0045
(`vault_requires` feature ids and parked ops). Implementation is tracked in [#332](https://github.com/justin13888/Sunrise/issues/332).

## Context

A Task must belong to a Stream today. `Task.stream_id` is a required
`EntityRef` (`crates/sunrise-domain/src/task.rs#Task`), and the storage row
enforces it with `stream_id BLOB NOT NULL REFERENCES streams`. Routine templates
and Blocks carry the same constraint. "No stream" is spelled as a sentinel
Stream with a fixed id, `INBOX_STREAM_BYTES`
(`crates/sunrise-domain/src/inbox.rs`), and a draft with no
stream is written into it (`crates/sunrise-core/src/engine/task.rs#create_task`).

That conflates two facts that are different:

1. **"This task belongs to no project."** A personal errand, a thought, a
   one-off. It is a permanent and legitimate home.
2. **"Nobody has looked at this task yet."** An untriaged capture.

Because both are the same bit, a task can never be "triaged, and deliberately
not in any stream": moving it out of the Inbox *requires* inventing a stream.

Deleting a stream is also unsafe. `delete_stream` tombstones the stream row and
nothing else (`crates/sunrise-core/src/engine/stream.rs#delete_stream`). Its
tasks, blocks and routine templates keep a `stream_id` that names a deleted
stream, and routines keep generating into it. Nothing repairs this, and nothing
could repair it on a replica that receives a new task for the stream *after*
the delete, because the delete ran on another device.

One fact from the tree makes the fix cheap. The Inbox sentinel is **already a
key domain of its own**: `ensure_base_epochs` mints an epoch for it on every
vault open, beside the vault-meta stream
(`crates/sunrise-core/src/engine/oplog.rs#ensure_base_epochs`), and every Inbox
task op is sealed under that key. What the sentinel lacks is not a key; it is
the right *type*.

## Decision

### 1. `stream_id` is optional on every entity that carries one

```
Task.stream_id:          Option<StreamId>
Block.stream_id:         Option<StreamId>
TaskTemplate.stream_id:  Option<StreamId>
```

On the wire an absent `stream_id` means "no stream". It is a per-field LWW
register (ADR-0044), so moving a task between streams, or out of every
stream, is one field write.

### 2. Stream-less content lives in the reserved private key domain

The key hierarchy names **three kinds of key domain**, and the type that names
one is a closed enum, not a stream id:

```rust
enum KeyDomain {
    Meta,              // vault-meta: see the table below
    Private,           // stream-less tasks, blocks and external events
    Stream(StreamId),  // a user-created Stream
}

fn domain_of(task: &Task) -> KeyDomain {
    task.stream_id.map_or(KeyDomain::Private, KeyDomain::Stream)
}
```

Every entity kind seals its ops under exactly one domain:

| Entity | Key domain |
|---|---|
| Task, Block | `Stream(id)` when its effective `stream_id` names a stream; `Private` otherwise |
| ExternalEvent (calendar) | `Stream(id)` when the integration account names a target stream; `Private` otherwise ([ADR-0049](./0049-calendar-integrations-per-device-oauth.md)) |
| Stream | `Meta` |
| Routine (template and rule) | `Meta`. The template's `stream_id` is an ordinary field of a vault-meta entity, so re-pointing it is a `Patch` in `Meta` and never a re-seal. |
| Context | `Meta` |
| SavedView (saved searches) | `Meta` |
| IntegrationAccount | `Meta` ([ADR-0025](./0025-integration-account-entity.md)) |
| Preferences | `Meta` ([ADR-0050](./0050-preferences-and-day-schedule.md)) |
| Place | `Meta` ([ADR-0051](./0051-places.md)) |

A routine's generated occurrences are Tasks, so each is sealed by the Task row
of this table, under the domain its own `stream_id` names.

**Identifier.** The private domain's 16-byte id is `INBOX_STREAM_BYTES`, the
bytes the Inbox sentinel already uses (`00 00 00` followed by ASCII
`sunrise.inbox`). The constant is renamed `PRIVATE_DOMAIN_BYTES`, and the ASCII
is kept as a historical spelling. Reusing the bytes is the whole reason the
migration is cheap:

- every existing Inbox op is *already* sealed under this domain's epochs, so no
  op is re-encrypted;
- every replica already holds those epochs, because `ensure_base_epochs` mints
  them on open and pairing carries them;
- the relay routes by the envelope's 16-byte id, so nothing changes on the
  server;
- the bytes are below any ULID's timestamp prefix, so no Stream can ever be
  minted with them (the argument in `inbox.rs` still holds).

**Key lifecycle.** The same as a Stream key's, with no exception:
random 32 bytes per epoch, wrapped under the vault root, distributed to devices
and to the identity by `key_envelope`, and rotated together with every Stream
whenever a device is revoked (ADR-0024 decision 5,
[`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md)). It is minted at
vault open by `ensure_base_epochs`, as it is today.

**It is never a Stream.**

- It has no stream row, and `Query::StreamList` never returns it.
- No command takes a `KeyDomain`. Stream commands take a `StreamId`, and
  `KeyDomain::Private` cannot be converted into one, so "rename the private
  domain", "share the private domain" and "delete the private domain" do not
  type-check. The runtime guard `require_writable_stream` stays as a second
  line for bytes that arrive from a peer.
- It is never shared. Sharing ([`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md))
  grants a `StreamId`, and a stream-scoped grant can never carry a private
  domain key. A test pins that a share export contains no private-domain epoch.
- It is never shown as a stream. The sidebar's "Inbox" is a view (§3), not a
  row, and a stream picker offers "No stream" as the absence of a choice.

**Moving between domains.** A move of a Task or Block between domains is two
things in one transaction:

- **A `stream_id` `Patch`.** This is the only new write. It carries the new
  `stream_id` at the move's stamp, and it is sealed under the **source**
  domain as the departure marker (the entity id and the new `stream_id`,
  nothing else). A reader that holds only the source key therefore sees the
  entity leave instead of seeing a stale copy stay. The marker carries no
  title or body, so moving a task out of a shared stream leaks nothing about
  where it went beyond "not here".
- **A re-seal of the entity's field state under the destination domain,
  preserving each field's original stamp.** Every register travels with its
  existing `(value, stamp, origin)`, every OR-set with its existing add-tags
  and removed tags, and every counter with its existing deltas. A receiver
  merges them exactly as the original writes, so the re-seal is a carry, not
  a write: it cannot beat a concurrent newer edit to any field, and it does not
  touch a field's stamp. A fresh full write at the move's stamp would violate
  ADR-0044 §1's rule that an op carries only the fields its command wrote,
  and would re-create the overwrite ADR-0044 removes.

A routine template is not moved this way: it lives in `Meta` whatever its
`stream_id` says.

### 3. Inbox is a view, and "untriaged" is structural

```
inbox(task) =  effective_stream(task.stream_id) IS NULL     ; §4
           AND task.planned_at   IS NULL
           AND task.target_at    IS NULL
           AND task.hard_due_at  IS NULL
           AND task.triaged_at   IS NULL
           AND task.state        IN (todo, in_progress)
           AND NOT task.archived AND NOT task.deleted
```

A task is **untriaged** exactly when it has no home (no stream), no time (none
of ADR-0047's three time fields), and no explicit acknowledgement. It is a pure
predicate over fields every replica already merges, so:

- **No command has to remember to clear a flag.** Setting a stream or any of
  the three times removes a task from the Inbox *because* the predicate stops
  holding, not because the command wrote a second field. A peer that predates
  `triaged_at` and sets `planned_at` still moves the task out of the Inbox on
  every replica.
- **`triaged_at: timestamp?`** is the one explicit bit, and it exists only for
  the case the other fields cannot express: "I have looked at this, it belongs
  to no stream, and it has no date." `Command::MarkTriaged { ids }` sets it to
  now. `Command::ReturnToInbox { ids }` clears it, and the task re-enters the
  Inbox only if the other clauses also hold. It is a system-stamped instant,
  a per-field LWW register, and it is never inferred.

**Why not an explicit `triaged: bool` alone.** Every write path that gives a
task a home or a time would have to set it in the same op, and a path that
forgets (an importer, a routine, an older peer) leaves a task that is planned
for Tuesday and also sits in the Inbox. The predicate makes that state
unrepresentable. **Why not the fields alone.** "Personal, no date, dealt with"
is a real and common state, and without an explicit bit it can only leave the
Inbox by acquiring a fake date.

### 4. Deleting a stream re-homes everything it held, in one command

```rust
Command::DeleteStream { id: StreamId, disposition: Disposition }

enum Disposition {
    Detach,               // every dependent goes to no stream
    MoveTo(StreamId),     // every dependent goes to a live stream
}
```

In **one transaction**, the command:

1. writes `stream_id = disposition.target()` on every live Task, Block and
   routine `TaskTemplate` that names the stream. For a Task or Block this is a
   domain move per §2 (a `stream_id` `Patch` plus a stamp-preserving re-seal).
   For a routine template it is a plain `Patch` of the template's `stream_id`
   in `Meta`, with no re-seal;
2. writes `parent_id = null` on every child Stream, which become top-level;
3. tombstones the Stream, and records the disposition on the tombstone as
   `rehomed_to: StreamId?` (absent for `Detach`).

A reader never observes a dangling reference, even for writes the command could
not see. A task created in the stream on another device *after* the delete, and
merged later, is resolved by a read-time rule:

```
effective_stream(ref) =
    ref is None                         → None
    stream(ref) is live                 → ref
    stream(ref) is a tombstone          → effective_stream(tombstone.rehomed_to)
    (a chain longer than 8, or a cycle) → None
```

Every query, every "which domain seals the next write" decision and routine
generation read `effective_stream`, never the raw register. The next write to
such a task persists the resolved value, so the late arrival converges to the
same state the command would have produced. `MoveTo` a stream that is itself
deleted concurrently falls through the same chain, and a cycle (two streams
each re-homed into the other on two devices) resolves to no stream on every
replica, because the rule is a pure function of the merged tombstones.

A detached task that has no time and no `triaged_at` satisfies the §3
predicate and so appears in the Inbox. That is intended: losing its stream is
exactly the moment it needs a new home, and nothing had to write a flag for it
to be asked.

The dependents are **re-homed, never deleted**. Blocks are not deleted with
their stream, and routines are not archived: the only thing a stream deletion
removes is the stream.

### 5. Migration from the fixed Inbox sentinel

- **Storage.** The migration rewrites `stream_id = INBOX_STREAM_BYTES` to
  `NULL` in `tasks`, `blocks` and `routines`, drops the `NOT NULL` and the
  `REFERENCES streams` constraints on those columns, and deletes the sentinel's
  synthetic row if one exists. It sets no `triaged_at`, so exactly the tasks
  that were in the Inbox before are in the Inbox after (subject to their time
  fields, which the Inbox never considered before; a planned Inbox task leaves
  the Inbox, which is the point).
- **Wire, reading.** A decoded `stream_id` equal to the sentinel bytes is read
  as `None`, forever. The sentinel can never name a real Stream, so this
  mapping is total and cannot misfire.
- **Wire, writing.** A build with this change writes an absent `stream_id` and
  declares the feature id `task.optional_stream` in `vault_requires`
  (ADR-0045). An older build keeps syncing, parks what it cannot decode, and
  refuses to write Tasks, Blocks and Routines until it is updated, because a
  write from it would reintroduce a required `stream_id`.
- **`INBOX_STREAM_BYTES`** survives as the private domain's id and as a
  migration input. No query selects on it.

## Alternatives considered

| Option | Why not |
|---|---|
| **Keep the sentinel Stream, add `triaged: bool`** | Leaves "no project" spelled as a Stream that is not one, so every stream list, picker, share dialog and count has to special-case it, as they do today. The special cases are the flakiness this ADR exists to remove. |
| **A fresh id for the private domain** | Cleaner bytes, and it forces re-sealing every Inbox op or carrying two domains forever. The ASCII in the old id is cosmetic; the continuity is not. |
| **Seal stream-less tasks under the vault-meta key** | No new domain at all, and it breaks the separation `inbox.rs` records: rotating meta would rotate personal content, and any future meta-scoped grant would carry the user's private tasks. |
| **Refuse to delete a non-empty stream** | Simple, and it only moves the problem: a task created on another device after the check still lands in a deleted stream. The read-time rule in §4 is needed whatever the command does. |
| **Delete a stream's dependents with it** | Destroys user data as a side effect of tidying a sidebar. Rejected on the no-loss invariant. |
| **Inbox = `stream_id IS NULL` only** | Makes the Inbox the permanent home of every personal task, which is the conflation this ADR removes. |

## Consequences

- `docs/02-domain/tasks.md`, `streams.md`, `time-blocks.md` and
  `routines-and-recurrence.md` describe `stream_id` as optional and link here.
- `streams.md` §Child-stream lifecycle on parent delete changes from "orphans"
  to "promoted to top level in the same command".
- A Block with no stream has no stream tint; clients render it in the neutral
  palette colour.
- Every query that filtered by stream now has a third answer (no stream), and
  planning views gain a "No stream" group. Reviews count stream-less work as
  its own bucket.
- `Query::Inbox` becomes the §3 predicate, and the synthetic Inbox row in
  `Query::StreamList` is replaced by a separate count on the Inbox view.
- The private domain rotates on every revocation. That was already true of the
  sentinel.
- The invariant "no live Task, Block or Routine resolves to a deleted stream"
  becomes a property test over random command sequences on two replicas.

## What would force revisiting this

1. **Sharing a subset of stream-less tasks.** The private domain is
   all-or-nothing by construction. A need to share personal tasks with someone
   means the user moves them into a Stream, and if that proves unworkable the
   domain model, not the view, has to change.
2. **More than one private domain per vault** (for example a per-device
   scratch space). The `KeyDomain` enum would grow a variant, and the
   single-id argument in §2 would need restating.
