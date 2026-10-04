---
status: accepted
---

# Audit and Tamper Evidence

The relay is untrusted but is in the message path. We need to detect: dropped ops, replayed ops, reordered ops, and fork attacks (different views of history served to different devices).

[ADR-0043](../11-adr/0043-commit-tree.md) is the design of record for detecting omission, reordering and forks. This document describes what it built and what is still a target.

## Implementation status

Built ([#325](https://github.com/justin13888/Sunrise/issues/325)):

* **Per-device hash chains.** Every op a current build writes carries envelope field 14, the `op_hash` of the same device's previous op in the stream, and field 15, the tips of other devices' prefixes its writer had seen since it last listed them (`crates/sunrise-crypto/src/op_envelope.rs#OpEnvelope`, written by `crates/sunrise-core/src/engine/chain.rs#writer_links`).
* **Link checks on receipt.** A receiver checks both fields, and what earlier ops said this one would be, against the ops it holds. A gap is recorded as an expected op, a mismatch as fork evidence, and the op is applied either way (`crates/sunrise-core/src/engine/chain.rs#check_links`).
* **A running chain root per `(stream, device)`**, stored on each op inside the contiguous prefix and extended where the sync cursor moves (`crates/sunrise-core/src/engine/chain.rs#fold_chain`).
* **A per-stream digest exchanged between replicas** as a `StreamDigest` control op, and compared entry by entry on receipt (`crates/sunrise-core/src/engine/chain.rs#reconcile`).
* **Fork evidence retention.** A second envelope at a held `(stream, device, seq)` is kept verbatim in `fork_evidence`.

Still a target:

* **No rollback detection.** §Rollback detection has its input now, the chain roots, but no reconnect handshake compares them with the relay.
* **No integrity indicator in any client.** `Engine::chain_integrity` counts fork evidence, divergences and known missing ops; nothing renders them.
* **No peer-served backfill and no bisection.** A missing op is requested from the relay; a divergence names the device and a seq at or below which two replicas differ, not the first differing op.
* **`server_first_seen_ms` feeds no ordering rule.** The annotation does exist, but only per batch and only as advice: `Ack.server_first_seen_ms` (`crates/sunrise-wire-protocol/src/payloads.rs:93`) is stamped at `crates/sunrise-server/src/api/sync/publish.rs:111,226` and parsed back onto the synthesized `Ack` frame at `crates/sunrise-sync/src/sse.rs:962#send_frame`. Nothing persists it and nothing orders by it, and nothing here wants it.

Tampering with any single stored op is caught by the `OpEnvelope` AEAD and signature, which cover fields 14 and 15 like every other header field.

## Identity and replay invariants

The op envelope's `(stream_id, device_id, seq)` triple is the canonical replay-detection key. `0013_baseline.sql` declares `UNIQUE (stream_id, device_id, seq)` on `ops`, so a re-delivered op is dropped idempotently, and `sync_cursors.last_applied_seq` is the end of the contiguous prefix per `(stream_id, device_id)`.

Receivers enforce:

- `seq` starts at 1 per `(stream_id, device_id)`. An op above a gap is stored and applied, and sits outside the prefix until the gap fills; the relay replays from the cursor.
- Below a compaction floor ([ADR-0059](../11-adr/0059-client-op-log-compaction.md) §2) the prefix starts above the floor instead. Every seq at or below it is covered by the merge state and the floor's chain root, whether or not its row is still held, so it is neither a gap nor a missing op, and a delivery there is a duplicate. A different op at the floor's own seq is fork evidence like any other.
- A repeated `(stream_id, device_id, seq)` whose envelope is the same op (equal `op_hash`) is silently dropped (idempotent re-delivery).
- A repeated `(stream_id, device_id, seq)` whose envelope is a **different** verified op is **fork evidence**: the second envelope is kept in `fork_evidence`, the first stays the applied one, and a `core.chain.fork` warning is logged. Sync does not stop.

> **Amended ([ADR-0043](../11-adr/0043-commit-tree.md) §4).** A mismatched repeat used to be integrity-fatal: sync stopped. That is the "break" the merge invariant forbids, and it let a single stolen device key take a whole vault offline. It is now evidence, kept and surfaced.

## Per-device chains and the stream digest

```
op_hash(env)  = BLAKE3(canonical_cbor_envelope_bytes(env), 32)        ; field 11 included, magic prefix excluded
root(d, 0)    = BLAKE3::derive_key("sunrise.op_chain.init.v1", stream_id || device_id)
root(d, n)    = BLAKE3::derive_key("sunrise.op_chain.step.v1", root(d, n-1) || op_hash(op(d, n)))
digest(S, F)  = BLAKE3::derive_key("sunrise.stream_digest.v1",
                  stream_id || for each (d, n_d) in F sorted by d: d || u64be(n_d) || root(d, n_d))
```

`op_hash` is computed from a re-encoding of the envelope, so two replicas agree on it whatever encoding reached them. Each device's ops are folded in its own seq order, so a late op from one device changes nothing already folded for another, and no ordering key is shared with the merge. The frontier `F` is the replica's `sync_cursors`: each device with a non-empty contiguous prefix and that prefix's end. The chain root covers ops written before chaining existed, because it is computed from the bytes the replica holds.

> **Amended ([ADR-0043](../11-adr/0043-commit-tree.md)).** This section specified one root per Stream, folding every device's ops in the global `(hlc, device_id, seq)` order ([ADR-0027](../11-adr/0027-v1-self-host-first.md) removed the relay's clamp from that order). It was never called by a product path: an op arriving late with an earlier key forced a re-fold from that point, and a differing root could not say which device's ops differed. Its functions, `stream_root_init` and `stream_root_step` in `crates/sunrise-crypto/src/merkle.rs`, stay pinned by their frozen vectors. Compaction ([ADR-0059](../11-adr/0059-client-op-log-compaction.md) §7) did not adopt them: a snapshot commits to its frontier's per-device chain roots instead.

Devices with > 5 min skew display a `"Your clock is ≥ 5 minutes off; sync may produce unexpected ordering"` warning.

## Digest ops

Each device publishes a `StreamDigest` control op (`DOC_SCHEMA_V` 9) in a stream, sealed under that stream's key, when one is due: it has published none there yet and holds an op, 256 ops have entered the stream's log since its last, or 24 h have passed since its last and any op has entered. A `StreamDigest` op, from any device, counts toward none of the three, so idle devices do not answer each other's digests forever. The sync driver checks on its anti-entropy timer. A device publishes only in a stream it holds a key for.

```cddl
stream-digest = {
  "frontier": [* [bstr .size 16, uint, bstr .size 32]],   ; [device_id, n_d, root(d, n_d)], sorted by device_id
  "digest":   bstr .size 32,                              ; digest(stream, frontier)
  unknown-fields
}
```

The frontier is the writer's before the digest op itself.

> **Amended ([ADR-0043](../11-adr/0043-commit-tree.md) §5).** This section specified a `CheckpointPayload` of one root and a `covers` map, emitted at the end of a debounced batch, on graceful shutdown and after a crash. None of that was built. The digest op carries a root per device instead, and the cadence above is the whole rule.

## Verifying digests from peers

When device A applies a digest from device B for Stream S, a payload whose `digest` is not the digest of its own frontier, or whose frontier is not sorted by device id, is damaged and ignored (`core.chain.digest_invalid`). Otherwise, for each `(d, n, root)`:

1. A holds `d` through `n`: it compares its own `root(d, n)`. Equal confirms the two hold the same ops of `d` through `n`, and clears an earlier disagreement at or below `n`. Different is a **divergence**: A records it in `chain_divergence`, naming S, `d`, B and `n`, and logs `core.chain.divergence`. A keeps applying ops.
2. A holds fewer than `n`: A is missing `(its n_d, n]` of `d`. It keeps the claim in `chain_claims`, re-subscribes from its cursor at once, and checks the claim when its prefix reaches `n`.
3. A holds more: B is behind. Nothing is recorded.

A known missing op from field 14 or 15 is handled like case 2: it is recorded in `chain_expected`, the sync driver re-subscribes, and the op is checked against the named hash when it arrives.

## Rollback detection

A simpler attack: the relay serves a known-old snapshot to a device after that device was offline.

Target mitigation: every device already persists its chain roots per `(stream, device)`. On reconnect, a delivery that cannot extend a stored root is a rollback. Not built: no handshake compares them, and the relay serves from the cursor, which a rollback below the cursor cannot reach.

## Fork detection (server view divergence)

A hostile relay could serve different op sets to different devices. Each replica's own chain then looks consistent, and only a comparison between replicas shows the split: a digest from one replica disagrees with another's root for the same device and seq, which is case 1 above. A relay that withholds a digest delays the comparison; it cannot fake agreement, because every digest is signed by its writer and sealed under the stream key.

This is *eventual* tamper evidence, not real-time prevention. We accept the residual risk and document it.

## Per-vault Integrity indicator

Target: a persistent UI element in advanced settings, fed by `Engine::chain_integrity`:

- **Green** — no fork evidence, no divergence, nothing known missing.
- **Yellow** — ops known missing (`wanted` > 0), which a resync is expected to clear.
- **Red** — fork evidence or a divergence. The vault keeps syncing; the indicator names the device so the user can revoke it.

A `red` state offers a "save forensic bundle" affordance that exports the relevant envelopes and roots. The envelopes in `fork_evidence` are signed by the device they implicate, so anyone can re-verify them.

## Out of scope

- **Transparency logs** (Certificate-Transparency-style public log of identity-key events). Not designed; it would pair with federation, which does not exist either.
- **Multi-party verification of relay integrity** (oblivious transfers, third-party auditor). Out of scope.
