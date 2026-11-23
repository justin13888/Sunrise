# 0075 — An editor writes a shared Stream's contents and nothing else, and every replica that applies the Stream is the boundary

**Status:** accepted

**Extends** [ADR-0074](./0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md)
(the sharing threat model and the relay viewer grant), which named this
decision in its §3 and its revisit item 1, and
[ADR-0020](./0020-v1-must-demotions.md) §Addendum (a read-only grant per
Stream, with expiry, revocable by key rotation).
**Amends** [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md)
§Roles, §Egress scrubbing and §Edits by editors, and
[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md)
§A5's reference scrubbing semantics, which rested the same rule on the
editor's UI.
**Amends, for editor ops in a shared Stream only,**
[ADR-0034](./0034-revocation-bounds-reads-not-writes.md) (no replica refuses
an entity op), [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) §3
(entity writes stay ungated) and
[ADR-0045](./0045-schema-identity-and-feature-gating.md) §4 (an op is applied
or parked) and [ADR-0043](./0043-commit-tree.md) §4 (of two ops at one
position, the first is the one applied): §9 says what each amendment is and
what it leaves standing. ADR-0043 §3 is not amended, and §9 says why.
**Depends on** [ADR-0016](./0016-hlc-timestamps.md) (HLC),
[ADR-0043](./0043-commit-tree.md) (each device's ops chain by `prev_hash`),
[ADR-0044](./0044-per-field-ops.md) (per-field ops) and
[ADR-0045](./0045-schema-identity-and-feature-gating.md) (structural features).
**Answers** [#1321](https://github.com/justin13888/Sunrise/issues/1321).
**Built:** nothing. Every build refuses an `editor` grant until all of §8 is
built.

## Context

`ShareGrantPayload` field 4 is a role, and the role is a text value so a later
build can add one without a new field. The only role built is `viewer`
(`crates/sunrise-crypto/src/share_grant.rs`): a grant whose role is anything
else decodes as `ShareGrantError::UnsupportedRole`, and nothing reads it as a
viewer. [`sharing-with-others.md`](../03-crypto/sharing-with-others.md) has
always listed an `editor` row, and the parity matrix ranks "view shared stream
as editor" a MUST, but nothing decided what an editor may do or who stops it
doing more.

A viewer needs no such answer. ADR-0074 §3 holds the viewer's writes off the
owner's channel by construction: the relay appends a caller's ops to the
caller's own channel only. Its replicas hold the same line without trying: an
envelope is verified under a `DeviceCert` from the verifying account's own
vault-meta log
([`data-encryption-format.md`](../03-crypto/data-encryption-format.md)
§Signature key resolution), and a recipient's devices are in none of the
owner's, so their ops never verify there. An editor ends both. Its ops must
reach the owner's channel, and they must verify on replicas that never paired
with it.

Three facts in the tree bound the answer:

- **Holding a Stream key is not authority to write.** The key is symmetric:
  everyone sealed an epoch can produce envelopes that open under it. Only the
  envelope's device signature says who wrote one.
- **A recipient cannot read the owner's vault-meta log.** The `ShareGrant` op
  that records a share is sealed there, and the recipient never reads it. So a
  viewer, or a second editor, cannot learn from it who else may write.
- **An editor writes its own clock.** An envelope's HLC is the writer's. Any
  rule that ends an editor's authority "at time T" by comparing T with the
  editor's own stamps is a rule the editor can backdate past.

## Decision

### 1. What an editor may do

An `editor` grant confers everything a `viewer` grant does, plus one thing.
The editor may write entity ops into the shared Stream's own log. These are
the ops whose envelope `stream_id` is the shared Stream, under an epoch it was
sealed, and they may create, patch and delete the entities that live in that
log below the Stream: its Tasks, Blocks, Notes and Attachments. An editor's
op applies with the same weight as one of the owner's (§5).

### 2. What an editor may never do

- **Write a control op, of any family, in any stream.** No `key_envelope`,
  `device_cert`, `device_revoke*`, `share_grant`, `share_revoke`,
  `identity_transition`, `vault_requires`, `device_features` or snapshot.
  Granting, revoking, re-sharing and rotating stay the owner identity's alone,
  as §Roles always said.
- **Write the Stream entity itself.** Its name, color, pause, archive,
  deletion and sharing are the owner's. The grant seals the name and color,
  and an editor that could rename the Stream would show every other recipient
  a name the owner never sealed.
- **Write any other stream.** That includes the owner's vault-meta log, any
  other Stream of the owner's, and any other recipient's channel.
- **Move an entity out of the shared Stream, or name one outside it.** An op
  that sets an entity's Stream to another, or carries an `EntityRef` (a Note's
  `{kind: "ref"}` included) to an entity that is not in the shared Stream, is
  outside the grant. §Egress scrubbing used to rest on "an editor's UI cannot
  construct such a ref". The boundary in §3 makes that structural.
- **Join the owner's account.** An editor is never a device of the owner's
  account. It is not on the roster, it is sealed no `key_envelope` and no key
  bundle, and it holds no membership authority. It holds exactly the epochs
  its grants seal.

### 3. The boundary is every replica that applies the Stream, by itself

The owner's devices, every viewer and every editor each decide, from the
shared Stream's log alone, whether an editor's op applies. Nobody decides it
for them: not the relay, which is a party the boundary is drawn against
(ADR-0074 §Threat model), and not the owner's vault-meta log, which recipients
cannot read. An op applies only if all four of these hold:

- **(a) Its signer chains to an editor identity.** The envelope's device
  signature verifies under a `DeviceCert` that the editor identity signed. The
  `ID_S_pub` that cert verifies under hashes, by `identity_id_from_pub`, to the
  `recipient_identity_id` of an editor grant on this Stream. Because the
  identity id is a hash of the key, a cert from any other identity cannot pass
  as the editor's.
- **(b) The owner attests the grant inside the Stream.** The owner identity in
  force signs a record that names the editor identity, its `ID_S_pub` and the
  role, and publishes it in the shared Stream's own log, where every holder of
  the Stream key reads it. The vault-meta `ShareGrant` op stays the owner's
  private record. This attestation is the public one, and a replica that
  cannot find it treats the signer as no editor.
- **(c) It is inside the editor's frontier.** The op is under an epoch one of
  the editor's grants sealed, and it falls at or below any revocation cut §4
  sets for its device.
- **(d) Its content is inside §1 and §2.**

An op whose envelope signature verifies but which fails (a) to (d) is
**parked, not dropped and not a sync failure**
([ADR-0045](./0045-schema-identity-and-feature-gating.md) §4). It is written
to `ops` like any other op, under a new parking reason, `editor_authority`. It
counts as received, so it advances the sync cursor and no cursor freezes
against relay retention. It is not `LossEvidence`. Nothing of it is
materialized.

Whether an editor op applies is **a function of the Stream's retained log**,
never of the order that log arrived in. A replica decides (a) to (d) from the
attestations, cuts and certs it holds, and every one of those is an op in the
same log. So the decision is re-taken whenever one of them lands: an
attestation, a cut or a writer cert arriving in the Stream retries every op
parked there as `editor_authority`, through the full apply path, in
`(hlc, device_id, seq)` order, exactly as ADR-0045 §4's replay does. An editor
op that arrives before its attestation is parked, and applies when the
attestation arrives. One that arrives after it applies at once. A replica
that met the two in one order and a replica that met them in the other end in
the same state. A cut that lands after ops past it were applied takes
them back out (§4). Two replicas that hold the same log therefore hold the
same Stream, whatever order either received it in.

An envelope whose signature does not verify stays the hard reject the
verification order already makes it.

### 4. An editor's authority ends at a frontier, never at a time

Revoking an editor is the viewer's revocation (rotate the epoch, seal it to
everyone but the revoked, withdraw the relay grant), plus a cut. The
`share_revoke` op for an editor carries, for each of the editor's devices that
wrote into the Stream, the last `(seq, op_hash)` the owner's device accepted.
The owner republishes that cut in the Stream beside the attestation (§3(b)).
For a revoked editor's device, every replica applies only the op the cut
names and that op's `prev_hash` ancestors (ADR-0043 §1), and nothing else,
whatever its HLC. A later `seq` is past the cut. A second envelope at a `seq`
the chain already holds is off it. The editor cannot write either kind and
have it land below the cut. An op past the cut or off the chain fails §3(c)
and is parked like any other op that fails §3. The chain check that decides
it is an authority check, which a cut makes possible. It is not a link check,
and [ADR-0043](./0043-commit-tree.md) §3 is unchanged. Which half of a fork
applies is not: a cut overrides ADR-0043 §4's "the first is the one applied"
for the fork it names (below, and §9).

