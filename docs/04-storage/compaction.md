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

Before it deletes, it folds any entity row a local command wrote and the merge
had not folded yet, because that fold reads the op's stream and kind back from
the log (by the `ops_by_target` index migration 0035 adds).

The pages the deleted rows held go on SQLite's free list and are reused by
later writes. The file does not shrink, because the vault's `auto_vacuum`
pragma does not take effect ([#461](https://github.com/justin13888/Sunrise/issues/461)).

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
doc-state = { "v": 1, "entities": [* entity] }       ; sorted by id
entity = {
  "id": bstr .size 16, "kind": tstr,                  ; registry tag
  "stamps": [* stamp],                                ; distinct, ascending
  "created": bool, "create": s / null, "legacy": s / null,
  "patch_ms": uint / null, "head": s,
  "registers": [* [field, bstr / null, origin, s]],
  "maps":      [* [field, key, bstr, origin, s]],
  "adds":      [* [field, bstr, s]],
  "removes":   [* [field, bstr, bstr .size 16, bstr .size 16, uint]],
  "deltas":    [* [field, int, s]],
}
s = uint                                              ; index into "stamps"
stamp = [hlc_ms, hlc_logical, device: bstr .size 16, seq, stream: bstr .size 16]
```

Values are the canonical CBOR bytes the merge tables hold.

### Retained envelopes

Every op of the stream at or below the frontier whose effect is not in
`doc_state`, verbatim and in stamp order: control ops, focus and review
records, and parked ops. A writer refuses to write a snapshot while it holds an
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
outcome is `Pending` and no floor rises. Then `doc_state` is **joined** into
the local merge state, every entity it names is re-projected, and each frontier
device's floor rises to its entry.

The join folds each row in through the write an op carrying it would have
used. Every such write is idempotent and order-independent, so a replica that
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

**Not built yet: transport.** Nothing carries a snapshot record between devices.
`Core::stream_snapshot` hands out the stored record and
`Core::apply_stream_snapshot` takes one in, but neither the relay, the blob
store nor pairing moves it. That is
[#462](https://github.com/justin13888/Sunrise/issues/462).

## What stays forever

Nothing that is not an entity write: device certs, key envelopes,
revocations, identity transitions, focus and review records, and parked ops.
Each device's tip. The latest snapshot per stream, on its compactor and on any
replica that applied it.

## Server side

The relay does not compact and cannot: it never reads an op. Its retention is
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
- `crates/sunrise-bench/benches/compaction.rs`: the op log at 10k and 100k
  tasks before and after a fold, the snapshot record that replaces it, and
  what both cost.
