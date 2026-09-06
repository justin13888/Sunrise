---
status: accepted
---

# Audit and Tamper Evidence

The relay is untrusted but is in the message path. We need to detect: dropped ops, replayed ops, reordered ops, and fork attacks (different views of history served to different devices).

## Implementation status: two hash functions, and nothing else

**Everything in this document is a target.** What exists in `crates/` is `sunrise-crypto/src/merkle.rs`: `stream_root_init` and `stream_root_step`, byte-exact to the formulas under §Per-Stream Merkle root and covered by frozen vectors in `crates/sunrise-crypto/tests/frozen_vectors.rs`. **Those tests are their only callers.** Nothing in the engine, the sync layer, or either client folds an applied op into a root.

Concretely, none of the following exists:

* **No root is persisted.** There is no column, no table, and no "highest-seen root per Stream", so §Rollback detection has nothing to compare against on reconnect.
* **No checkpoint op.** Of the 24 `InnerOp` variants (`crates/sunrise-core/src/inner_op.rs`), 21 are domain CRUD and the three [ADR-0024](../11-adr/0024-key-hierarchy.md) added carry keys and trust (`key_envelope`, `device_revoke`, `device_cert`). `CheckpointPayload` has no encoder, and the 256-op / 24 h emission rule has no timer.
* **`server_first_seen_ms` feeds no ordering rule.** The annotation does exist, but only per batch and only as advice: `Ack.server_first_seen_ms` (`crates/sunrise-wire-protocol/src/payloads.rs:93`) is stamped at `crates/sunrise-server/src/api/sync.rs:419` and parsed back onto the synthesized `Ack` frame at `crates/sunrise-sync/src/sse.rs:435`. Nothing persists it and nothing orders by it. Per *op* it does not exist at all — the relay stores `relay_frames(account_h, stream_id, bytes, n_bytes, created_ms)`, parses only `EnvelopeHeader` (`{stream_id, device_id, seq}`) for routing, and emits no unsigned addendum. Since the amendment below removes the clamp, no ordering rule wants one.
* **No fork or rollback detection, and no integrity indicator.** §Verifying checkpoints from peers, §Fork detection and §Per-vault Integrity indicator describe no code and no UI.

What *is* enforced today is the replay invariant in §Identity and replay invariants: `0013_baseline.sql` declares `UNIQUE (stream_id, device_id, seq)` on `ops`, so `Engine::apply_remote` drops a re-delivered op idempotently, and `sync_cursors.last_applied_seq` is the per-`(stream_id, device_id)` high-water mark. Tampering with any single stored op is caught by the `OpEnvelope` AEAD. Detecting *omission and reordering across* ops — which is what the rest of this document is for — is not built.

## Identity and replay invariants

The op envelope's `(stream_id, device_id, seq)` triple is the canonical replay-detection key. The inner-Op `op_id` (a ULID) is the canonical merge identity used for idempotent application — duplicate `op_id` arrivals are dropped silently.

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

Concurrent ops apply in `(hlc, device_id, seq)` order before being folded into
the root — the hybrid logical clock, then the raw 16-byte device id (memcmp,
higher wins), then the writer's per-`(stream, device)` sequence number. This is
byte-identical to the entity-level LWW comparison key
([ADR-0016](../11-adr/0016-hlc-timestamps.md),
[`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md) §The comparison key,
`crates/sunrise-storage/migrations/0013_baseline.sql:97-99`), which is the point:
one ordering key for the whole system, and no second one to keep in agreement
with it.

> **Amended ([ADR-0027](../11-adr/0027-v1-self-host-first.md)).** This section
> previously folded concurrent ops in `(hlc_clamped, device_id_lex, seq)`, where
> `hlc_clamped` clamped the signed HLC to ±5 min around the relay's
> `server_first_seen_ms`. The clamp is removed.
>
> **Why:** the clamp gave the relay an input into the ordering of the one
> structure whose whole purpose is detecting what the relay did. An adversary who
> can shift `server_first_seen_ms` can shift the fold and therefore the root,
> which makes a divergent root deniable — the failure the Merkle root exists to
> make undeniable.
>
> **What this gives up:** a device with a badly wrong clock can now push an op
> far up or down the fold order. That was already the honest state. The clamp
> bounded the *ordering* effect without bounding *acceptance*, and a receiver
> refuses an op more than `MAX_DRIFT_MS` out anyway
> ([`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md)).
> The skew warning below stays.

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