A cut can land after ops past it were applied: a replica received the
editor's later ops before the owner's revoke reached it. When the cut lands,
the replica **re-derives the shared Stream**: it rebuilds the Stream's
materialized entities from its retained log, with §3's checks read against
the attestations and cuts it now holds. The retained log is every op in
`ops`, applied and parked alike, and every envelope in `fork_evidence` at a
position the cut covers. An op past the cut is then un-materialized and
parked. Where the cut, or the chain below it, names the half of a fork that
this replica holds only as fork evidence (ADR-0043 §4), the two halves trade
places: the named half takes the `ops` row and is the one applied, and the
half that held it moves into `fork_evidence` and is not materialized. This
amends ADR-0043 §4 for that one case (§9). Without it, a replica that met the
halves in one order would show the named op and a replica that met them in
the other would show neither. The rebuild throws nothing away, so a later cut
that moves the frontier, or the withdrawal of a cut, re-derives again and
brings back what it now admits. A cut correction is lossless, as ADR-0034's
corollary 2 requires of one. No such rebuild exists in the engine today, and
§8 item 5 builds it.

A swap changes the op the chain root folds. ADR-0043 §5 folds
`root(d, n)` from the op `ops` holds at `n` and stores it on that op as
`ops.chain_root`, so once the halves at seq `k` trade places, every root
stored for that device from `k` up is over the half that left. The rebuild
recomputes them: for the editor's device whose halves traded, it refolds
`root(d, m)` for every `m` from `k` to the end of that device's contiguous
prefix, applied and parked ops alike, and stores each on its op. No other
device's roots move. The root then describes the op set the replica applies,
so two replicas that hold the same ops and the same cut hold the same roots,
in whichever order the halves reached them. Leaving the stale root would make
the digest disagree forever between replicas whose Streams are equal, and
record a `chain_divergence` no later agreement could clear. This amends
ADR-0043 §5's "changes nothing already folded" for this one case (§9). The
cost is one hash step per op in that prefix from `k`, paid once per swap.

