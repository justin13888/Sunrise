---
status: accepted
---

# Compaction

A client folds the old part of its op log away once every device that still
needs it has acknowledged it, and a device that missed the folded range
catches up from a signed snapshot of the stream's state instead.
[ADR-0059](../11-adr/0059-client-op-log-compaction.md) is the decision record;
this document is the design of record for what ships.

The code is `crates/sunrise-core/src/engine/compaction.rs` (the floor and the
fold), `crates/sunrise-core/src/engine/snapshot.rs` (the record) and
`crates/sunrise-core/src/engine/merge/snapshot.rs` (`doc_state`). The tables
are migration `0035_op_log_compaction.sql`.

## The floor

Each `(stream, device)` prefix may have a floor (`compaction_floor`): every op
of that device in that stream at or below `seq` is **covered**. Its effect is in
the per-field merge state ([ADR-0044](../11-adr/0044-per-field-ops.md)), or its
row is still held, and the floor's chain root
([ADR-0043](../11-adr/0043-commit-tree.md)) commits to the whole prefix under
it. The floor keeps:

| Column | Meaning |
|---|---|
| `seq` | every op at or below it is covered |
| `op_hash` | the `op_hash` of the op at `seq`, which the next op names in its `prev_hash` |
| `root` | `root(device, seq)` |
| `hlc_ms`, `hlc_logical` | the stamp of the op at `seq`, which bounds every op the floor covers |

A floor never falls. Wherever the op log read the op at a position, it reads
the floor once the row is gone:

- the contiguous prefix, and so `sync_cursors`, starts above the floor;
- the chain root resumes from the floor's root;
- a `prev_hash` or a head naming a covered position is not recorded as a
  missing op;
- a delivery at a covered position is a duplicate and is not applied again. A
  different op at the floor's own seq is fork evidence, kind `seq`;
- a device never writes a `seq` at or below its own floor;
- `Engine::prime_hlc` restores the clock from the floors as well as from
  `ops`.

This is how the no-gap rule of
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Identity and replay invariants holds with a floor: `seq` is contiguous above
it, and the root at the floor stands for everything below. A peer that holds a
different op below a floor disagrees with the floor's root, which the stream
digest compares.

## Eligibility

`Engine::compact_op_log` raises each floor to the highest seq that is, for its
device in its stream, all of:

1. acknowledged by every known device of the stream (below);
2. older than the retention window: the op's stamp is at least
   `retention_ms` old;
3. strictly below this replica's own tip of that prefix, which is kept because
   the next op links to it and `prime_hlc` reads it.

It then deletes the ops at or below the floor whose whole effect is in the
merge state: every op, full-state or `Patch`, of an entity the registry merges
field by field into a row (`Merge::Lww` with a table), and old `StreamDigest`
ops. It never deletes:

- an op parked for a kind this build does not know (ADR-0045 §4);
- a control op: key envelopes, device certs, revocations, identity
  transitions;
- a focus or review record, whose effect is an append-only row;
- an op still waiting in the outbox;
- an op that a missing op's expectation was named by (`chain_expected`).

Before it deletes, it folds any entity row the merge had not folded yet,
because that fold reads the op's stream and kind back from the log (by the
`ops_by_target` index migration 0035 adds). This build folds every local op
as it writes it, so such a row comes only from a vault older than migration
0033 or from a build before ADR-0065's third pull request, which wrote a
command's row directly.

A defer is compacted like any other task edit. It is logged as the
`task.update` it is, and a `task.defer` row an older build logged for its own
defers is a compactable kind too.

A run that deleted any row ends with an incremental vacuum, which returns the
pages the deleted rows held to the filesystem, so the file shrinks
([`local-database.md`](./local-database.md) §Vacuum / maintenance).

### Acknowledgement and known devices

A device acknowledges a prefix by publishing a stream digest whose frontier
reaches it. A digest that checks out is kept in `peer_frontiers` as the peer's
acknowledgement of every prefix it names; a later digest only raises it. This
replica's own acknowledgement is its sync cursor.

A device is **known** in a stream when it is not revoked, was heard from within
`known_device_window_ms` (its newest tip, floor or digest), and is either a
member of the account or has written in the stream. This replica is always
known. A device silent for longer than the window, or never heard from, stops
holding compaction back, and catches up from a snapshot when it returns. A
revoked device never holds it back.

### Policy

| Field | Default | |
|---|---|---|
| `retention_ms` | 30 days | the relay's own retention window |
| `known_device_window_ms` | 30 days | |
| `snapshot_every_ms` | 1 day | how often the compactor rewrites a stream's snapshot |

`Core::set_compaction_policy` replaces it. The sync driver's anti-entropy tick
runs compaction at most once a day; `Core::compact_op_log` runs it now.

