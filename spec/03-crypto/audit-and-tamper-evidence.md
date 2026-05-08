---
status: draft
---

# Audit and Tamper Evidence

The server is untrusted but in the message path. We need to detect: dropped ops, replayed ops, reordered ops, fork attacks (different views of history served to different devices).

## Per-device monotonic counters

Every op carries `(device_id, seq)` where `seq` is strictly monotonic within a device for a given Stream. Receiving devices verify:

- `seq` increases by 1 (no gaps after compaction).
- A gap implies the server withheld an op or it was lost. Gap detection produces a sync warning.

## Per-stream Merkle hash chain

For each Stream, devices maintain a running **op-log root**:

```
root_n = BLAKE3( root_{n-1} || op_id_n || env_hash_n )
```

`env_hash_n` = BLAKE3 of the full op envelope.

- Each device publishes its current root in periodic "checkpoint ops."
- Other devices verify checkpoints they receive against their own computed root.
- A mismatch indicates: malicious server (different views), out-of-order delivery, or a bug. The client surfaces a "vault integrity warning."

Note: this is *eventual* tamper evidence, not real-time prevention. CRDTs allow concurrent ops, so two checkpoint roots can legitimately differ at a given moment. The check is: "for the set of ops both devices have seen, does the merge-equivalent root match?"

## Fork detection (server view divergence)

A hostile server could serve different op sets to different devices. To detect:

1. Each device, on every reconnect, sends its **highest seen `(device_id, seq)` pairs** for each Stream.
2. The server's response includes ops since those points.
3. The server's response is signed (transport-level via account auth) but content-trusted only via the per-op signatures.
4. If the server omits ops it has previously delivered, the next sync from the originating device will surface the gap (because the originating device's checkpoint will reference ops the receiving device never saw).

This is not perfect; a sufficiently elaborate fork can hide for a long time if the user's devices never reconnect to the same canonical view. We accept this and document the limitation.

## Rollback detection

A simpler attack: server serves a known-old snapshot to one device after the device was offline.

Mitigation: each device persists its **highest-seen op-log root** per Stream. On reconnect, it compares the server's first delivery against its stored root; if the server's chain doesn't extend the stored root, the device refuses to apply and surfaces a "rollback detected" warning.

## Replay protection

`(device_id, seq, op_id)` make replays detectable: the same op appearing twice is identical and is idempotent (the CRDT layer drops duplicates by `op_id`). However, the *act* of replaying old ops is itself a sign of either bug or malice; we log a counter.

## What the user sees

- A per-vault "Integrity" indicator (green / yellow / red) in advanced settings.
- On `red`, a clear instruction: stop syncing, contact support / cross-verify with another device.

## What we explicitly don't do in v1

- Transparency logs (Certificate-Transparency-like public log of identity-key changes). Tracked for v2 once federation is in scope.
- Multi-party verification of server integrity (out of scope).
