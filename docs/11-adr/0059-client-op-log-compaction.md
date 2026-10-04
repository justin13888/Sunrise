# 0059 — The client op log is folded below an acknowledged floor, and a stream is bootstrapped from a signed snapshot of its merge state

**Status:** accepted

**Built by** [#330](https://github.com/justin13888/Sunrise/issues/330).

**Depends on** [ADR-0043](./0043-commit-tree.md) (per-device chains, the
stream digest), [ADR-0044](./0044-per-field-ops.md) (per-field merge state) and
[ADR-0045](./0045-schema-identity-and-feature-gating.md) §4 (parked ops).
**Resolves** the two blockers
[`../04-storage/compaction.md`](../04-storage/compaction.md) named while it was
`proposed`, and answers ADR-0043's resolved question 8.

## Context

Every op a device received stayed in its local `ops` table forever. The relay
trims its own copy at 30 days or 256 MiB per channel
(`crates/sunrise-server/src/relay_log.rs#DEFAULT_MAX_AGE_MS`), so a device
offline for longer than that, or a new device joining an old account, could
catch up only from what the relay still held.

The compaction document could not be built as written, for two reasons. Its
snapshot's `doc_state` was specified as Loro bytes, which the workspace cannot
produce. And its catch-up rule let a device's own old ops disappear, which
contradicts the rule that `seq` is contiguous per `(stream, device)`
([`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Identity and replay invariants) without saying what a receiver does with the
hole.

Three things that landed since make both answerable. Every field now merges by
its own CRDT type, so a replica's state is a join of per-field states rather
than rows (ADR-0044). Every device's ops form a hash chain whose running root
commits to the whole prefix, and replicas exchange those roots in a stream
digest (ADR-0043). And an op this build cannot read is parked rather than
dropped (ADR-0045 §4).

## Decision

### 1. `doc_state` is the per-field merge state a stream's ops wrote

A snapshot's state is the canonical encoding of the ADR-0044 tables: for each
entity, its merge bookkeeping, and every register, map entry, OR-set add, OR-set
remove and counter delta that an op sealed in the stream wrote. A row is
attributed by the stream in its own stamp, so a stream's snapshot carries
nothing another stream's ops wrote, and a stream whose key a reader lacks
reveals nothing to it. Each entity lists its distinct stamps once and its rows
name them by index. The encoding is in
`crates/sunrise-core/src/engine/merge/snapshot.rs`.

Applying one is a **join**, not a replacement. Every write in the merge state
is idempotent and order-independent: a register keeps the greatest stamp, an
add, a remove and a delta are keyed by the op that made them, and the legacy
floor only rises. So each row of the snapshot is folded in through exactly the
write an op carrying it would have used, and a replica that joins a snapshot
and then applies the ops above its frontier holds what a replica that replayed
every op holds, whatever it held before.

Rejected: rows of the entity tables, the shape the compaction document fell
back to. A row has lost what a later op's merge needs: per-field stamps, OR-set
tags, counter deltas. Rejected: shipping the folded ops themselves. That is a
log, not a snapshot, and it does not shrink.

### 2. A hole below a floor is covered by the floor's chain root

Each `(stream, device)` prefix may have a **floor**, in `compaction_floor`
(migration 0035): every op of that device at or below `seq` is covered, and the
floor keeps `root(device, seq)`, the `op_hash` of the op at `seq`, and that op's
stamp. A covered position is neither held nor missing. The op log reads the
floor wherever it read the op at a position:

- the contiguous prefix, and so the sync cursor, starts above the floor;
- the chain root resumes from the floor's root, and `root(d, seq)` and the op
  hash at `seq` read from the floor once the row is gone;
- a link or a head that names a covered position is not an expectation;
- a delivery at a covered position is a duplicate and is not applied again. A
  different op at the floor itself is fork evidence, because the hash there is
  known. Below it no single op's hash is, and a peer that holds a different op
  there disagrees with the floor's root, which the digest already compares;
- a device never takes a `seq` at or below its own floor;
- `Engine::prime_hlc` restores the clock from the floor stamps as well as from
  `ops`, because a floor's stamp bounds every op it covers.

So `seq` stays contiguous per `(stream, device)` above the floor, and the
chain root at the floor commits to everything under it. That is the
no-gap invariant restated for a log with a floor.

### 3. Compaction folds what every known device has acknowledged

`Engine::compact_op_log` raises each floor to the highest position that is
acknowledged by every **known** device of the stream, older than the retention
window, and strictly below this replica's own tip. It then deletes the ops under
the floor whose whole effect is in the merge state: every op of an entity the
registry merges field by field into a row, and old stream digests. It never
deletes an op that is parked, a control op (keys, certs, revocations, identity
transitions), a focus or review record, an op still waiting in the outbox, or
an op that a missing op's expectation was named by. Before it deletes, it folds
any entity row a local command wrote and the merge had not yet folded, because
that fold reads the op's stream and kind back from the log.

A device's **acknowledgement** of a prefix is the last frontier it published in
its stream digest, kept in `peer_frontiers`; this replica's own is its sync
cursor. A device is **known** in a stream when it is not revoked, was heard
from within the window, and is either a member of the account or has written in
the stream. A device silent for longer than the window, or never heard from,
stops holding compaction back and catches up from a snapshot when it returns. A
revoked device never holds it back.

The tip is kept because the next op of each device links to it and because it
is what `prime_hlc` reads.

`CompactionPolicy` holds the bounds: retention (30 days), the known-device
window (30 days) and how often the compactor rewrites its snapshot (one day).
The sync driver's anti-entropy tick runs compaction at most once a day, under
the policy a caller set with `Core::set_compaction_policy`.

Rejected: waiting until a snapshot covers the range before deleting it
locally, which the compaction document described. A replica's own merge state
already holds every op it would delete; a snapshot is for a device that does
not.

### 4. The snapshot record: magic kind 4, sealed and signed

A snapshot covers one stream at a stated causal frontier. The record is the
magic prefix `SR 0x04` with format version 1, then a canonical CBOR map:
the stream, the stream-key epoch, the writer's device id and clock, the
frontier, ADR-0043's stream digest of it, the writer's identity-signed cert, a
nonce, the sealed body, and the writer's Ed25519 signature over all of it. Each
frontier entry is `[device, seq, root, op_hash, hlc_ms, hlc_logical]`: the root
lets a reader check the frontier against its own chain, and the op hash is what
the first op above the frontier names in its `prev_hash`.

The body is the `doc_state` of §1 and every op of the stream at or below the
frontier whose effect is not in that state, verbatim: control ops, focus and
review records, and parked ops. It is sealed with XChaCha20-Poly1305 under a
key derived from the stream key, with the record's other fields as AAD. The
full layout is in [`../04-storage/compaction.md`](../04-storage/compaction.md)
§Snapshot record.

A reader checks the magic and version, the signature under the writer's cert
(its own copy, or the carried one once it verifies under an identity on this
account's chain), that the writer is not revoked, that the digest is the
digest of the frontier, that the body opens, and that every frontier entry its
own prefix reaches agrees with its own chain root. Then it applies the carried
ops through the ordinary receive path, joins the state, and raises each
frontier device's floor to its entry.

Rejected: Loro bytes, for the reason in §1. Rejected: a new op kind carrying
the snapshot. An op is bounded by the 4 MiB frame, takes a seq in the writer's
chain, and is trimmed by the relay with everything else; a record is none of
those.

### 5. The compactor is the known device with the least id

Only the stream's compactor writes its snapshot, at most once per
`snapshot_every_ms`, and keeps the latest in `stream_snapshots`. Election is
recomputed on every run from the known-device set, so a compactor that goes
silent is replaced once the window passes.

### 6. A snapshot's state is attested by its writer and nothing finer

The ops a snapshot folded are gone, so no single write in its state can be
re-verified; what a reader trusts is that a current member of the account
signed it. That is why a revoked writer's snapshot is refused, and it is the
trust compaction trades for bounded storage. The frontier is checkable: a
reader whose prefix reaches an entry compares roots, and a peer's later digest
compares the bootstrapped replica's roots with its own.

### 7. What this record does not decide

How a snapshot reaches a device that needs one. The record is written, stored
and applied by the core, and `Core::stream_snapshot` and
`Core::apply_stream_snapshot` hand it in and out, but no transport carries it
between devices yet: neither the relay, nor the blob store, nor pairing. That
is [#462](https://github.com/justin13888/Sunrise/issues/462). The relay's own
retention is unchanged.

ADR-0043's global-order fold (`stream_root_init`, `stream_root_step`) is not
the snapshot's commitment format: the frontier's per-device chain roots are.
The fold stays test-only and pinned.

## Consequences

- **Migration 0035** (`STORAGE_V` 35): `compaction_floor`, `peer_frontiers`
  and `stream_snapshots`, and an index `ops_by_target` on
  `ops (target_id, device_id, seq)`. Without it, folding every entity under a
  floor looked each one's op up by a full scan, which made a fold quadratic
  in the log.
- The pages a fold frees stay on SQLite's free list and are reused by later
  writes; the file does not shrink, because the vault's `auto_vacuum` pragma
  does not take effect ([#461](https://github.com/justin13888/Sunrise/issues/461)).
- No change to any wire format: the snapshot record is new, magic kind 4 was
  reserved for it, and no op or envelope field moves. `DOC_SCHEMA_V` is
  unchanged.
- An entity's activity feed (`Query::ActivityTimeline`) and the review trends
  read the op log, so they no longer reach back past the retention window. The
  feed already treats a history with no create as truncated rather than
  inventing a transition.
- A forked op below a floor is not caught as single-op evidence, only as a
  root disagreement in a digest.
- Storage on the compactor carries one snapshot per stream, about the size of
  the merge state that stream's ops wrote.
- `crates/sunrise-bench/benches/compaction.rs` measures the log at 10k and 100k
  tasks before and after a fold, and the snapshot that replaces it.

## Revisit if

- A transport carries snapshots between devices: the record format here is
  what it carries, and a relay that pins the newest snapshot per channel would
  need to tell one apart without reading it.
- An op kind other than an entity write comes to have its whole effect in the
  merge state: it becomes compactable by the registry rule, not by a list.
- A peer channel arrives (ADR-0043 resolved question 5): a replica could then
  be asked for ops below its floor, and the floor would have to say no.
