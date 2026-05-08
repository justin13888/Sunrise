---
status: draft
---

# Compaction

Without compaction, the op log grows forever. Compaction trims ops that are no longer needed for sync or audit.

## Eligibility

An op is eligible for compaction when **all** of the following hold:

1. Its `applied_at` is set on every device known to the identity (acknowledgment received).
2. It is older than a retention window (default: 30 days for normal ops, 365 days for control ops like share grants/revokes).
3. The op is not a checkpoint or transition op needed for tamper-evidence anchors.

## How

Compaction folds a range of ops into a **snapshot op** for a Stream:

```
SnapshotOp = {
    kind: "snapshot",
    stream_id: …,
    covers: { device_id => max_seq },     // the range it replaces
    state: encrypted-CBOR( current materialized state of stream, at this checkpoint ),
    base_root: …,                          // the op-log root at this point
    sig: …,
}
```

A snapshot op is itself signed and encrypted under the Stream key. Devices that haven't yet processed the underlying ops can apply the snapshot directly and skip the predecessors.

After all known devices acknowledge the snapshot, the predecessor ops can be deleted from local storage and the server's log.

## Who initiates

Any device with full visibility can propose a snapshot. To avoid two devices producing slightly different snapshots in the same window, we elect a "compactor" device deterministically (lowest device_id alive in the last 7 days).

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

## User-visible effect

None in the normal case. UI shows "vault size" and a manual "rebuild from history" button (which is mostly diagnostic).

## Test surface

- Round-trip test: produce a stream, generate snapshot, simulate fresh device joining, verify state matches.
- Adversarial test: reorder ops the server delivers; verify devices detect via root mismatch.