A `StreamDigest` published before the swap, over the old half, is not
withdrawn or rewritten: it is a signed op in the log, and history is never
rewritten (ADR-0043 §7). It stays true as a record of what its writer held
then. A replica that has swapped reads it as ADR-0043 §5 reads any frontier:
where its own `root(d, n)` differs, it records a `chain_divergence` naming the
editor's device, the publisher and `n`. That is honest evidence, because the
publisher did hold the other half at `n`. It clears as §5 clears any
divergence, at the publisher's first later digest that agrees at or above it.
The publisher recomputes its own roots when the cut and the named half both
reach it, and the cut is an op entering the log, so §5's cadence makes a
digest of the publisher's due within a day of its last. Until one agrees, the
divergence stands and `Engine::chain_integrity` counts it. A replica that
holds only the half the cut does not name keeps its root over that half,
and disagrees with its peers until the named half reaches it, which is the
fork it really holds.

Expiry ends an editor's writes the same way. When a device of the owner's sees
an editor grant expire at its own clock, it emits the same revoke with its
cut. Until that cut lands, the editor's writes apply. The recipient's own
`expires_at` check on what it reads is unchanged.

The cost is honest ops that were in flight. An op the editor wrote before the
revocation, but that the owner's device had not yet received when it cut, is
outside the cut and never applies. An op a replica had already shown, but
that is past the cut, disappears from it when the cut lands. That is the price
of a cut the editor cannot move, and the revoke UI says both.

### 5. Editors merge like the owner, with authorship kept

An editor's op that applies merges like any other: per-field ops
([ADR-0044](./0044-per-field-ops.md)) ordered by HLC
([ADR-0016](./0016-hlc-timestamps.md)). It is not the entity-level LWW that
§Edits by editors once cited from the superseded ADR-0014. The op's
`device_id` names its author, and (a) ties that device to the editor
identity. There is no review or merge-request mode.

### 6. The relay routes an editor's writes, and decides nothing about them

When this is built, the relay gains a write route into the owner's channel for
a caller whose account holds a relay grant in force on that Stream, registered
with the role `editor`. The owner states the role when registering, just as it
states the expiry, because the relay cannot read `ShareGrantPayload`. That
check is hygiene against a third account filling the owner's channel, and
nothing more. A hostile relay can put any envelope into any channel today, and
§3 parks, unapplied, whatever it adds that no editor wrote. This ends ADR-0074 §3's "a
viewer cannot write, by construction" for editor grants only. A viewer grant
keeps no write route.

### 7. The wire carries the role it already has

