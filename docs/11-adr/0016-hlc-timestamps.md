# 0016 — Hybrid logical clocks order writes; `(hlc, device_id, seq)` is the LWW key

**Status:** accepted

**Amends:** [ADR-0014 — Entity-level LWW in SQLite is the v1 merge model](./0014-entity-level-lww-merge.md),
which named `(ts_ms, device_id)` as the comparison key and recorded the skewed-clock
defect below as "this ADR's problem rather than an accident". This is the fix.

**Closes:** issue #21.

## Context

`lww_wins` compared `envelope.ts_ms` — an unbounded reading of the writing
device's system clock — and broke ties with a memcmp on the raw device id. Two
things follow from that, and the second is much worse than the first.

**A wall clock cannot order two devices.** It is not monotonic (NTP steps it
backwards, users set it by hand, VMs resume with a stale one), and two devices
never agree on it. Within a single millisecond it cannot order anything at all,
which is not a corner case: creating a task and immediately patching it lands
both ops in one millisecond routinely.

**A fast clock is a permanent veto, not a tie-break.** A device an hour ahead
wins every conflict it enters for the next hour. A device a year ahead wins them
forever. Its peer can re-edit the value a hundred times and every one of those
edits loses to the same stale op, because the peer's honest timestamp is still
the smaller number. Nothing is logged, nothing is surfaced, and from the user's
side the field simply refuses to change. That is issue #21.

Both replicas converge, so the proptest and the chaos suite pass. Convergence on
the wrong value is still convergence.

## Decision

**Envelope field 5 carries an `Hlc { physical_ms: u64, logical: u32 }`** — a
hybrid logical clock (Kulkarni et al., 2014) — instead of a bare `ts_ms`, and
the LWW comparison key is `(hlc, device_id, seq)`.

An HLC is not a clock reading, it is a **causal position** that stays
human-meaningful:

* **Send** is strictly increasing. `physical_ms` takes the wall clock when the
  wall clock is ahead and the logical counter resets; otherwise the counter
  increments. A device's own ops are totally ordered even if its clock stalls or
  jumps backwards.
* **Receive** is `max(local, received, now) + 1`. After observing a peer's op,
  this device sits above it, so everything it emits afterwards sorts after the
  op that caused it. Causality without a vector clock.
* **The receiver stores the SENDER's value**, not its own post-merge reading.
  Two replicas that receive the same op in different orders must record the same
  stamp for it, or the LWW winner would depend on delivery order — the exact
  divergence LWW exists to prevent.
* **A reading more than `MAX_DRIFT_MS` (5 minutes) beyond the receiver's own
  clock is refused** — not applied, not recorded in the op log, and *not
  absorbed*. Absorbing it would drag this device's clock forward with the bad
  one and propagate the skew to every peer it talks to next. A reading in the
  **past** is always accepted: that is a laptop opened after a week offline, not
  a fault, and refusing it would break offline-first.

### Why the fast clock stops winning

The HLC does not stop a fast device from winning a genuinely concurrent race —
somebody has to win, and the tie-break has to be deterministic. What it removes
is the veto. A peer **cannot edit an entity it has never seen**, so by the time
it edits, it has already absorbed the fast device's stamp and sits above it. The
next round goes to whoever wrote last, which is what a user expects and what the
old rule made impossible.

### The three terms

| Term | Why it is there |
|---|---|
| `hlc` | The above. Orders across devices and inside a millisecond. |
| `device_id` | Raw 16-byte memcmp, higher wins. Breaks **cross-device** ties deterministically so every replica picks the same winner. Unchanged from ADR-0014. |
| `seq` | The writer's per-`(stream, device)` counter, already envelope field 4. Reached only when two ops from the **same** device carry an equal `hlc`. |

`seq` closes the residual documented at `engine.rs`. The send rule makes a
same-device HLC tie impossible *while a device's clock state lives*; it becomes
possible across a process restart, because the logical counter is deliberately
not persisted. In that window `seq` still orders the two ops correctly. The
memcmp must **not** be applied to a device's own ops — `dev > dev` is false, so
the later op would lose to the earlier one and be silently discarded on every
remote replica while the originating replica kept it. That bug was found and
fixed before this ADR; `seq` is what makes the fix principled rather than a
special case.

### Injection

`HlcClock` is injected through `CoreConfig` beside `Clock`. It is the only
stateful thing in the core, `clippy.toml`'s disallowed-methods list and the CI
determinism gate forbid reading an ambient clock in crate source, and a test
that wants to model a skewed replica must be able to supply the skew.
`CoreConfig::with_clock` and `Engine::from_clock` derive it from the wall clock,
so a test skews one thing and gets a coherently skewed replica.

## What we give up

* **A container-format bump.** Field 5 changes type from `uint` to
  `[uint, uint]`, so `ENVELOPE_FORMAT_V` moves and the frozen vectors move with
  it. Neither number ever shipped.
* **Five minutes of exposure.** A device up to `MAX_DRIFT_MS` fast still wins
  concurrent races for that long. The bound is a trade: tighter, and honest
  devices with sloppy NTP get their ops refused; looser, and a broken clock
  keeps its advantage longer. Five minutes matches the existing skew warning in
  `docs/03-crypto/audit-and-tamper-evidence.md`.
* **Refused ops are silently dropped by the receiver.** The sender does not
  learn that its clock is why. Surfacing that needs a wire error path and is
  left for the auth/error work.
* **HLC state does not survive a restart.** Persisting it would need a durable
  write on the op path for a case `seq` already covers.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Clamp `ts_ms` to a server-observed window** (the relay-side rule in `audit-and-tamper-evidence.md`) | Requires a trusted third party for a property two offline devices must have on their own, and Sunrise's relay is explicitly untrusted. Retained as the *relay's* rule, which is a different job. |
| **Vector clocks** | Exactly orders concurrency and detects it. Costs O(devices) per op on a system where the entity-level merge model has no use for detected concurrency — it must still pick one winner. |
| **Lamport clock only** | Orders causally but loses all wall-clock meaning: a stored stamp stops answering "when", which reviews, stats, and the activity feed all read. |
| **Pack the HLC into one `u64`** (48 bits physical, 16 logical) | Keeps field 5 a `uint` and avoids the format bump. Caps the logical counter at 65 535 and hides an overflow rule inside an integer. Field ids and array elements are cheap; a silent counter wrap is not. |
| **`(hlc, device_id, seq)` in envelope field 5 (chosen)** | One ordering key, causal, bounded, and replica-independent because the sender's value is what gets stored. |

## Consequences

* **Two devices more than five minutes apart cannot sync.** That is intended,
  and it is why the existing "your clock is ≥ 5 minutes off" warning matters.
* **`lww_ts_ms` became `lww_hlc_ms`, with `lww_hlc_logical` and `lww_seq`
  beside it.** The old column name described a wall clock it no longer held.
* **The relay is unaffected.** It reads envelope headers, never the HLC, and its
  own clamp rule in `audit-and-tamper-evidence.md` is unchanged apart from
  naming the field.
* **Per-field LWW is still deferred**, see ADR-0014's amendment. The HLC changes
  *which* write wins, not *how much* of the entity the winner replaces.

## What would force revisiting this

1. **A device legitimately more than five minutes off.** An air-gapped machine
   with no NTP, say. The answer is probably a user-visible clock-correction
   flow, not a wider window.
2. **Concurrency detection becoming useful.** If the merge model ever needs to
   know that two writes were concurrent rather than merely ordered — to surface
   a conflict, or to merge per-field — an HLC cannot tell it. That is the point
   at which vector clocks or dotted version vectors stop being over-engineering.
