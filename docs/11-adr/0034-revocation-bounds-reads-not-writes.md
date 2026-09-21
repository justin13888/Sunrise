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
3's reservation has been taken up, and what stopped holding is §"Why re-adding
a peer-side refusal is not free" as a statement about *every* op. Peer-side
enforcement now exists for the two **control** ops whose effect this engine can
re-derive, and it meets corollary 3 by **deriving the register** from a kept
ledger rather than by refusing an op where it lands, so no replica's delivery
order decides anything. Corollary 3's *prediction* did not hold, and that is
recorded at revisit trigger 1 below rather than left for a reader to notice: it
expected peer-side enforcement to arrive as defence in depth "not as the only
line", and with `require_device_sig` at its default the relay bound is not in
force, so it is the only line there is. The decision below about a revoked
device's **entity**
writes — a task edit, a note, a focus session — is unchanged; ADR-0041 says so
itself (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md:9-11`), and it is
now load-bearing in a narrower place.

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
With no replica refusing anything, delivery order stops being an input to the
task table: every replica applies every op it can decrypt, and the effect
converges for the same reason every other op's effect converges. The question
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
- Nothing bounds writes. `Command::RevokeDevice` makes no request of the relay,
  and cannot: `DELETE /api/v1/devices/{device_id}` names the **relay's** ULID for
  a device, minted at registration, while a vault knows only its own 16-byte
  device id and no peer's relay id
  ([#80](https://github.com/justin13888/Sunrise/issues/80)).

### Why re-adding a peer-side refusal is not free

**Scope, 2026-09-20.** This section priced re-adding peer-side refusal as a
dilemma with no third horn. It **has been re-added**, and under a shape that
takes neither: [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) gates the
two control ops whose effect this engine can re-derive by deriving the register
from a kept ledger, so nothing turns on a cursor and no op is refused where it
lands. What survives below is the pricing for **entity** writes, and it is live
rather than historical — ADR-0041 §3 rests on it when it declines to gate them
(`docs/11-adr/0041-peer-side-revocation-is-a-fold.md:244-249`), and ADR-0041's
alternatives (b) and (c) reject exactly these two horns "for the reason ADR-0034
rejects it"
(`docs/11-adr/0041-peer-side-revocation-is-a-fold.md:468-477`). The first horn's
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
  and no device is resolved to check
  (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md:53-59`).

Both halves are correct about their own case, which is the tell that the choice
is wrong rather than the implementation — for the family where both still apply,
which is now entity writes alone.

## Decision

**Revocation bounds a revoked device's reads. It does not bound its writes, no
replica refuses an op on account of its sender's revocation, and the effect
converges precisely because of that.**

The guarantee, stated positively and in the terms a reader of
`key-rotation.md` needs:

> Once a replica has applied a `device_revoke`, it seals the revoked device no
> key envelope for any epoch minted at or after the cut, so the device can read
> nothing written after it. Every replica applies every op it can decrypt,
> whatever its sender's revocation state and whatever order the `device_revoke`
> and the op arrive in, so two replicas holding the same op set hold the same
> task table. Revocation is **eventually consistent and forward-only on reads,
> and is not a write bound at all**: what stops a revoked device writing is the
> relay refusing its uploads, which is not built
> ([#80](https://github.com/justin13888/Sunrise/issues/80)).

Three corollaries, recorded so they are not rediscovered:

1. **Delivery order is not an input to the materialized state.** The meta stream
   and the task streams have independent delivery and always will; the design
   answer is that neither gates the other, not that they be ordered.
2. **A cut correction is lossless.** Because nothing was refused under the old
   cut, a revocation re-issued from a healthy device changes what is *sealed
   next* and destroys nothing that was already applied. This is what makes the
   LWW register safe to move in both directions.
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
this engine also does not have. If peer-side enforcement is ever wanted *after*
the relay bound lands, (c) is the shape to reconsider, because by then there are
no ops to refuse in the ordinary case and the cursor question stops being hot.

**(d) No peer-side refusal at all. Chosen.** It is what the tree does, it is the
only option under which the effect converges without a projection rebuild, and it
is the only one that keeps a cut correction lossless. Its cost is stated rather
than hidden: a revoked device that keeps its relay credentials goes on writing,
and every replica accepts that work. For the lost-device case revocation is
written for, those writes predate the user noticing; for a hostile device they do
not, and that is what the relay bound is for.

## Consequences

- **`key-rotation.md` §Revocation gains the convergence guarantee.** It already
  says reads are bounded and writes are not; what it does not say is that the
  *effect* converges, which is the property a reader who has met #78 will be
  looking for.
- **#78 closes without code.** The mechanism it reports was removed by the change
  that closed [#76](https://github.com/justin13888/Sunrise/issues/76); what
  remained open was whether the resulting behaviour is the intended end state.
  It is, for v1.
- **#80 and #82 keep their scope and gain a reason.** #80 is the write bound;
  #82 is peer-side defence in depth *after* it. Neither is a convergence fix,
  and filing them as one is what this record prevents.
- **The unmitigated bypass is untouched and stays named.** A revoked device still
  holds `ID_S_priv` and can self-certify a fresh device id
  ([#105](https://github.com/justin13888/Sunrise/issues/105)); nothing here
  narrows that, and `key-rotation.md` already states it as unmitigated.
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
   writes.** The guarantee above is what the UI may promise. A screen that says
   "this device can no longer make changes" makes this ADR wrong rather than the
   screen, and the fix is #80, not a peer-side gate.
4. **A second control op growing an order-dependent effect.** Corollary 1 is a
   property of the whole control-op family, not of `device_revoke` alone. The
   first op whose effect depends on which stream drained first reopens the
   general question this record settles for revocation.