`editor` is the text in field 4, under the HPKE AAD and the granter's
signature like `viewer`. Adding it adds no field. The re-seal a rotation emits
carries each recipient's role unchanged, so `ShareRecipient` gains a role when
this is built. Today it can only mean `viewer`, and a rotation must never turn
an editor into a viewer or a viewer into an editor.

### 8. Nothing of it is built until all of it is

A partial editor is a viewer that can write past a boundary that does not
exist yet. So the decode stays as it is (`UnsupportedRole` for `editor`), and
no build seals an editor grant, until every one of these is built and tested
together:

1. **Writer certs travel in the Stream.** Every device that writes into a
   shared Stream publishes there the `DeviceCert` chain a non-member replica
   needs to verify it. This gap is not the editor's alone. Today a recipient
   holds no cert for the owner's devices either, and a snapshot carrying their
   ops fails as `EngineError::UnknownDevice`.
2. **The in-Stream attestation** of §3(b), and its cut (§4).
3. **`share_revoke` with the frontier cut**, which the viewer's revocation
   also still lacks.
4. **The replica-side check** of §3: the `editor_authority` parking reason,
   and the retry of a Stream's parked editor ops when an attestation, a cut or
   a writer cert lands in it. Tested for each of (a) to (d) and each item of
   §2.
5. **The shared Stream's re-derivation** of §4: a rebuild of one Stream's
   materialized entities from its retained log and the `fork_evidence` the
   cut covers, run when a cut lands, which un-materializes ops past it and
   swaps in the fork half the cut names, recomputing that device's
   `ops.chain_root` from the swapped seq up. Tested by delivering one log to
   two replicas in different orders, the cut before and after the ops past
   it, and the two halves of a fork in either order, and asserting the two
   Streams are equal and their stream digests at one frontier are equal.
6. **The relay's editor write route** of §6.
7. **A structural feature** (ADR-0045 §7), named when it is built and listed
   in `vault_requires` before any device seals an editor grant. Until then, no
   device of the owner's account can hold a grant that another of its devices
   cannot enforce.

### 9. What this amends, and what it leaves standing

§3 and §4 hold an entity op back on account of who signed it, take one back
out after a cut, and apply the fork half a cut names. Four records speak to
that, and each is read here rather than past.

- **[ADR-0034](./0034-revocation-bounds-reads-not-writes.md)** decides that
  no replica refuses an entity op on account of its sender's revocation, and
  that the effect converges because of it. This record amends it for one
  population: the devices of an editor identity, writing into a Stream they
  hold a grant on. For them, standing is checked at every replica, and a
  revocation cut bounds writes as well as reads. What ADR-0034 guards is kept.
  Delivery order is not an input (its corollary 1, and corollary 3's
  condition on any peer-side enforcement), because §3 derives standing from
  the retained log and retries a parked op when the evidence lands. A cut
  correction is lossless (corollary 2), because §4's rebuild throws nothing
  away. And no cursor stalls, which is the #82 dilemma, because a parked op
  counts as received. This is ADR-0034's alternative (c), refusal as a derived
  view, which that record names as the shape to reconsider. Every device of
  the owner's account, and every entity op outside a shared Stream, stays
  exactly as ADR-0034 decides.
- **[ADR-0041](./0041-peer-side-revocation-is-a-fold.md) §3** leaves entity
  writes ungated because the engine has no projection rebuild. That reason
  stands for every stream but a shared one. §8 item 5 builds a rebuild scoped
  to one shared Stream, and editor ops are gated there only once it exists.
  The fold, its gate and the read bound are untouched.
- **[ADR-0045](./0045-schema-identity-and-feature-gating.md) §4** says an op
  that verifies is applied or parked. That holds: an editor op that fails §3
  is parked. This record adds one parking reason, `editor_authority`, and one
  replay trigger beside `Core::open`'s: an attestation, a cut or a writer cert
  landing in the Stream retries the ops parked there for that reason. §4's
  rebuild adds the one path by which an applied op becomes parked. Parked ops
  stay in the digest and in snapshots, as §4 of that record requires.
- **[ADR-0043](./0043-commit-tree.md) §3** is not amended. Receivers still
  never refuse on a link: a gap, a fork or a legacy op from any writer applies
  or is recorded as that record says. What holds back a revoked editor's op
  past its cut, or off the chain the cut names, is §3(c), an authority check
  the cut makes.
