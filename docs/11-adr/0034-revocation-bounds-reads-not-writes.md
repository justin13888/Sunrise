# 0034 — Revocation bounds a device's reads, not its writes, and no replica refuses an op

**Status:** accepted

**Amends:** [`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md)
(§Revocation and the implementation-status bullet above it: the convergence
guarantee is stated, and the write bound is routed to the relay) and
[`../01-architecture/threat-model.md`](../01-architecture/threat-model.md)
(A3's revocation mitigation says what revocation does and does not bound).

**Depends on:** [ADR-0024](./0024-key-hierarchy.md) — random wrapped stream keys
are what make a read bound expressible at all.

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
  `git grep refused_ops` returns nothing across the whole tree.
- `Engine::is_revoked` (`crates/sunrise-core/src/engine.rs:975`) has exactly two
  non-test callers, and both are on the **key-distribution** side:
  `emit_key_envelopes`'s anti-join against `device_revocations`
  (`crates/sunrise-core/src/engine.rs:3899`) and the early return in
  `backfill_key_envelopes` (`:3995`). Nothing in the apply path consults it.
- `apply_remote_all` says so at step b
  (`crates/sunrise-core/src/engine.rs:750-758`): *"A revoked device's row is
  found here like any other, and its op is applied like any other."*
- `upsert_sync_cursor`'s doc (`crates/sunrise-core/src/engine.rs:4187`) records
  the removal directly: *"A refused op is **not** decided and does not appear
  here. It was, briefly."*
- The test `a_revoked_devices_ops_still_apply_at_the_replica`
  (`crates/sunrise-core/src/engine.rs:12650`) revokes a device at a cut before
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

- `device_revoke` writes `device_revocations`, an LWW register on the op's own
  HLC with `revoked_by` as the tie-break, and a device may not move its own cut
  (`crates/sunrise-core/src/engine.rs:1281-1330`).
- The cut's `(cut_ms, cut_logical)` decides **which** revocation wins when two
  race. The **presence of the row** is the whole read test — there is no clock
  comparison in the path, and `is_revoked`'s own doc explains at length why a
  correct comparison is indistinguishable from presence and an incorrect one
  collapses to a bare wall clock after a restart, which `HlcClock::peek` makes
  easy to reach (`crates/sunrise-core/src/config.rs:52-62`).
- Nothing bounds writes. `Command::RevokeDevice` makes no request of the relay,
  and cannot: `DELETE /api/v1/devices/{device_id}` names the **relay's** ULID for
  a device, minted at registration, while a vault knows only its own 16-byte
  device id and no peer's relay id
  ([#80](https://github.com/justin13888/Sunrise/issues/80)).

### Why re-adding a peer-side refusal is not free

[#82](https://github.com/justin13888/Sunrise/issues/82) records the dilemma that
took five review rounds to bottom out, and it is not about convergence:

- **Advance the cursor past a refused op** and the refusal is permanent. The cut
  is an LWW register that moves in *both* directions — deliberately, so a bad cut
  from a slow clock can be corrected — so a correction can never bring the
  refused work back. That is loss of real user work.
- **Do not advance it** and the cursor freezes while the relay keeps accepting
  the revoked device's uploads. Within retention, eviction raises
  `evicted_through` past the frozen cursor, `relay_replay`'s gap loop fires on
  every subscribe, and `mark_degraded()` latches. Within 30 days of an ordinary
  revocation, every device in the account permanently reports data loss as the
  expected outcome of an administrative action.

Both halves are correct about their own case, which is the tell that the choice
is wrong rather than the implementation.

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
  `crates/sunrise-core/src/engine.rs:12630` and `apply_remote_all`'s step b gain
  a citation of this ADR in place of a bare issue number, so the next reader
  finds a decision rather than an open question.

## What would force revisiting this

1. **[#80](https://github.com/justin13888/Sunrise/issues/80) landing.** A relay
   that refuses a revoked device's uploads changes every premise here: there are
   no ops to refuse in the ordinary case, the cursor cannot freeze against a
   climbing `max_seq`, and peer-side enforcement becomes cheap defence in depth.
   #82 is the re-entry point, not an edit to this file.
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
