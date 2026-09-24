# 0043 — Ops form per-device hash chains with causal heads, and replicas compare a per-stream digest

**Status:** proposed

This is a design of record that has not been built. It is ranked in phase P1
of [`../roadmap.md`](../roadmap.md) and tracked by [#325](https://github.com/justin13888/Sunrise/issues/325). Under
[ADR-0042](./0042-v0-forever.md) §4, a `proposed` record is binding on nothing
yet: its field numbers and formulas are reservations, not contracts. The open
questions at the end MUST be answered, by amending this record, before it is
moved to `accepted`.

**Relates to:**

- [ADR-0044](./0044-per-field-ops.md) (accepted), which makes convergence
  independent of delivery order. That is what lets a chain be *checked*
  without being *enforced*.
- [ADR-0045](./0045-schema-identity-and-feature-gating.md) (accepted), which
  reserves envelope field 13. This record reserves 14 and 15 under ADR-0045
  §5's additive-field rule.

**Would amend**
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Per-Stream Merkle root and §Fork detection.

## Context

The owner's invariant is:

> **Merging vaults across client versions MUST NEVER break and MUST NEVER lose
> data.**

An invariant nobody can check is a hope. Today no replica can tell whether it
holds the same op set as another.

- **Ops are signed one at a time and linked to nothing.** The envelope binds
  `(stream_id, device_id, seq, hlc)`
  (`crates/sunrise-crypto/src/op_envelope.rs#OpEnvelope`). It carries no
  reference to any earlier op.
- **The one integrity structure in the tree is unused.** The per-stream Merkle
  fold in `crates/sunrise-crypto/src/merkle.rs` is called only by
  `crates/sunrise-crypto/tests/frozen_vectors.rs`. Its fold order is the global
  `(hlc, device_id, seq)` order. So an op that arrives late, with an earlier
  key, forces a re-fold from that point. That is one reason nothing was built
  on it.
- **The cursor is a contiguous prefix per `(stream, device)`.** An op above a
  gap is stored but sits outside the prefix
  (`crates/sunrise-core/src/engine/oplog.rs#upsert_sync_cursor`). A gap is
  visible to the replica that has it. But a relay that withholds the *last* op
  of a device's sequence leaves no gap at all.
- **Anti-entropy trusts the relay.** Resync is a `Subscribe` that asks the
  relay to replay everything past each cursor (`crates/sunrise-core/src/sync_driver.rs`,
  module docs §Inbound: resync). Replicas never compare their state with each
  other.
- **Forks are unrepresentable.** A device that signs two different ops at the
  same `(stream, seq)` produces two ops that each verify. Their only trace is
  the `UNIQUE (stream_id, device_id, seq)` on `ops`, which keeps whichever
  arrived first. Everything after that point depends on arrival order. A fork
  can be caused by an attacker holding a device key, or by a device restored
  from an old backup.

## Decision (proposed)

### 1. Each op commits to its device's previous op: `prev_hash`, envelope field 14

```cddl
? 14: bstr .size 32,   ; prev_hash   op_hash of this device's op at (stream_id, seq - 1)
```

```
op_hash(env) = BLAKE3(canonical_cbor_envelope_bytes(env), 32)
```

`op_hash` is the `env_hash` that
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Per-Stream Merkle root already defines: the hash of the envelope's full
canonical encoding, **including field 11**, the signature. There is one hash of
an op in the system, not two.

- **`seq = 1` carries no field 14.** Nor does any op written before this record
  lands. An op without the field is a *legacy link*: it asserts nothing about
  its predecessor.
- **The first chained op after a legacy prefix still commits to its
  predecessor.** A writer can compute `op_hash` of any op it holds. So a chain
  starts wherever a device upgrades, and it covers the op before it.
- **The field is under the AAD and the signature** by ADR-0015's exclusion
  rule. It is additive, so it needs no container bump (ADR-0045 §5).
- **A writer MUST fill `prev_hash` from its own op log**, never from memory
  alone. A device that cannot find its own op at `seq - 1` has lost local
  history. It MUST NOT emit a chained op until it has recovered that op from
  the relay or a peer (open question 6).

### 2. Each op names what its writer had seen: `heads`, envelope field 15

```cddl
? 15: [* head],        ; heads
head = [ bstr .size 16, uint, bstr .size 32 ]   ; [device_id, seq, op_hash]
```

`heads` lists, for each **other** device whose applied prefix in this stream
advanced since the writer's previous op in this stream, the tip of that
prefix: the device id, the seq and the `op_hash`. The list is sorted by
`device_id`, and a device appears at most once.

- **It is a delta, not a full vector clock.** The writer's own `prev_hash`
  chain carries everything it listed before. So the full causal context of an
  op is recovered by walking back along the chain, not by repeating it in every
  op.
- **An op's causal past is well defined.** It is the op's own chain, plus every
  op reachable through any `head` on that chain. Two ops are concurrent exactly
  when neither is in the other's causal past.
- **A `head` a receiver does not hold is a *known* missing op.** The receiver
  knows the device, the seq and the hash it should find. It MUST request that
  op (§5). It MUST NOT wait for it before applying the op that named it.
  ADR-0044's merge does not need causal delivery, and delaying application
  would make one lost op block every op after it.

### 3. Receivers check links; they never refuse on them

When an op at `(stream, device, seq = n)` is applied, one of four cases holds:

| Case | Receiver holds `n - 1`? | `prev_hash` | Outcome |
|---|---|---|---|
| Linked | yes | equals `op_hash(n - 1)` | Apply. The chain extends. |
| Gap | no | any | Apply. Record the expected hash for `n - 1`, and request it (§5). When it arrives, check it against the recorded hash. |
| **Fork** | yes | **differs** | Apply, and record **fork evidence** (§4). |
| Legacy | any | absent | Apply. No link is asserted. |

No case refuses the op. [ADR-0034](./0034-revocation-bounds-reads-not-writes.md)
established that no replica refuses an op, and ADR-0044 makes every apply
order-independent. So a link failure is **evidence to surface, not a reason to
diverge**.

### 4. Two ops at one position is equivocation, and both are kept

**Fork evidence** is two distinct envelopes, both correctly signed by the same
device, that claim the same `(stream_id, seq)` or the same `prev_hash`. The two
envelopes are the proof. Anyone can re-verify them, and neither can be forged
without the device's key.

- **Both ops are retained.** The second one cannot go into `ops` under the
  existing `UNIQUE (stream_id, device_id, seq)`, so it goes into a
  `fork_evidence` table with both envelopes. The merge treats both as applied:
  every field op in both folds in under its own stamp, per ADR-0044. That is
  deterministic, because both replicas eventually hold both ops.
- **It is surfaced, not fatal.** The user sees an integrity warning that names
  the device, and is offered revocation. The rule in
  [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
  §Identity and replay invariants, which stops sync on a mismatched repeat of
  `(stream_id, device_id, seq)`, would be amended to this. Stopping sync is
  exactly the "break" the invariant forbids.
- **Evidence propagates.** A replica that holds fork evidence sends the second
  envelope to its peers through the relay (open question 4). Otherwise each
  replica would keep only the half it happened to receive first.

### 5. Replicas exchange a per-stream state digest

Each replica keeps, for each stream and each device, a running **chain root**
over that device's contiguous prefix:

```
root(d, 0) = BLAKE3::derive_key("sunrise.op_chain.init.v1", stream_id || device_id)
root(d, n) = BLAKE3::derive_key("sunrise.op_chain.step.v1", root(d, n-1) || op_hash(op(d, n)))
```

The **stream digest** at a frontier `F = {(d, n_d)}` is:

```
digest(stream, F) = BLAKE3::derive_key("sunrise.stream_digest.v1",
                      stream_id || for each d in F sorted by device_id:
                                     device_id || u64be(n_d) || root(d, n_d))
```

- **It is incremental.** Applying the next op of a device is one hash step.
  Late arrival from another device changes nothing already folded, unlike the
  global-order Merkle fold in `merkle.rs`.
- **It works on legacy ops.** The chain root is computed locally from the bytes
  the replica holds, so it needs no `prev_hash`. Field 14 makes the writer
  *attest* the order. The digest makes replicas *agree* on it.
- **The frontier is the cursor.** `n_d` is `sync_cursors.last_applied_seq`, the
  contiguous prefix this replica already tracks. Computing a digest costs
  nothing extra.
- **Exchange.** Each device periodically publishes a signed `StreamDigest`
  control op in the stream it describes, sealed under that stream's key:

  ```cddl
  stream-digest = {
    "frontier": [+ [bstr .size 16, uint, bstr .size 32]],   ; [device_id, n_d, root(d, n_d)]
    "digest":   bstr .size 32,
    ? "projection": bstr .size 32,     ; optional; see open question 2
    unknown-fields
  }
  ```

  The cadence would take over the "every 256 ops or 24 h" checkpoint rule in
  `audit-and-tamper-evidence.md`.
- **Reconciliation compares per-device entries, not the whole digest.** For
  each `(d, n, root)` in a peer's frontier:
  - If this replica holds `d` through `n` and its own `root(d, n)` differs, the
    two hold different ops for `d` at or below `n`. That is a fork, or
    corruption, and the replica bisects by requesting `op_hash` at chosen
    seqs.
  - If this replica holds fewer than `n` ops for `d`, it is missing exactly
    `(its n_d, n]` for `d`. It re-subscribes from its cursor. If the relay no
    longer has those ops, it asks peers (open question 5).
  - If this replica holds more, the peer is behind, and nothing is wrong.

  Divergence and omission are therefore **detected**, not assumed away, and
  detection does not rely on the relay's view.

### 6. How this relates to what exists

- **Cursors and relay resync stay.** They remain the fast path. The digest is
  the check that the fast path worked, and the key to recovering when it did
  not.
- **`merkle.rs` stays test-only.** The digest in §5 replaces the per-stream
  Merkle root as the design of record for detecting omission and reordering.
  Whether `merkle.rs` is deleted or re-pointed is open question 7.
- **The Merkle fold order** (`(hlc, device_id, seq)`, clause 6 of
  [ADR-0027](./0027-v1-self-host-first.md)) is not needed by the digest, which
  folds each device separately. It remains the canonical *apply* order for
  rebuild.
- **Revocation is unchanged.** [ADR-0041](./0041-peer-side-revocation-is-a-fold.md)
  folds revocations from the op set. The chain only makes the op set
  verifiable.

### 7. How this is, and is not, "like git"

There is a real resemblance: content-addressed ops, each committing to a
parent, and a frontier that commits to all of history. But three things
differ, and each difference is deliberate:

- **There are no branches.** Each device's chain is linear by construction.
  Two children of one parent is not a branch to merge later. It is
  equivocation (§4).
- **There are no merge commits.** Concurrent chains never reconcile through a
  new op that chooses a result. They converge because every field merges by
  its CRDT type (ADR-0044), so the "merge" is a pure function of the op set and
  never needs to be written down. `heads` records what a writer had *seen*, not
  a choice it made.
- **History is never rewritten or checked out.** Nothing rebases, amends or
  resets. Compaction may fold old ops into a snapshot ([#330](https://github.com/justin13888/Sunrise/issues/330)), but the
  snapshot commits to the chain roots at its frontier, so the history it
  replaces is still identified.

## Alternatives considered

| Option | Why not (so far) |
|---|---|
| **Keep the global-order Merkle fold** in `merkle.rs` | It is not incremental under late arrival, which is the normal case. It also cannot say *which* device's ops differ. |
| **Full vector clock in every op** | O(devices) on every op, when the chain already implies all but the delta. |
| **Only a local running fold (§5), no `prev_hash`** | This is the cheapest option, and it detects divergence between replicas. What it cannot do is let the *writer* attest its own order. So a single replica cannot tell "the relay reordered" from "the writer did", and a snapshot cannot cite a writer-signed frontier. Kept as open question 1, because it may be enough. |
| **Refuse or park an op on a broken link** | That makes one lost op block every later op. It also re-introduces delivery-order dependence, which ADR-0034 forbids. |
| **Stop sync on fork**, as the audit doc says today | That is the "break" the invariant forbids, and it lets a single stolen key take a whole vault offline. |

## Open questions

These MUST be resolved before this record becomes `accepted`:

1. **Is `prev_hash` worth its 35 bytes per op?** The local fold in §5 detects
   divergence without it. Writer attestation matters for single-replica
   detection and for snapshots. Is that worth one field on every op? A variant
   is to set `prev_hash = root(d, n-1)`, the writer's chain root rather than
   the previous op hash. That makes every op attest the writer's whole history.
2. **Digest over the op set, over materialized state, or both?** The op-set
   digest detects missing or extra ops. A projection digest (a canonical hash
   of the materialized rows) would also detect a *merge* bug: two replicas with
   the same ops and different state. It costs a canonical projection encoding,
   which [#326](https://github.com/justin13888/Sunrise/issues/326) and [#327](https://github.com/justin13888/Sunrise/issues/327) need anyway. Should it be normative, or a
   diagnostic?
3. **The cost of `heads`.** A delta-heads list is bounded by the number of
   devices writing to the stream. For a typical user that is under 5 entries of
   about 55 bytes each. Should there be a cap, and what happens above it?
   Truncating would silently weaken the causal claim. Should `heads` move
   inside the ciphertext? The relay has no use for it, and in the header it
   exposes which devices' ops a writer had seen before writing. The relay
   already knows this because it served those ops, but it is still metadata
   the relay does not need.
4. **Relay visibility of `prev_hash`.** In the cleartext header, the relay
   could detect forks itself, and could also selectively withhold one half of
   a fork. Should fork evidence travel as its own control op, so that it
   reaches peers whether or not the relay forwards the second envelope?
5. **Peer-served backfill.** When the relay has evicted ops that a digest shows
   are missing (the relay keeps 30 days or 256 MiB per channel), which peer
   serves them, over what channel, and with what authorization? Today there is
   no peer-to-peer path.
6. **Honest forks.** A device restored from a backup of its own vault reuses
   `(device_id, seq)`, and so equivocates without any attacker. Should restore
   mint a new device id (the simplest fix), or should the chain carry an
   *epoch* that a restore bumps? Either way, how does this interact with
   `revoke_device`'s standing rules in ADR-0041?
7. **`merkle.rs` and the audit doc.** Delete the global-order fold, or keep it
   as the snapshot's commitment format? The frozen vectors that pin it would
   move either way.
8. **Compaction ([#330](https://github.com/justin13888/Sunrise/issues/330)).** A snapshot at frontier `F` must carry
   `root(d, n_d)` for every `d` in `F`, plus the parked ops (ADR-0045 §4), so
   that chains can continue above it. The first op above a snapshot links to
   an op the new device never holds. Is `prev_hash` then checked against a
   snapshot-supplied `op_hash`?
9. **Revocation and chains.** A revoked device's later ops still chain. Should
   the digest frontier stop at a revoked device's last pre-revocation op? If
   so, how does that stay a pure function of the op set, given that ADR-0041's
   fold can unwind a revocation?

## Consequences (if accepted)

- **Two envelope fields**, 14 and 15. They are additive under ADR-0045 §5, so
  there is no container bump and no refusal by older readers, which preserve
  them and still verify their signatures.
- **Two new control ops**, `StreamDigest` and possibly a fork-evidence carrier.
  One new table (`fork_evidence`), and chain-root columns on `sync_cursors`.
- **The integrity indicator in `audit-and-tamper-evidence.md` gets something
  to show**: the frontier agreed with each peer, and any fork evidence held.
- **[#326](https://github.com/justin13888/Sunrise/issues/326)'s harness gains an oracle.** Two replicas with equal digests at
  equal frontiers hold the same op set, and a differing projection digest at
  the same frontier is a merge bug by definition.
