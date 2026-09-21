# 0034 — Revocation bounds a device's reads, not its writes, and no replica refuses an op

**Status:** accepted

**Amends:** [`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md)
(§Revocation and the implementation-status bullet above it: the convergence
guarantee is stated, and the write bound is routed to the relay) and
[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md)
(A3's revocation mitigation says what revocation does and does not bound).

**Depends on:** [ADR-0024](./0024-key-hierarchy.md) — random wrapped stream keys
are what make a read bound expressible at all.

**Amended by:** [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) — corollary
3's reservation has been taken up, and **the amendment reaches this whole
record rather than an enumerated list of its sections**: wherever this file
speaks of *every* op, read it as speaking of **entity** writes, whose decision
is unchanged. §Decision and §"Why re-adding a peer-side refusal is not free"
carry dated scope notes saying what moved and what did not, and §Alternatives
(d) carries a third; those notes are where the detail sits, not the boundary of
where the amendment reaches. A section list in this field would have been read
as exhaustive for the whole file, and nothing in `docs/11-adr/` sanctions
reading one that way.

**Precedence:** where this record restates a rule that
[`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) or
[ADR-0041](./0041-peer-side-revocation-is-a-fold.md) states operationally,
those documents govern and this one is a summary of them. That is a bound on
future drift and **not** a licence to state a rule here more loosely than the
document it summarises: every rule stated in this record is written to hold as
written, and a summary that contradicts its source is a defect in this file
rather than a permitted simplification.

