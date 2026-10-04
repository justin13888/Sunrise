# 0043 — Ops form per-device hash chains with causal heads, and replicas compare a per-stream digest

**Status:** accepted

Built by [#325](https://github.com/justin13888/Sunrise/issues/325). It was
ranked in phase P1 of [`../roadmap.md`](../roadmap.md). The open questions it
was proposed with are answered in §Resolved questions, and §4 and §5 are
amended where the answers changed them. Fields 14 and 15, the three
`derive_key` contexts and the `StreamDigest` shape are now contracts.

**Relates to:**

- [ADR-0044](./0044-per-field-ops.md) (accepted), which makes convergence
  independent of delivery order. That is what lets a chain be *checked*
  without being *enforced*.
- [ADR-0045](./0045-schema-identity-and-feature-gating.md) (accepted), which
  reserves envelope field 13. This record reserves 14 and 15 under ADR-0045
  §5's additive-field rule.

**Amends**
[`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
§Per-Stream Merkle root, §Fork detection and §Identity and replay invariants.

## Context

This section describes the tree before #325.

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

## Decision

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
  the relay or a peer: it writes a legacy link instead, which asserts nothing,
  rather than a guess, which would assert something false. It does not stop
  writing, which would be the "break" the invariant forbids.
- **`op_hash` is computed from a re-encoding**, not from the bytes a peer
  sent, and excludes the 5-byte magic prefix. Two replicas holding one
  envelope therefore agree on its hash whatever encoding reached them
  (`crates/sunrise-crypto/src/op_envelope.rs#op_hash`).

### 2. Each op names what its writer had seen: `heads`, envelope field 15

```cddl
? 15: [+ head],        ; heads; absent when there is none
head = [ bstr .size 16, uint, bstr .size 32 ]   ; [device_id, seq, op_hash]
```

`heads` lists, for each **other** device whose applied prefix in this stream
advanced since the writer last listed it, the tip of that prefix: the device
id, the seq and the `op_hash`. The list is sorted by `device_id`, a device
appears at most once, and it holds at most 256 entries
(`MAX_CHAIN_HEADS`). An empty array is malformed: the writer omits the field
instead, so a set has exactly one encoding. Writer and reader refuse a list
out of that shape alike.

- **Above the cap, a device is deferred, not dropped.** The writer lists the
  256 lowest device ids and marks only those listed (`chain_heads_sent`), so
  the next op lists the rest. Truncating would have weakened the causal claim
  silently.

- **It is a delta, not a full vector clock.** The writer's own `prev_hash`
  chain carries everything it listed before. So the full causal context of an
  op is recovered by walking back along the chain, not by repeating it in every
  op.
- **An op's causal past is well defined.** It is the op's own chain, plus every
  op reachable through any `head` on that chain. Two ops are concurrent exactly
  when neither is in the other's causal past.
- **A `head` a receiver does not hold is a *known* missing op.** The receiver
  knows the device, the seq and the hash it should find, and records them
  (`chain_expected`). It MUST request that op, which it does by re-subscribing
  from its cursor (§5). It MUST NOT wait for it before applying the op that
  named it.
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
diverge**. The same holds for an op whose writer is not the device at `n - 1`
but another device's `head` naming a position this replica holds: a match is
nothing, a mismatch is fork evidence, and an unheld position is an
expectation. An op that later arrives where an expectation waits is checked
against it. Parked ops (ADR-0045 §4) are checked like applied ones, because
they are in the log and count toward the prefix.

### 4. Two ops at one position is equivocation, and both are kept

**Fork evidence** is two distinct claims, both correctly signed by the same
device, about the same `(stream_id, device_id, seq)`: two envelopes at that
seq, or an envelope whose field 14 or 15 names a different op there than the
one this replica holds. The signed envelopes are the proof. Anyone can
re-verify them, and neither can be forged without the device's key.

- **Both ops are retained; the first is the one applied.** The second
  envelope at a seq cannot go into `ops` under the existing
  `UNIQUE (stream_id, device_id, seq)`, so it goes into `fork_evidence`
  verbatim, beside the hash of the op `ops` holds. An op that names the other
  branch, at a position this replica does not hold, is an ordinary op and is
  applied. The second envelope at one seq is **not** materialized. Applying it
  would be deterministic only once every replica holds it, and nothing yet
  carries it to them (open question 4, resolved below), so a replica that
  applied its half alone would diverge from its peers by construction. Not
  applying it keeps today's behaviour: the relay serves one order, and a relay
  that serves two is what §5 detects. No data is lost: the envelope is kept,
  so a later change that carries evidence to peers can fold both halves in.
- **It is surfaced, not fatal.** It is a `core.chain.fork` warning, a row
  counted by `Engine::chain_integrity`, and the evidence an integrity
  indicator shows. The rule in
  [`../03-crypto/audit-and-tamper-evidence.md`](../03-crypto/audit-and-tamper-evidence.md)
  §Identity and replay invariants that stopped sync on a mismatched repeat of
  `(stream_id, device_id, seq)` is amended to this. Stopping sync is exactly
  the "break" the invariant forbids.
- **Evidence does not propagate yet.** A peer learns of a fork from the
  digest (§5): two replicas holding different halves disagree on that
  device's root, and each records a `chain_divergence` that names the device
  and the seq at or below which they differ.

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
  contiguous prefix this replica already tracks. The root moves where the
  cursor does (`crates/sunrise-core/src/engine/oplog.rs#upsert_sync_cursor`),
  one step per op, and is stored on the op (`ops.chain_root`), so a root at
  any seq inside the prefix is one read. A device with an empty prefix is
  left out of the frontier.
- **Exchange.** Each device publishes a signed `StreamDigest` control op
  (`DOC_SCHEMA_V` 9) in the stream it describes, sealed under that stream's
  key:

  ```cddl
  stream-digest = {
    "frontier": [* [bstr .size 16, uint, bstr .size 32]],   ; [device_id, n_d, root(d, n_d)]
    "digest":   bstr .size 32,
    unknown-fields
  }
  ```

  The frontier is the writer's before the digest op itself. The cadence takes
  over the "every 256 ops or 24 h" checkpoint rule in
  `audit-and-tamper-evidence.md`: a digest is due in a stream where this
  device has none yet and holds an op, where 256 ops have entered the log
  since its last one, or where a day has passed since its last one and any op
  has entered (`Engine::publish_due_stream_digests`, which the sync driver
  calls on its anti-entropy timer). A `StreamDigest` op, from this device or
  any other, counts toward none of the three: otherwise an idle account's
  devices would answer each other's digests once a day per stream, forever.
  A device publishes only in a stream it holds a key for; it never mints one
  for a digest. A payload whose `digest` is not the digest of its own frontier, or whose frontier is not sorted by
  device id, is damaged and read for nothing.
- **Reconciliation compares per-device entries, not the whole digest.** For
  each `(d, n, root)` in a peer's frontier:
  - If this replica holds `d` through `n` and its own `root(d, n)` differs, the
    two hold different ops for `d` at or below `n`. That is a fork, or
    corruption, and it is recorded as a `chain_divergence` naming the stream,
    `d`, the peer and `n`. An agreement at or above a recorded divergence
    clears it, because roots chain. Locating the first differing seq needs a
    request the peer answers, which needs a peer channel (question 5); it is
    not built.
  - If this replica holds fewer than `n` ops for `d`, it is missing exactly
    `(its n_d, n]` for `d`. It keeps the claim (`chain_claims`, the highest per
    peer), checks it when its prefix reaches `n`, and re-subscribes from its
    cursor, which asks the relay for exactly the ops past it. If the relay no
    longer has those ops, the claim stays, and says so.
  - If this replica holds more, the peer is behind, and nothing is wrong.

  Divergence and omission are therefore **detected**, not assumed away, and
  detection does not rely on the relay's view. A known missing op, from a
  digest or from field 14 or 15, is loss evidence to the sync driver, which
  re-subscribes at once instead of waiting for its timer.

### 6. How this relates to what exists

- **Cursors and relay resync stay.** They remain the fast path. The digest is
  the check that the fast path worked, and the key to recovering when it did
  not.
- **The global-order fold in `merkle.rs` stays test-only.** The digest in §5
  replaces the per-stream Merkle root as the design of record for detecting
  omission and reordering. The chain root and the digest live beside it in
  `merkle.rs`, frozen by their own vectors. The old fold stays pinned until
  compaction decides the snapshot's commitment format (question 7).
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

| Option | Why not |
|---|---|
| **Keep the global-order Merkle fold** in `merkle.rs` | It is not incremental under late arrival, which is the normal case. It also cannot say *which* device's ops differ. |
| **Full vector clock in every op** | O(devices) on every op, when the chain already implies all but the delta. |
| **Only a local running fold (§5), no `prev_hash`** | This is the cheapest option, and it detects divergence between replicas. What it cannot do is let the *writer* attest its own order. So a single replica cannot tell "the relay reordered" from "the writer did", and a snapshot cannot cite a writer-signed frontier. See question 1. |
| **Materialize both halves of a fork** | Deterministic only once every replica holds both, and nothing carries the second half to peers yet. See §4 and question 4. |
| **Publish digests outside the op log** | There is no channel between replicas other than the relay's op stream, and an op is signed, sealed and replayable like every other. |
| **Refuse or park an op on a broken link** | That makes one lost op block every later op. It also re-introduces delivery-order dependence, which ADR-0034 forbids. |
| **Stop sync on fork**, as the audit doc says today | That is the "break" the invariant forbids, and it lets a single stolen key take a whole vault offline. |

## Resolved questions

Each answer below is what #325 built. The questions as they were proposed
follow, under the same numbers.

1. **`prev_hash` is kept, and it is the previous op's hash.** A single
   replica can tell a writer's order from a relay's only through it, and a
   snapshot (#330) needs a writer-signed frontier to cite. The chain-root
   variant would make every op attest the whole history, but a receiver could
   check it only against a root it had folded itself, which is the digest's
   job already.
2. **The digest is over the op set only.** A projection digest needs the
   canonical projection encoding #326 and #327 are building; it can join the
   payload later as an unknown-tolerant key, without a new kind.
3. **`heads` is capped at 256 entries, and above the cap a device is deferred
   to the next op, never dropped (§2). It stays in the cleartext header.**
   Moving it into the ciphertext would make the inner op carry an envelope
   concern; the relay learns nothing from it it did not learn by serving the
   ops it names.
4. **Evidence does not travel as its own op yet, and the second half of a
   fork is retained but not applied (§4).** The digest already tells peers
   that two replicas hold different ops for a device. A carrier op, and
   folding both halves in once every replica holds both, are follow-up work.
5. **No peer-served backfill.** A missing op is requested from the relay by
   re-subscribing. When the relay no longer has it, the claim or expectation
   stays recorded, so the loss is visible rather than silent. A peer channel
   is out of scope.
6. **An honest fork is recorded like any other.** A restored device that
   reuses its `(device_id, seq)` produces fork evidence and a divergence, both
   naming it. Minting a new device id on restore is the remedy and belongs to
   the restore flow, not to the chain; no epoch is added to the chain. The
   evidence does not revoke anything, so ADR-0041's standing rules are
   untouched.
7. **The global-order fold stays, test-only and pinned**, until compaction
   (#330) chooses the snapshot's commitment format. The chain root and the
   digest are new functions beside it with their own frozen vectors.
8. **Compaction is #330's to decide.** What this record fixes is what a
   snapshot must carry for chains to continue above it: `root(d, n_d)` and
   `op_hash(op(d, n_d))` for every `d` in its frontier, so the first op above
   it can be checked against the second.
9. **The frontier covers every device, revoked or not.** The digest is a
   claim about which ops a replica holds, not about which it trusts, and a
   revocation removes no op. Keeping revocation out of it is what keeps it a
   pure function of the op set while ADR-0041's fold can unwind a revocation.

## The questions as proposed

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

## Consequences

- **Two envelope fields**, 14 and 15. They are additive under ADR-0045 §5, so
  there is no container bump and no refusal by older readers, which preserve
  them and still verify their signatures.
- **One new control op**, `StreamDigest`, at `DOC_SCHEMA_V` 9. A v8 build
  parks it and replays it after an upgrade.
- **Migration 0034** (`STORAGE_V` 34): `op_hash` and `chain_root` columns on
  `ops`, and the tables `chain_heads_sent`, `chain_expected`, `chain_claims`,
  `fork_evidence` and `chain_divergence`. The hashes and roots of rows a vault
  already holds are computed from their envelopes on the first fold.
- **The integrity indicator in `audit-and-tamper-evidence.md` gets something
  to show**: `Engine::chain_integrity` counts fork evidence, divergences and
  known missing ops. No client renders it yet.
- **[#326](https://github.com/justin13888/Sunrise/issues/326)'s harness gains an oracle.** Two replicas with equal digests at
  equal frontiers hold the same op set, and a differing projection digest at
  the same frontier is a merge bug by definition.