- **[ADR-0043](./0043-commit-tree.md) §4** says that of two ops at one
  position both are kept, the first is the one applied, and the second is not
  materialized. This record amends the middle clause for one case: a fork by
  a revoked editor's device at a position its cut covers. There the half the
  cut names, or that lies on the chain below it, is the one applied, whichever
  arrived first, and §4's rebuild trades the two halves between `ops` and
  `fork_evidence` to make it so. ADR-0043 §4 kept the second half out because
  applying it would make a replica's state depend on which half it met first.
  The cut is the input that removes that dependence: it is signed by the
  owner, it names one half by `op_hash`, and every replica that holds the
  named half applies the same one. Both halves are still kept, the fork is
  still surfaced as `core.chain.fork`, and evidence still does not propagate,
  so a replica that never received the named half applies neither until it
  does. Every other fork, including any by the owner's devices or by an
  editor not yet cut, stays exactly as ADR-0043 §4 decides.
- **[ADR-0043](./0043-commit-tree.md) §5** says applying an op folds one
  step into its device's root, and a late arrival "changes nothing already
  folded". This record amends that for the same one case: when §4's rebuild
  trades the halves at seq `k`, it recomputes `ops.chain_root` for that
  device from `k` to the end of its prefix, so the root folds the half that
  is applied. A `StreamDigest` already published over the old half stays in
  the log unchanged, and a replica that has swapped records the
  `chain_divergence` it shows until the publisher's next digest agrees (§4).
  The root formula, the digest, the `StreamDigest` shape and every other
  device's roots are unchanged.

## Alternatives considered

- **Drop the editor role, and share read-only for good.** Rejected: the parity
  matrix ranks "view shared stream as editor" a MUST, the role field and its
  text values were reserved for this, and dropping the role would leave the
  design's one collaborative mode undecided rather than decided.
- **The owner is the only writer, and editors send proposals the owner's
  device re-emits.** Rejected: every edit would wait for an owner device to
  come online before any other recipient saw it, authorship would become the
  owner's, and §Edits by editors already rejected a merge-request model.
- **The relay authorizes each editor op.** Rejected for ADR-0074's reasons:
  the relay is one of the parties the boundary is drawn against, it cannot
  check an identity chain without the `sunrise-crypto` dependency it
  deliberately lacks, and a hostile relay would simply skip the check.
- **End an editor's authority at a time (`effective_at_ms`, or the grant's
  `expires_at`) compared with each op's HLC.** Rejected: the editor writes its
  own HLC and can backdate every later op below the cut.
- **Let the vault-meta `ShareGrant` op be the only record of who may edit.**
  Rejected: only the owner's devices can read it, so viewers and other editors
  would have no basis for applying or refusing an editor's op, and their
  replicas would diverge from the owner's.
- **Let the client UI keep editors inside the Stream.** Rejected: a modified
  client is the attacker. A rule that lives only in a UI fails no test when it
  is broken.
- **Pair the editor into the owner's account as a device.** Rejected: that
  would seal it every Stream's key and give it membership authority over the
  account.

## Consequences

- The `editor` role has a fixed meaning. Building it is §8 and nothing beyond.
  Any change to what an editor may do is a new ADR, not an implementation
  choice.
- ADR-0034, ADR-0041 §3, ADR-0043 §4 and §5, and ADR-0045 §4 each gain one scoped
  exception or addition for editor ops in a shared Stream, and keep
  everything else (§9).
  Each carries an `Amended by` line pointing here.
- The viewer stays exactly as built. `share_grant.rs` refuses `editor`, and its
  docs cite this ADR as the reason.
- Item 1 of §8 is also a precondition for any recipient, viewer or editor, to
  verify what it reads. The client work in
  [#1320](https://github.com/justin13888/Sunrise/issues/1320) meets it before
  either role renders a shared Stream.
- An editor's write leaves a mark on the relay: the relay learns which account
  wrote into which other account's Stream, and when. That adds to the sharing
  graph [`sharing-with-others.md`](../03-crypto/sharing-with-others.md)
  §Privacy implications already concedes.

## Revisit if

1. **A role other than `viewer` and `editor` is wanted.** A commenter, or an
   editor limited to some entity families, is a new value in field 4 and a new
   ADR, decided against §3 rather than beside it.
2. **The commit tree needs an editor's devices to publish `StreamDigest`
   control ops** (ADR-0043). §2 forbids every control op to an editor, so a
   digest from one would need this ADR amended.
3. **Cross-relay sharing is designed.** §6's write route then lives on the
   owner's relay alone, and an editor on another relay has none.