**Note, 2026-09-17 (citations only, at `e9a4c09`):** this file carried nine
code-span references at `e9a4c09`, and eight of them had rotted onto unrelated
code — the decision did not move, the code under it did. Seven of the eight
were `path:line` citations. The remaining one was a bare `` `:381` `` with no
path at all, leaning on the sentence before it; the gate's grammar rejects that
form as a citation and never counted it, so no amount of checking could have
caught that one, and it now carries its path. Each of the nine was repointed,
or left alone where it was already correct, and each now names the symbol it
means, `path:line#symbol`, which
[`.github/scripts/citation-gate.py`](../../.github/scripts/citation-gate.py)
checks for containment, so a line that drifts **out of the item it names** is a
red check rather than a silent lie
([#249](https://github.com/justin13888/Sunrise/issues/249)). Containment is not
aboutness, and it gets weaker the larger the item: measured against the eight,
the check catches five, and drift *within* a named item — three of these cite
into `apply_control_op`, which is over seven hundred lines — stays green. That
is a narrower hole than the line-existence check these citations rotted
through, not the absence of one. Claims about
the code were re-read against the code in the same pass: `Engine::is_revoked`
has one non-test caller and not two; `git grep refused_ops` returns hits rather
than nothing — every one of them a sentence in this file, this one included,
which is why the claim below is now written without a count; and the apply path
does reach the revocation register, by the chain the bullet below traces,
though no read of it decides whether an op applies. **No conclusion here
changed**, and every one of them was re-read against the code first.

## Context

### The question, and why the tree answers it differently than the issue asks

[#78](https://github.com/justin13888/Sunrise/issues/78) states that revocation
converges the **cut** but not the **effect**: two replicas with identical op sets
diverge permanently, because peer **B** applies a revoked device's op before the
revocation arrives while peer **C** meets the revocation first and refuses the op
into a `refused_ops` record it never revisits. It offers three options — (a)
retroactive un-materialization, (b) prospective-only refusal, written down, (c)
refusal as a derived view.

**That mechanism is not in the tree.** The refusal path was taken out. Read from
the code rather than from the issue:

- There is **no `refused_ops` table and no refusal record anywhere** —
  `git grep refused_ops` matches nothing outside this file: no migration, no
  query, no type, no record. Every hit it returns is a sentence in this ADR
  naming the thing in order to say it is gone, so the grep is not silent and
  the claim it is offered for still holds. Deliberately not stated as a
  number: the previous wording ("returns nothing across the whole tree") was
  false the day it was written, and a count here is falsified by the next
  sentence that mentions the table.
- Since migration 0028 the key-distribution side reads `device_read_bounds`
  rather than the register, so the register's own predicate `Engine::is_revoked`
  (`crates/sunrise-core/src/engine/revocation.rs:576#is_revoked`) has exactly
  **one** non-test caller and it is not on the apply path at all: the revoking
  command reads it to report whether its own row survived the fold
  (`crates/sunrise-core/src/engine/revocation.rs:257#revoke_device`). What the
  apply path reaches is the **bound**, twice, and both reads are on the
  **key-distribution** side: the anti-join is `emit_key_envelopes`'s
  `NOT EXISTS` against `device_read_bounds`
  (`crates/sunrise-core/src/engine/oplog.rs:309-311#emit_key_envelopes`), which
  is SQL and calls nothing, and the caller is the early return in
  `backfill_key_envelopes`
  (`crates/sunrise-core/src/engine/oplog.rs:418#backfill_key_envelopes`), which
  tested `is_revoked` until 0028 gave the bound its own table.
  The apply path does reach that early return, and inside a single
  transaction: `apply_remote_all` opens one
  (`crates/sunrise-core/src/engine/sync.rs:279#apply_remote_all`), routes a
  control op into `apply_control_op`
  (`crates/sunrise-core/src/engine/sync.rs:312#apply_remote_all`), and a
  published device cert carries it on into `backfill_key_envelopes`
  (`crates/sunrise-core/src/engine/sync.rs:968#apply_control_op`). What no
  read of either table decides is whether an op **applies**; it decides which
  device is sealed key material, and that is this whole decision in one
  sentence. An earlier draft of this bullet said nothing in the apply path
  consulted the register at all, which the call chain above falsifies.
- `apply_remote_all` says so at step b
  (`crates/sunrise-core/src/engine/sync.rs:200-201#apply_remote_all`): *"A
  revoked device's row is found here like any other, and its op is applied like
  any other."*
- `upsert_sync_cursor`'s doc
  (`crates/sunrise-core/src/engine/oplog.rs#upsert_sync_cursor`) records the
  removal directly: *"A refused op is **not** decided and does not appear here.
  It was, briefly."* Cited without a line on purpose — that paragraph is being
  rewritten, and a line number into it is a citation built to rot.
- The test `a_revoked_devices_ops_still_apply_at_the_replica`
  (`crates/sunrise-core/src/engine/tests.rs:7437-7439#a_revoked_devices_ops_still_apply_at_the_replica`)
  revokes a device at a cut before
  every op it writes — the strongest form of the premise — and asserts the op
  applies, materializes and is passed by the cursor.

So the divergence #78 describes **requires the refusal that no longer exists**.
With no replica refusing an **entity** op — and since
[ADR-0041](./0041-peer-side-revocation-is-a-fold.md) the two control ops it
gates are the only refusals in the tree — delivery order stops being an input to
the task table: every replica applies every entity op it can decrypt, and the
effect converges for the same reason every other op's effect converges. The question
#78 asks is therefore live but its premise has moved, and the honest answer is
not one of its three options.

### What is actually enforced, and what is not

Revocation today is a **register plus a read bound**:

- `device_revoke` is **recorded whatever its sender's standing**, in
  `device_revoke_ops`, and `device_revocations` is rebuilt from that ledger on
  every such op
  (`crates/sunrise-core/src/engine/revocation.rs:1251#apply_device_revoke`)
  rather than upserted into: the fold deletes the register outright and
  re-inserts the winners
  (`crates/sunrise-core/src/engine/revocation.rs:1225#refold_device_revocations`).
  It is still an LWW register on the op's own HLC with `revoked_by` as the
  tie-break, but that rule is now the fold's ascending walk — a later row simply
  overwriting an earlier one
  (`crates/sunrise-core/src/engine/revocation.rs:1056-1057#refold_device_revocations`).
  The guarded upsert this bullet described until
  [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) is gone, and both
  citations it carried had rotted onto identity-transition code inside
  `apply_control_op` — green under the containment check, which is the hole the
  note above predicts.
- A device may not move its own cut: the one edit the register never accepts
  from the party it is about. It is refused at ingest with a
  `core.device.revoke_refused` warning
  (`crates/sunrise-core/src/engine/revocation.rs:1292#apply_device_revoke`), and
  since ADR-0041 the same rule is held **again** in the fold
  (`crates/sunrise-core/src/engine/revocation.rs:1084#refold_device_revocations`),
  because the fold is the register's sole author and a rule enforced only on the
  way in would be absent for every row already in the ledger.
- The cut's `(cut_ms, cut_logical)` decides **which** revocation wins when two
  race. The **presence of the row** is the whole read test — there is no clock
  comparison in the path, and `is_revoked`'s own doc explains at length why a
  correct comparison is indistinguishable from presence and an incorrect one
  collapses to a bare wall clock after a restart, which `HlcClock::peek` makes
  easy to reach (`crates/sunrise-core/src/config.rs:71-79#peek`).
- **Entity writes are bounded at the relay, conditionally.** This bullet said
  until [#80](https://github.com/justin13888/Sunrise/issues/80) landed that
  nothing bounds writes and that `Command::RevokeDevice` *cannot* ask the relay
  for one, because `DELETE /api/v1/devices/{device_id}` names the **relay's**
  ULID for a device while a vault knows only its own 16-byte device id and no
  peer's relay id. That was the obstacle, and #80 removed it by adding the route
  that takes the id a vault has:
  `DELETE /api/v1/devices/by-vault-id/{vault_device_id}`
  (`crates/sunrise-server/src/api/devices.rs:310#revoke_by_vault_id`).
  `RevokeDevice` now inserts a `relay_revocation_intents` row in the op's own
  transaction, when the fold finds the revocation effective
  (`crates/sunrise-core/src/engine/revocation.rs:311-318#revoke_device`), and
  `sync_driver::drain_relay_revocations` retries it on every session
  (`crates/sunrise-core/src/sync_driver.rs:1466#drain_relay_revocations`). The
  bound is real and **conditional**: the relay enforces only against a
  device-signed request, and `require_device_sig` defaults to false, so in the
  default deployment it is not in force
  ([ADR-0041 §#80 landed, read out of the
  tree](./0041-peer-side-revocation-is-a-fold.md#80-landed-read-out-of-the-tree)).
- **Two control ops are refused at the peer; entity writes are not.** A
  `device_revoke` whose sender the ledger revokes is skipped by the fold —
  **unless the only party to have revoked that sender is the very device the
  row is about**, which is the gate's one exception and keeps two devices
  revoking each other converging on *both* revocations instead of letting a
  back-dated op silence its target
  (`crates/sunrise-core/src/engine/revocation.rs:1106-1116#refold_device_revocations`),
  one naming its own sender is refused at ingest
  (`crates/sunrise-core/src/engine/revocation.rs:1292#apply_device_revoke`), and
  a read-bounded sender's third-party `key_envelope` recipient claim is not
  recorded (`crates/sunrise-core/src/engine/sync.rs:705#apply_control_op`). That
  is [ADR-0041](./0041-peer-side-revocation-is-a-fold.md), and it reaches no
  entity write.

### Why re-adding a peer-side refusal is not free

**Scope, 2026-09-20.** This section priced re-adding peer-side refusal as a
dilemma with no third horn. It **has been re-added**, and under a shape that
takes neither: [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) gates the
two control ops whose effect this engine can re-derive by deriving the register
from a kept ledger, so nothing turns on a cursor and no op is refused where it
lands. What survives below is the pricing for **entity** writes, and it is live
rather than historical — ADR-0041 §3 cites ADR-0034 when it declines to gate
them ([ADR-0041 §3. What is deliberately not
gated](./0041-peer-side-revocation-is-a-fold.md#3-what-is-deliberately-not-gated)),
and its alternative **(b)** rejects the first of these two horns "for the reason
ADR-0034 rejects it" ([ADR-0041
§Alternatives considered](./0041-peer-side-revocation-is-a-fold.md#alternatives-considered),
under (b)). Alternative (c) rejects the second horn on its own grounds — an
unbounded stall, `evicted_through` passing the frozen cursor, `mark_degraded()`
latching — and does not cite this record, which is worth saying because §3's
stated reason is the no-projection-rebuild argument, and that argument is
Alternative (a) below rather than either horn. The first horn's
mechanism is rewritten against the tree; its conclusion is unchanged, and now
rests on a different reason than the one it was written for.

[#82](https://github.com/justin13888/Sunrise/issues/82) records the dilemma that
took five review rounds to bottom out, and it is not about convergence:

- **Advance the cursor past a refused op** and the refusal is permanent. Not
  because the cut cannot move. It can, in both directions, and always could: the
  register is not a ratchet and was never a `MIN`, the fold walks the ledger
  ascending so a later row overwrites an earlier one
  (`crates/sunrise-core/src/engine/revocation.rs:1056-1057#refold_device_revocations`),
  a cut that landed wrong is corrected by revoking again from a healthy device
  (`crates/sunrise-core/src/engine/revocation.rs:1269-1275#apply_device_revoke`),
  and since ADR-0041 a re-fold can lower a cut or drop the row outright, because
  the register is a pure function of the op set rather than something edited in
  place
  (`crates/sunrise-core/src/engine/revocation.rs:1120-1126#refold_device_revocations`).
  **None of that reaches an op the cursor has already passed.** A correction
  changes what is sealed *next*; it brings nothing back. The loss is permanent
  because the correction is forward-only, not because the cut is — which is the
  same property corollary 2 relies on from the other side.

  The tree has since gained a second and stronger reason, and it is the one to
  reach for first. For the ops ADR-0041 *does* gate, **the gate reads no cut at
  all**: `revokers_all`, the discount pass and the walk's condition are built
  from `(sender, revoked)` pairs and nothing else, and the HLC decides only
  which row wins the register
  (`crates/sunrise-core/src/engine/revocation.rs:877-889#refold_device_revocations`).
  A correction therefore leaves the gate's answer exactly as it was, whichever
  direction it moves the cut. Recoverability was never a function of the
  register's direction; it is a function of whether anything was thrown away,
  and under the fold nothing is — the skipped op stays in the ledger.
- **Do not advance it** and the cursor freezes while the relay keeps accepting
  the revoked device's uploads. Within retention, eviction raises
  `evicted_through` past the frozen cursor, `relay_replay`'s gap loop fires on
  every subscribe, and `mark_degraded()` latches. Within 30 days of an ordinary
  revocation, every device in the account permanently reports data loss as the
  expected outcome of an administrative action.
  [#80](https://github.com/justin13888/Sunrise/issues/80) has since landed and
  takes most of this horn's premise away **where the relay bound is in force**,
  and none of it in the default deployment, where `require_device_sig` is false
  and no device is resolved to check ([ADR-0041 §#80 landed, read out of the
  tree](./0041-peer-side-revocation-is-a-fold.md#80-landed-read-out-of-the-tree)).

Both halves are correct about their own case, which is the tell that the choice
is wrong rather than the implementation — for the family where both still apply,
which is now entity writes alone.

## Decision

**Revocation bounds a revoked device's reads. It does not bound its writes, no
replica refuses an op on account of its sender's revocation, and the effect
converges precisely because of that.**

**Scope, 2026-09-20.** As written that is a claim about *every* op, and
[ADR-0041](./0041-peer-side-revocation-is-a-fold.md) stopped it being one. Read
it as a claim about **entity** writes — a task edit, a note, a focus session —
which is the scope ADR-0041 preserves and says it preserves ([ADR-0041 §3. What
is deliberately not
gated](./0041-peer-side-revocation-is-a-fold.md#3-what-is-deliberately-not-gated);
its header says the same at
`docs/11-adr/0041-peer-side-revocation-is-a-fold.md:9-11`, which sits above
every section heading in that file and so is cited by line, unanchored). The
decision itself has not changed for that family. What changed is its reach: two
**control** ops are refused at the peer, listed in §"What is actually enforced,
and what is not" above, so "no replica refuses an op" is false read across the
whole op set and true read across entity writes.

**Corollary 3 is met for one of those two gates and not the other, and the
difference is worth carrying rather than rounding off.** The `device_revoke`
gate derives `device_revocations` from a kept ledger rather than refusing an op
where it lands, and it reads no cut at all, so no replica's delivery order
decides who is revoked. The recipient-claim gate does not have that property: it
reads `device_read_bounds`, a ratchet over the registers *this* replica computed
along its own arrival order rather than a function of the op set — "a derivation
this table does not have"
(`crates/sunrise-core/src/engine/revocation.rs:1202-1210#refold_device_revocations`),
a predicate that "does not converge across replicas"
([`key-rotation.md` §Revocation](../03-crypto/key-rotation.md#revocation)), and
a concession ADR-0041 makes for itself ([ADR-0041 §2. A read-bounded device's
third-party `key_envelope` claim is not
recorded](./0041-peer-side-revocation-is-a-fold.md#2-a-read-bounded-devices-third-party-key_envelope-claim-is-not-recorded)).
It can afford that because declining a row can only cause *more* key
distribution and never less, which is not a general licence;
[#282](https://github.com/justin13888/Sunrise/issues/282) is the open question
of what a converging derivation would be. Corollary 3's *prediction* also did
not hold — it expected peer-side enforcement as defence in depth "not as the
only line", and with `require_device_sig` at its default it is the only line
there is — which is recorded at revisit trigger 1 below.

The guarantee, stated positively and in the terms a reader of
`key-rotation.md` needs:

> Once a replica has applied a `device_revoke` **whose sender was still
> ungated there**, it seals the revoked device no key envelope for any epoch
> minted at or after the cut, so the device can read nothing written after it.
> A row the fold gated bounds nobody: `device_read_bounds` is written only from
> the fold's surviving register
> (`crates/sunrise-core/src/engine/revocation.rs:1216-1224#refold_device_revocations`)
> and a gated row never reaches it
> (`crates/sunrise-core/src/engine/revocation.rs:1106-1116#refold_device_revocations`).
> Every replica applies every **entity** op it can decrypt, whatever its
> sender's revocation state and whatever order the
> `device_revoke` and the op arrive in, so two replicas holding the same op set
> hold the same task table. Revocation is **eventually consistent and
> forward-only on reads, and is not a write bound on entity ops at all**: what
> stops a revoked device writing those is the relay refusing its uploads, which
> **is** built ([#80](https://github.com/justin13888/Sunrise/issues/80), closed
> 2026-09-08) and is in force only where `require_device_sig` is true, which is
> not the default. Its two **control** writes named above are refused at the
> peer as well ([ADR-0041](./0041-peer-side-revocation-is-a-fold.md)) — with
> the gate's one exception and the discount pass's two residuals, which that
> record states in §"What a user sees when an op is refused" item 4 and this
> one does not restate, because no sentence of the form "it can no longer
> revoke another device" holds for every ledger.

Three corollaries, recorded so they are not rediscovered:

1. **Delivery order is not an input to the materialized state.** The meta stream
   and the task streams have independent delivery and always will; the design
   answer is that neither gates the other, not that they be ordered.
2. **A cut correction is lossless for entity writes.** Because nothing a user
   typed was refused under the old cut, a revocation re-issued from a healthy
   device changes what is *sealed next* and destroys nothing that was already
   applied. This is what makes the LWW register safe to move in both directions.
   **The premise is entity-scoped since
   [ADR-0041](./0041-peer-side-revocation-is-a-fold.md)**: the two control ops
   it gates *are* refused, and a correction does not un-skip one, because the
   gate reads no cut at all and a correction leaves its answer exactly as it was
   (`crates/sunrise-core/src/engine/revocation.rs:877-889#refold_device_revocations`).
   The remedy there is to revoke again from a device the account still trusts,
   which is tolerable for an administrative act and would not be for a task
   edit.
3. **Peer-side enforcement, when it is built, must not reintroduce order
   dependence.** It arrives after the relay bound
   ([#82](https://github.com/justin13888/Sunrise/issues/82) depends on
   [#80](https://github.com/justin13888/Sunrise/issues/80)) as defence in depth
   over ops that should not have been accepted in the first place — not as the
   only line, and not as a decision each replica takes for itself out of its own
   delivery order.

## Alternatives considered

**(a) A revocation re-examines already-applied ops.** Rejected for v1. It is the
only option that would converge an *effect* that is currently not divergent, so
it buys nothing here — the divergence it answers went away with the refusal. Its
cost is unchanged and large: a retroactive projection rebuild scoped to one
device's ops after one timestamp, which this engine has no machinery for, and a
UI story for applied, visible user data vanishing when a revocation lands.
Combined with corollary 2, it is strictly worse: it makes a cut correction
destructive in one direction, which is exactly what the move away from
"earliest cut wins" was for.

**(b) Prospective-only refusal, written down.** Rejected — and this is the option
#78 suggests. It is a description of the code as it was *before* the refusal was
removed, not as it is. Taking it would mean re-adding `refused_ops`, and with it
both the permanent divergence #78 itself reports and the cursor dilemma #82
records. "Cheap and honest" was true of it when the refusal existed; today it is
neither, because the refusal is the expensive part.

**(c) Refusal as a derived view.** Rejected for v1 as the most principled and the
most work, which is #78's own assessment. It needs a different answer to "how
does the cursor pass an op that will never apply" — and #82 shows that both
available answers fail: advancing makes a reversible decision irreversible, not
advancing latches a data-loss warning on every device. Deriving refusal at read
time does not dissolve that; it moves it from the op log to the projection, which
this engine also does not have. If peer-side refusal of **entity** ops is ever
wanted, (c) is the shape to reconsider, because where the relay bound is in
force there are no ops to refuse in the ordinary case and the cursor question
stops being hot. The relay bound itself has landed
([#80](https://github.com/justin13888/Sunrise/issues/80), closed 2026-09-08);
what stays unbuilt, and is what (c) is about, is refusal of an **entity** op at
the peer.

**(d) No peer-side refusal at all. Chosen.**

**Scope, 2026-09-20.** "At all" was true when this was written and is not now.
[ADR-0041](./0041-peer-side-revocation-is-a-fold.md) refuses two **control**
ops at the peer — the two listed in §"What is actually enforced, and what is
not" above. Read what follows as the choice for **entity** writes, which is the
family it is still the choice for, and which is the family ADR-0041 declines to
gate ([ADR-0041 §3. What is deliberately not
gated](./0041-peer-side-revocation-is-a-fold.md#3-what-is-deliberately-not-gated)).

It is what the tree does for entity writes, it is the only option under which
the effect converges without a projection rebuild, and it is the only one that
keeps a cut correction lossless. Its cost is stated rather than hidden: a
revoked device that keeps its relay credentials goes on writing entity ops, and
every replica accepts that work. For the lost-device case revocation is written
for, those writes predate the user noticing; for a hostile device they do not,
and that is what the relay bound is for.

## Consequences

- **`key-rotation.md` §Revocation has gained the convergence guarantee, and
  this record is what asked for it.** When this bullet was written that file
  said reads are bounded and writes are not, and did not say the *effect*
  converges. Both clauses have since stopped being true of it: §Implementation
  status now states the relay write bound with its three conditions, and
  §Revocation carries the convergence guarantee outright, under the heading
  "What converges, and what does not"
  ([`key-rotation.md` §Revocation](../03-crypto/key-rotation.md#revocation)).
  That is the property a reader who has met #78 will be looking for, and it is
  where they will now find it.
- **#78 closes without code.** The mechanism it reports was removed by the change
  that closed [#76](https://github.com/justin13888/Sunrise/issues/76); what
  remained open was whether the resulting behaviour is the intended end state.
  It is, for v1.
- **#80 and #82 kept their scope and gained a reason.** #80 was the write
  bound; #82 was peer-side defence in depth *after* it. Neither was a
  convergence fix, and filing them as one is what this record prevented. Both
  have since closed, #80 on 2026-09-08 and #82 through
  [ADR-0041](./0041-peer-side-revocation-is-a-fold.md), and the ordering this
  bullet asked for is the one they landed in.
- **The unmitigated bypass is untouched and stays named.** A revoked **creator**
  still holds `ID_S_priv` and can self-certify a fresh device id
  ([#105](https://github.com/justin13888/Sunrise/issues/105)); nothing here
  narrows that. The scope is the creator's alone, and `key-rotation.md`
  §Revocation is where it is stated rather than here: a revoked device holds no
  signing key unless it is the creator, and a revocation run from any other
  device leaves the identity where it is. That file records #105 as closed
  outright for a device admitted by pairing, which is the half this bullet is
  not about.
- **No code changes.** The test doc at
  `crates/sunrise-core/src/engine/tests.rs:7397#a_revoked_devices_ops_still_apply_at_the_replica`
  and `apply_remote_all`'s step b gain
  a citation of this ADR in place of a bare issue number, so the next reader
  finds a decision rather than an open question.

## What would force revisiting this

1. **[#80](https://github.com/justin13888/Sunrise/issues/80) landing.**
   **Fired, and closed out.** #80 landed, #82 was re-entered through it as this
   trigger directed, and
   [#82](https://github.com/justin13888/Sunrise/issues/82) is now closed by
   [ADR-0041](./0041-peer-side-revocation-is-a-fold.md). The premise this
   trigger named held only in part: there are no ops to refuse in the ordinary
   case and the cursor cannot freeze against a climbing `max_seq` **where the
   relay bound is in force**, and the three conditions on that are narrow enough
   that peer-side enforcement did not turn out to be "cheap defence in depth" —
   with `require_device_sig` at its default it is the only line there is, which
   is what ADR-0041 §Decision had to answer rather than this file. The re-entry
   happened where this trigger sent it, so this file is amended by that record
   rather than edited into it.
2. **A projection rebuild appearing in the engine.** Option (a) is rejected on
   the strength of there being no way to un-materialize one device's ops after a
   timestamp. If something else builds that — undo, a repair path, a
   selective-replay tool — the cost of retroactive revocation collapses and the
   trade should be re-taken.
3. **Revocation being presented to a user as a security control that stops
   writes.** A screen that says "this device can no longer make changes" is
   wrong: #80 has landed and is the bound for **entity** writes, but only where
   `require_device_sig` is true, and the peer-side gate
   [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) added reaches two
   control ops and none of that family.

   **This record sanctions no screen copy about what a revoked device may still
   revoke.** Three attempts at one have stood here and each was falsified by a
   guard the one before it had not met: the `effective` guard in `revoke_device`
   (`crates/sunrise-core/src/engine/revocation.rs:257#revoke_device`), which
   withholds the cut where the local fold discards the op; then the fold's one
   exception; then the **discount pass**
   (`crates/sunrise-core/src/engine/revocation.rs:1040-1054#refold_device_revocations`),
   which drops a revoker `S` out of `V`'s set whenever the ledger holds a row
   revoking `S` from a sender that is not `V`. Three links of that — `O` revokes
   `X`, `P` revokes `O`, `Q` revokes `P` — leave `X` **on the revoked list while
   being ungated**, which is the pair of facts the gate exists to keep apart,
   and `X` revokes third parties on every replica. The residual is stated
   operationally, with the tests that pin it, at [ADR-0041 §What a user sees
   when an op is
   refused](./0041-peer-side-revocation-is-a-fold.md#what-a-user-sees-when-an-op-is-refused),
   item 4, and that text governs; what may be told to a user is held open by
   [#248](https://github.com/justin13888/Sunrise/issues/248) and
   [#252](https://github.com/justin13888/Sunrise/issues/252), and #252 is this
   defect, already filed. What binds the client copy is #241's remit and not
   this trigger. **The trigger itself stands unchanged**: revocation presented
   as a control that stops writes is a reason to revisit this decision, whatever
   a screen is eventually allowed to say.
4. **A second control op growing an order-dependent effect.** Corollary 1 is a
   property of the whole control-op family, not of `device_revoke` alone. The
   first op whose effect depends on which stream drained first reopens the
   general question this record settles for revocation.
