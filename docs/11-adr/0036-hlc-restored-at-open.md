# 0036 — The HLC is restored from the op log at open, not left to reset

**Status:** accepted

**Amends:** [ADR-0016](./0016-hlc-timestamps.md) — its "HLC state does not
survive a restart" concession in §What we give up is withdrawn, and the sentence
in §The three terms that calls a same-device tie across a restart the residual
`seq` covers is now the *whole* residual rather than an understatement of it.

**Depends on:** [ADR-0014](./0014-entity-level-lww-merge.md) — a merge rule that
compares `hlc` before anything else is what makes a same-device stamp inversion
lose data rather than merely look odd.

## Context

### The claim that did not hold

`MonotonicHlc` was built over `Hlc::default()` on every construction and primed
from nothing, and the comment on it read:

> State is deliberately **not persisted**. A restart resets the logical counter
> to 0, which is safe because the physical component dominates the ordering and
> only moves forward; the one case a reset can produce — two ops from this device
> sharing a `(physical_ms, logical)` pair across a restart — is what the `seq`
> term in the LWW tuple is there to break.

Every clause of that is about the logical counter, and `Hlc::default()` zeroes
the **physical** half as well. "The physical component … only moves forward" is
exactly the property a reset removes, so the justification argued for something
the code did not do.

### What the reset actually costs

A device's physical half is not its wall clock. `Hlc::receive` takes
`max3(local, received, now)` and accepts a peer up to `MAX_DRIFT_MS` — five
minutes — ahead of local time; `Hlc::send` carries the result forward. A device
that has absorbed one op from a peer whose clock leads therefore sits above its
own clock until the clock catches up, which is ordinary rather than adversarial:
`MAX_DRIFT_MS` exists precisely because honest devices disagree by minutes.

From a zero start after a restart, `send` returns roughly the wall clock — below
the stamps that device emitted moments earlier. That is **not** the tie ADR-0016
anticipated. `lww_wins` compares `hlc` before `device` and before `seq`, so:

* every replica that merges both of the device's ops keeps the **older** one,
  because the newer one's `hlc` is smaller and `seq` is never reached;
* the originating replica runs no LWW gate on its own writes, so it keeps the
  newer one.

Two replicas with the same op set, disagreeing permanently, with nothing
surfaced. A backgrounded mobile app killed and reopened is the whole setup.

`crates/sunrise-core/src/engine.rs`'s
`a_restart_does_not_make_this_device_emit_beneath_its_own_ops` is that scenario
end to end; with the restore removed it reports the peer keeping
`"edited before the restart"` after being handed both edits in order.

## Decision

**`Engine::prime_hlc` restores the clock at `Core::open` from `MAX(ts_ms)` over
`ops`, plus the logical half decoded from the envelopes at that millisecond.**

Two things make this cheap where ADR-0016 assumed it would not be.

**It is a restore, not a persist.** ADR-0016 rejected persistence because
"persisting it would need a durable write on the op path". Nothing is written
here. `ops.ts_ms` *is* the stamp's physical half already — `ops_insert_at` writes
`hlc.physical_ms` for a locally emitted op and `apply_remote_all` writes
`env.hlc.physical_ms` for an absorbed one — so the checkpoint the reset lost is
a column the op path has always maintained. The cost is one indexed aggregate
and a short decode at open, and nothing at all per op.

**The op log dominates every other durable stamp.** A materialized row's `lww_*`
columns and a `device_revocations` cut were both carried by an op, so priming
from `ops` covers them and no second source needs consulting.

The logical half is decoded rather than skipped because a device that emitted
`(t, 5)` before a restart and resumes at `(t, 0)` produces the same inversion in
the other half of the pair — one stalled millisecond and three ops is enough, no
peer required. That is
`priming_restores_the_logical_half_not_only_the_physical_one`.

`HlcClock::prime` is a required trait method rather than a defaulted one. A
defaulted no-op would let an implementor inherit the defect silently, and a
correctness property that depends on an implementor remembering is not a
property. It does not reuse `observe`: that refuses a reading beyond
`MAX_DRIFT_MS`, which is right for a peer's claim and wrong for this replica's
own history — a device whose clock was set back would have its own past refused
and resume beneath it — and it adds one to the logical counter, so reopening a
vault twice with no writes between would walk the clock upwards for nothing.

## Options rejected

**Leave it and correct the comment.** This was the honest alternative and it is
what the issue offered as its second branch. It fails on the evidence above: the
behaviour is not safe, so a comment describing it accurately would be describing
a defect. Rejected by the test, not by preference.

**Persist the HLC as it advances.** A durable row updated on every `send` and
`observe`. Correct, and it is the option ADR-0016 priced and declined. It buys
nothing over restoring at open — the log is already the checkpoint — and costs a
write on the hottest path in the engine.

**Prime from the materialized `lww_*` columns.** Reads no envelopes, so no
decode. Rejected because those columns cover entity rows only: a control op
stamps no entity, and a device whose most recent op was a `key_envelope` or a
`device_revoke` would resume below it.

## Consequences

* `HlcClock` gains a required method. `MonotonicHlc` is the only implementor in
  the workspace.
* `Core::open` gains one read before anything stamps an op. It runs before
  `ensure_base_epochs` and `publish_device_cert`, both of which emit.
* An `Engine` built directly — `Engine::from_clock`, which is what the unit
  tests use — is **not** primed unless its caller calls `prime_hlc`. That is a
  deliberate seam: a test that models a fresh replica wants a fresh clock. The
  production path is `Core::open` and it primes.
* The residual is now exactly what ADR-0016 described and no more: two ops from
  this device sharing a `(physical_ms, logical)` pair across a restart, when the
  log's greatest stamp is this device's own last one and the wall clock has not
  moved. `seq` breaks those.
* A vault written before this repairs itself the first time it opens; there is
  nothing to migrate, because the clock is derived rather than stored.

## What would force revisiting this

1. **A durable stamp that never passes through `ops`.** The premise here is that
   the log dominates. Anything that records an `Hlc` outside an op — a local
   cache, a sidecar index, a per-device high-water mark written by sync — makes
   `MAX(ts_ms)` an underestimate, and the fix is to widen `prime_hlc`, not to
   drop it.
2. **`ts_ms` ceasing to be the stamp's physical half.** It is written from
   `hlc.physical_ms` at both insertion sites today. If a caller ever writes a
   receipt time there instead, the restore silently primes from the wrong clock.
3. **The op log being truncated or compacted.** Priming from a log whose head
   has been pruned is fine; priming from one whose *tail* has been dropped is
   not. A retention policy that can delete the most recent op needs a stored
   watermark, which is the persist option above.
4. **A second `HlcClock` implementation reaching production.** The trait method
   is required so this cannot happen silently, but an implementation that
   ignores `prime` would reintroduce the defect exactly.