## Pruning version rows

A fold deletes ops, not the version DAG they left
([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) §1–§2): one
`entity_versions` row per version and one `entity_frontier` row per device it
reaches. Once every stream is folded, `Engine::compact_op_log` prunes the rows
no version can still read (clarification D25,
`crates/sunrise-core/src/engine/merge/prune.rs`). A version is **passed**, and
its rows go, when all of:

1. **compacted:** its op is at or below its prefix's floor and gone from the
   log, so it is never delivered again or refolded by a rebuild;
2. **not read:** it is not a head, not its device's newest version of the
   entity (the next version's previous one, D2), not a register's winner or
   loser, and not the newest `Patch` that wrote `streak_keys` or
   `skipped_keys` (what a routine's derived streak reads, D17);
3. **its readers went with it:** every version that names it as a parent,
   and its device's next version, is in its stream and compacted, so no
   rebuild and no replica joining the stream's snapshot folds one again;
4. **the entity is closed:** every parent a version of the entity names is a
   version here or is at or below its floor, so no ancestor is still to come;
5. **every device that can still write has passed it:** each device in
   `devices` other than this one that is not revoked, or was revoked less
   than `retention_ms` ago, has acknowledged a version that descends from it.
   It wrote that version, or published a frontier in its stream that reaches
   it, and this replica holds that device's own prefix in the stream as far
   as that acknowledgement states. A device with an op held here unapplied
   (waiting for its key, parked, or held in a group) passes nothing.

So a version applied afterwards names no pruned version, has no pruned
previous version and descends from no pruned ancestor: every frontier it
reads is still stored, and is the one a replica that pruned nothing reads.
The rows, registers, losers, heads and conflicts are the same, and so is a
rebuild.

Unlike the op-log floor, a device silent for any time holds pruning back,
because its unsent versions may name anything it held. Revoking it releases
it once the relay no longer holds what it uploaded. The rule cannot see a
device paired elsewhere whose cert has not arrived, an op sealed in another
stream before the acknowledgement it read and never received here, or a
writer naming a parent that was not its head. A version of those that names
a pruned parent folds it as a parent never applied: the parent adds nothing
to its frontier, so its writes stay in the unresolved set beside the ones it
had seen there, until the next write naming the heads. No write is lost.

After a fold an entity keeps its heads, each device's newest version, its
registers' winners and losers, and the versions not yet folded:
`crates/sunrise-core/src/engine/tests/prune.rs` holds an entity to that bound
however many edits it took. `CompactionReport::versions_pruned` counts the
rows, and `core.compaction.done` logs it as `n_versions`.

## Who writes the snapshot (compactor election)

The known device with the least `device_id`, in byte order, is the stream's
compactor. Election is recomputed on every run, so a compactor that goes silent
is replaced once the window has passed. Only the compactor writes the stream's
snapshot, at most once per `snapshot_every_ms`, and keeps the latest in
`stream_snapshots`. A snapshot is not needed for a replica to fold its own log:
its merge state already holds every op it deletes.

## Snapshot record

A snapshot covers one stream at a stated causal frontier.

```text
record = "SR" 0x04 0x0001 || canonical CBOR map
```

| Key | Field | Type |
|---|---|---|
| 1 | `stream_id` | `bstr .size 16` |
| 2 | `epoch` | `uint`, the stream-key epoch the body is sealed under |
| 3 | `generated_by` | `bstr .size 16`, the writer's device id |
| 4 | `generated_at_ms` | `uint`, the writer's wall clock |
| 5 | `frontier` | `[* [device: bstr .size 16, seq: uint, root: bstr .size 32, op_hash: bstr .size 32, hlc_ms: uint, hlc_logical: uint]]`, sorted by device |
| 6 | `digest` | `bstr .size 32`, ADR-0043's `stream_digest` over `(device, seq, root)` |
| 7 | `cert` | `bstr`, the writer's identity-signed `DeviceCert` |
| 8 | `nonce` | `bstr .size 24` |
| 9 | `body` | `bstr`, XChaCha20-Poly1305 of the canonical CBOR body |
| 10 | `sig` | `bstr .size 64`, Ed25519 under the writer's device key |

- The body key is `BLAKE3-KDF("sunrise.snapshot.key.v1", stream_key)` for the
  stream key at `epoch`.
- The AAD is `"sunrise.snapshot.aad.v1" || magic || canonical CBOR of fields
  1 to 8`.
- The signature covers `"sunrise.snapshot.sig.v1" || magic || canonical CBOR of
  fields 1 to 9`.

The body is `{1: doc_state, 2: [* retained envelope]}`.

### `doc_state`

The canonical per-field merge state the stream's ops wrote. For each entity: its
merge bookkeeping, and every register, map entry, OR-set add, OR-set remove and
counter delta whose stamp (an add's and a remove's tag) names the stream. Each
entity lists its distinct stamps once and its rows name them by index.

```cddl
doc-state = { "v": 2, "entities": [* entity],        ; sorted by id
              ? "members": [* member] }              ; in stamp order
member = [group: bstr .size 16, size: uint, stamp]
entity = {
  "id": bstr .size 16, "kind": tstr,                  ; registry tag
  "stamps": [* stamp],                                ; distinct, ascending
  "created": bool, "create": s / null, "legacy": s / null,
  "patch_ms": uint / null, "head": s,
  "registers": [* [field, bstr / null, origin, s]],   ; each register's winner
  "losers":    [* [field, bstr / null, origin, s]],   ; the rest of its antichain
  "maps":      [* [field, key, bstr, origin, s]],
  "adds":      [* [field, bstr, s]],
  "removes":   [* [field, bstr, bstr .size 16, bstr .size 16, uint]],
  "deltas":    [* [field, int, s]],
  "versions":  [* version],                           ; in stamp order
  "frontier":  [* [v, s]],                            ; F(versions[v]) reaches s
}
version = [s, kind: "legacy" / "patch", origin, create: bool,
           parents: bstr / null, group: [bstr .size 16, uint] / null,
           fields: bstr, head: bool]
s = uint                                              ; index into "stamps"
v = uint                                              ; index into "versions"
stamp = [hlc_ms, hlc_logical, device: bstr .size 16, seq, stream: bstr .size 16]
```

Values are the canonical CBOR bytes the merge tables hold. Version 2 adds the
register losers, the versions (ADR-0065 §1) and their causal frontiers (§2),
so a version above the snapshot whose parents it compacted away still orders
against them, and a conflict outlives the ops that caused it (§8). A version-1
document is still joined: its registers join as writes that reach only
themselves.

`versions` carries the versions the writer still holds; one it pruned is read
by no version a joiner can still fold. A pruned version can still have been a
group member (ADR-0065 §6), and a joiner holding the rest of its group needs
it present, so `members` names every group member of the stream whose version
the writer pruned (D14, D25). The key is absent when there is none, and a
build that does not read it ignores it.

### Retained envelopes

Every op of the stream at or below the frontier whose effect is not in
`doc_state`, verbatim and in stamp order: control ops, focus and review
records, parked ops, and the members of a group still waiting for the rest of
it (ADR-0065 §6), which compaction never deletes either. A writer refuses to write a snapshot while it holds an
op above its device's contiguous prefix, since no frontier could state it.

## Applying a snapshot

`Engine::apply_snapshot` (and `Core::apply_stream_snapshot`) checks, in order:

1. the magic prefix and the format version;
2. the signature, under the writer's cert as this vault holds it, or under the
   carried cert once it verifies under an identity on this account's chain;
3. that the writer is not revoked;
4. that the frontier is sorted and the digest is the digest of it;
5. that the body opens under a key this replica holds at `epoch`;
6. that every frontier entry this replica's own prefix reaches agrees with its
   own chain root there.

A record that fails 1 to 4 or 6 is refused. With no key at `epoch` the outcome
is `NoKey`, and with every entry already reached it is `Covered`; neither
writes anything. Otherwise the retained envelopes go through the ordinary
receive path, each verified on its own. If one is still waiting for a key, the
outcome is `Pending` and no floor rises. If one's writer has no cert here yet,
the apply fails with `UnknownDevice` and no floor rises either: certs travel in
the vault-meta stream, so a device that holds none applies that stream's
snapshot first. Then `doc_state` is **joined** into
the local merge state, every entity it names is re-projected, and each frontier
device's floor rises to its entry.

The join folds each row in through the write an op carrying it would have
used. Versions join first, keyed by op-ref; each frontier entry keeps the
greater of the held and the joined stamp, and the entity's frontiers and heads
are recomputed with the joined ones as their floor. Registers and losers then
join through the register fold, which reads the order through them. A version
of a group, and an entry of `members`, records its member as present, so a
member this replica holds back completes once the snapshot brings the rest. Every such write is idempotent and order-independent, so a replica that
joins a snapshot and then applies the ops above its frontier holds what a
replica that replayed every op holds, whatever it held before. Applying a
record twice, or one older than the replica, changes nothing.

A snapshot's state is attested by its writer's signature and nothing finer: the
ops it folded are gone. That is the trust compaction trades for bounded
storage, and the reason a revoked writer's snapshot is refused.

## Catch-up

A device that joins an account, or returns after the relay has trimmed what it
missed, applies the stream's latest snapshot and then the ops above its
frontier, which the relay replays from the cursor the snapshot set. A device's
own old ops can be folded too: its floor covers them, it never reuses their
seqs, and no receiver treats the hole as a gap.

### Transport

[ADR-0068](../11-adr/0068-a-streams-snapshot-travels-as-a-pinned-blob.md) is
the decision record; the code is `crates/sunrise-core/src/snapshot_sync.rs`
(both legs), `crates/sunrise-server/src/api/snapshots.rs` (the pin) and
`crates/sunrise-server/src/store/snapshots.rs` (relay migration 0006).

**Publish.** A record is a blob, not a frame: past about five thousand tasks it
outgrows the 4 MiB frame cap. The stream's compactor uploads its latest record
through the blob routes, in chunks of at most 1 MiB, then pins it with
`PUT /api/v1/snapshots/{stream_id}` ([`../06-server/api.md`](../06-server/api.md)
§Stream snapshots). A replica publishes only a record it wrote, and only while
it is the stream's compactor, so a device the election has moved past never
pins its older record over its successor's. The driver publishes before a
session reports `Live` and on every resync tick, after compaction; what it
published is remembered per process, and the first publish after a restart asks
the relay what it pins before uploading.

**What the relay keeps.** One record per stream, the last one pinned: a new pin
deletes the blob the old one named. Pins are not op retention's, so a record
outlives every op it covers, and they go only with the account. The relay
reads one thing from the record, its cleartext magic prefix, and refuses to
pin a blob of any other kind.

**Recover.** The relay's gap event names its stream. The driver asks for the
stream's pin, fetches the blob, checks it hashes to the pinned id, and applies
it (§Applying a snapshot), which raises the stream's floors and cursors to the
record's frontier. It then re-subscribes every stream from its cursors, before
reading another frame. A record that cannot be applied yet (`NoKey`,
`Pending`, a writer whose cert has not arrived) is retried after any other
record applies and on the next gap; one that is missing, adds nothing or is
refused leaves the session `Degraded`, as does a second gap on a stream whose
record was applied. A new device needs nothing extra: its empty cursors draw a
gap on every stream whose early ops were trimmed.

**What it does not cover.** A record is as fresh as the compactor's cadence,
one day. Under the relay's age bound that is always enough; under its size
bound a stream busy enough to evict 256 MiB in a day can outrun its record. A
returning device that is itself the compactor finds only its own last record.
Both leave the device `Degraded`, as before.

## What stays forever

Nothing that is not an entity write: device certs, key envelopes,
revocations, identity transitions, focus and review records, and parked ops.
The version rows a device that can still write has not passed (§Pruning
version rows).
Each device's tip. The latest snapshot per stream, on its compactor, on any
replica that applied it, and pinned on the relay.

## Server side

The relay does not compact and cannot: it never reads an op. It pins each
stream's latest snapshot record (§Transport). Its op retention is
unchanged, 30 days or 256 MiB per channel
(`crates/sunrise-server/src/relay_log.rs#DEFAULT_MAX_AGE_MS`,
`crates/sunrise-server/src/relay_log.rs#DEFAULT_MAX_BYTES`). Compaction makes
that bound survivable: a device beyond it catches up from a snapshot rather
than from ops nobody holds.

## User-visible effect

None in the normal case. An entity's activity feed and the review trends read
the op log, so they reach back as far as the retention window and no further.

## Test surface

- `crates/sunrise-core/src/engine/tests/compaction.rs`: a missing
  acknowledgement holds a fold back and a digest releases it; the retention
  window; a silent or revoked device is not waited for; a replica writes on
  above its floor; a re-delivery below a floor is a duplicate and a forged op
  at it is fork evidence; what compaction never deletes; the compactor
  election; a device bootstrapped from a snapshot and its tail equals one that
  replayed every op, in projection, cursors and digest; a snapshot written
  after a fold; parked ops carried by a snapshot; a covered snapshot; a
  forged, foreign or divergent one is refused; the clock primed from the
  floors.
- `crates/sunrise-core/src/engine/tests/prune.rs`: each condition of the
  pruning rule on hand-built versions; a property that devices editing,
  syncing, acknowledging and compacting in any order hold what a replica that
  never pruned holds; the bound on an entity's rows; a silent device holding
  pruning back; the documented fold of a late version naming a pruned parent;
  a rebuild after pruning; a joiner of a pruned replica's snapshot; and the
  group members a snapshot names present.
- `crates/sunrise-bench/benches/compaction.rs`: the op log at 10k and 100k
  tasks before and after a fold, the snapshot record that replaces it, and
  what both cost; and a report of the version rows an entity keeps against
  the versions it took and the devices that acknowledge them, held by a
  silent device and pruned once every device has passed them, with what the
  fold and a rebuild cost either way.
