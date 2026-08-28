---
status: accepted
---

# Audit and Tamper Evidence

The relay is untrusted but is in the message path. We need to detect: dropped ops, replayed ops, reordered ops, and fork attacks (different views of history served to different devices).

## Identity and replay invariants

The op envelope's `(stream_id, device_id, seq)` triple is the canonical replay-detection key. The inner-Op `op_id` (a ULID) is the canonical CRDT-merge identity used for idempotent application — duplicate `op_id` arrivals are dropped silently.

Receivers enforce:

- `seq` strictly monotonic per `(stream_id, device_id)`, starting at 1, no gaps.
- A gap is a sync warning: surfaced as "received op #(N+2); op #(N+1) missing" with a "request resync" affordance.
- A repeated `(stream_id, device_id, seq)` whose envelope bytes don't match the previously stored one is treated as integrity-fatal: sync stops, the user sees an integrity warning.
- A repeated `(stream_id, device_id, seq)` whose bytes do match is silently dropped (idempotent re-delivery).

## Per-Stream Merkle root

For each Stream, every device maintains a running root computed in causal-order applied:

```
root_0     = BLAKE3("sunrise.stream_root.init.v1" || stream_id, 32)
root_n     = BLAKE3("sunrise.stream_root.step.v1" || root_{n-1} || env_hash_n, 32)
env_hash_n = BLAKE3(canonical_cbor_envelope_bytes_n, 32)
```

Concurrent ops apply in `(hlc_clamped, device_id_lex, seq)` lexicographic order before being folded into the root, where:

```
hlc_clamped = (clamp(envelope.hlc.physical_ms,
                     server_first_seen_ms - 5 * 60_000,
                     server_first_seen_ms + 5 * 60_000),
               envelope.hlc.logical)
```

The clamp window is the same 5 minutes as `MAX_DRIFT_MS` in
[`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md), and for the same reason: a reading further out than that is a broken clock, and a *client* refuses such an op outright. The clamp is the relay-side equivalent for a party that cannot refuse.

The relay timestamps every inbound op as `server_first_seen_ms` and includes it in the op's metadata as an unsigned addendum (see [`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md) §6). The annotation is part of every replicated op; relays MUST forward it unchanged. Two devices that have observed the same set of ops compute identical roots if both have observed the same `server_first_seen_ms` annotations.

Devices with > 5 min skew display a `"Your clock is ≥ 5 minutes off; sync may produce unexpected ordering"` warning.

## Checkpoint ops

Each device emits **one checkpoint per Stream per (256-op-window OR 24 h-elapsed), whichever comes first**. The 24 h timer resets on each emitted checkpoint.

- During a batch apply that crosses multiple thresholds, the device emits **exactly one** checkpoint at the end of the batch (debounced).
- "Immediately before disconnect": a device emits a final checkpoint on graceful shutdown if the most recent applied op is past the last checkpoint.
- Crash recovery re-emits the missed checkpoint on next startup.

The checkpoint payload (encrypted under the Stream key, like normal ops) is:

```cddl
CheckpointPayload = {
    1: bstr .size 32,            ; root after applying the most recent op
    2: { bstr .size 16 => uint } ; covers: the (device_id => max applied seq) snapshot at this root
}
```

## Verifying checkpoints from peers

When device A receives a checkpoint from device B for Stream S:

1. Look up A's local applied set restricted to the same `covers`.
2. Compute the root A would have for that restricted set.
3. If A's restricted-root equals B's `root`: confirmed; A advances its peer-trust state for B's view of S.
4. If they differ but A is missing some op B claims to have applied (i.e. B's `covers[d_i] > A's max applied seq for d_i`): A re-requests the missing ops and retries.
5. If they differ for a covers set A has fully applied: this is a **fork detected** — A surfaces an integrity-red warning, stops accepting more ops on S until the user inspects and acknowledges, and writes a forensic record (B's checkpoint envelope, A's restricted-root) to the vault.

## Rollback detection

A simpler attack: the relay serves a known-old snapshot to a device after that device was offline.

Mitigation: every device persists its **highest-seen `root` per Stream** durably. On reconnect:

1. Device sends its persisted `(root, covers)` to the relay.
2. Relay's reply is expected to either echo the same root (no new ops) or extend it (compute the new root over A's set ∪ new ops). If the relay's first delivery cannot extend A's stored root, A refuses to apply and surfaces a "rollback detected" warning.

## Fork detection (server view divergence)

A hostile relay could serve different op sets to different devices and avoid consistency forever, but in practice as devices reconnect to each other their checkpoints converge or diverge. The checkpoint mechanism above will eventually detect a fork once two formerly-divergent devices have a checkpoint comparison.

This is *eventual* tamper evidence, not real-time prevention. We accept the residual risk and document it.

## Per-vault Integrity indicator

A persistent UI element in advanced settings:

- **Green** — all checkpoints within the last 7 days verified across all paired devices.
- **Yellow** — gap detected once, recovered after resync.
- **Red** — fork or rollback detected; sync paused until the user acknowledges.

A `red` state offers a "save forensic bundle" affordance that exports the relevant envelopes, checkpoints, and roots in a sealed encrypted file the user can share with support or with peers for cross-verification.

## Out of scope (v1)

- **Transparency logs** (Certificate-Transparency-style public log of identity-key events). Tracked for v2 with federation.
- **Multi-party verification of relay integrity** (oblivious transfers, third-party auditor). Out of scope.
