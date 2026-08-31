---
status: accepted
---

# Compaction

> **This document describes target state, not v1.**
> Nothing here is implemented: there is no snapshot op (`InnerOp` has no
> such variant), no compactor election, and no retention sweep — the op log
> currently grows without bound. It also predates
> [ADR-0014](../11-adr/0014-entity-level-lww-merge.md), which replaced CRDT
> merge with entity-level LWW and removed the `loro` dependency, so the
> snapshot shape below needs redesigning before it can be built. See
> [`../implementation/overview.md`](../implementation/overview.md) for what is
> actually live.

Without compaction, the op log grows forever. Compaction trims ops that are no longer needed for sync or audit.

## Eligibility

An op is eligible for compaction when **all** of the following hold:

1. Its `applied_at` is set on every "known device" of the identity (see below).
2. It is older than a retention window (default: 30 days for normal ops, 365 days for control ops like share grants/revokes).
3. The op is not a checkpoint or transition op needed for tamper-evidence anchors.

### "Known device"

A "known device" is a device with an entry in `vault_meta.devices` that is **not** revoked AND has emitted a cursor op in the last 30 days. A revoked device is excluded immediately (no grace delay). A device silent > 30 days is excluded; if it returns, it must catch up via snapshot rather than blocking compaction.

## How

Compaction folds a range of ops into a **snapshot op** for a Stream. The snapshot's CBOR shape:

```cddl
; PROPOSED - not implemented, and not a wire format. See the banner above.
Snapshot = {
    v:                uint,                ; snapshot format version (1)
    stream_id:        bstr .size 16,
    upto_op_seq:      uint,
    generated_at_ms:  uint,
    doc_state:        bstr,                ; serialized Stream state, format TBD
    head_root:        bstr .size 32,
    participants:     [+ {device_id: bstr .size 16, last_op_seq: uint}],
    hash:             bstr .size 32,       ; BLAKE3(canonical CBOR of the above fields, 32)
}
```

`doc_state` **is undecided.** An earlier revision specified
`loro::Doc::export_snapshot()` bytes, which cannot be produced: the workspace
ships no CRDT library, and under ADR-0014 a Stream's state is rows in SQLite
rather than a mergeable document. Whatever replaces it has to be a canonical
serialization of the materialized entity rows plus their LWW stamps, since
those stamps are what makes a later op's merge deterministic — but that is a
design decision, not a settled one, and is the main reason this document is
not buildable as written. The encrypted-CBOR envelope would wrap the whole
structure, signed under the Stream key.

Validation on application:

1. Verify magic + version.
2. Recompute `hash` over the canonical CBOR of all other fields; reject on mismatch.
3. Verify `head_root` matches a re-derivation from the imported state.
4. Replace local Stream state with snapshot.

Devices that haven't yet processed the underlying ops can apply the snapshot directly and skip the predecessors. After all known devices acknowledge the snapshot, the predecessor ops can be deleted from local storage and the server's log.

## Who initiates (compactor election)

- Eligible compactor: the device with the smallest `device_id` (lex byte order) among all "known devices" at the moment compaction conditions become true.
- Re-elected per-Stream per-day. Election re-runs at the next compaction-eligibility check if the previous compactor went silent.
- No tie-breaker needed (`device_id`s are 16 random bytes; collision negligible).

## What about devices we haven't heard from?

A device that's been offline for >retention has missed compactions; it must catch up via:

1. Apply the most recent snapshot (which the server still holds).
2. Apply ops since the snapshot.

If a device has been offline so long that *its own old ops* have been compacted out (and the server has discarded them) — that's fine; the device's local copy still exists, and as long as no peer replays them, this is consistent.

## What stays forever

- Identity creation events.
- DeviceCert events for currently-active devices.
- Stream creation events.
- Share grants and revocations.
- The most recent snapshot per Stream.

These are the audit-relevant anchors; they are small and worth keeping.

## Server-side compaction

The server runs a parallel compaction:

1. Sees devices report their `last_seq` per Stream and per device.
2. When all devices ≥ a threshold, drops the underlying ciphertext from blob storage.
3. Retains the snapshot op until superseded.

The server does not reorder, rewrite, or merge ops; it only deletes ops it no longer needs to retain.

### Server-side retention after compaction

After compaction, the server retains the original ciphertext blobs for **30 days** under a `compacted/` prefix before deletion. This window allows:

- Late-arriving devices to catch up via the original op stream (faster than snapshot in some cases).
- Forensic recovery if a snapshot is determined to be defective.

After 30 days, the original blobs are hard-deleted; the snapshot is the only authoritative state.

## User-visible effect

None in the normal case. UI shows "vault size" and a manual "rebuild from history" button (which is mostly diagnostic).

## Test surface

- Round-trip test: produce a stream, generate snapshot, simulate fresh device joining, verify state matches.
- Adversarial test: reorder ops the server delivers; verify devices detect via root mismatch.
