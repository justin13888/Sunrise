# 0041 — Peer-side revocation enforcement covers the control ops whose effect can be re-derived, and the register becomes a fold

**Status:** accepted

**Amends** [ADR-0034](./0034-revocation-bounds-reads-not-writes.md) §Decision
corollary 3, which reserved peer-side enforcement for after the relay bound and
required that it not reintroduce order dependence. This is that enforcement,
and the corollary is met by deriving the register rather than by refusing an op
where it lands. ADR-0034's decision about a revoked device's **entity** writes —
a task edit, a note, a focus session — is unchanged and is now load-bearing in a
narrower place.

**Depends on** [#80](https://github.com/justin13888/Sunrise/issues/80), which is
closed, and whose state is asserted from the tree in §Context rather than taken
from the issue.

**Storage:** `STORAGE_V` 26 → 27, migration
`0027_device_revoke_ops.sql`. No new op, no wire change, no primitive change:
`DOC_SCHEMA_V`, `CRYPTO_SUITE_V` and `ENVELOPE_FORMAT_V` stay where they are.

## Context

### #80 landed, read out of the tree

ADR-0034's §"What would force revisiting this" names #80 landing as the first
trigger and #82 as the re-entry point. It has landed, on both sides:

- **Client.** `Command::RevokeDevice` records a durable intent in the same
  transaction as the op — `relay_revocation_intents`, migration 0020 — and
  `sync_driver::drain_relay_revocations` drains it with retry on every connect
  and reconnect, clearing the row on success and on a relay that never knew the
  device, and counting attempts otherwise. `Core::relay_revocation_pending`
  crosses the seam so a client can tell "revoked locally" from "the relay has
  stopped accepting it", and both Apple clients show it.
- **Relay.** `DELETE /api/v1/devices/{id}` refuses self-revocation, sets
  `revoked = 1` and deletes the device's push tokens in one transaction.
  `store.rs::active_device`'s SQL ends `AND revoked = 0`, so every signed route
  — `/sync/ops`, `/sync/session`, `/sync/subscribe`, `/sync/session/refresh` —
  resolves the device to `None` and answers 401. `GET /sync/events` re-runs the
  lookup on a `device_recheck_ms` interval and closes a live session with
  `AuthDeviceRevoked`.

**How a peer learns its upload was refused:** it does not learn it as a per-op
answer. It learns it as a 401 on the whole request, or as a `closed` frame on
the events stream — the device is cut off from the relay wholesale, not
op-by-op. That is the shape that matters here, because it means there is no
per-op refusal for a peer to reconcile and no cursor that can freeze against a
climbing `max_seq`. The dilemma #82 records is dissolved for entity ops by #80
rather than resolved by this record.

**And it is conditional.** Every relay-side check runs only when the request
carries a device signature. With no `header_sig_v2` and `require_device_sig`
false, `verify_bytes` returns `Ok(None)`, no device is resolved, and no
revocation check happens. `require_device_sig` **defaults to false**, and
`ServerConfig::validate` refuses to let it be true in single-tenant self-host.
So in the default deployment the relay bound is not in force, and peer-side
enforcement is not decoration — it is the only line there is.

### The hole nobody had named

Reading the apply path for what a revoked device can still do turns up something
worse than the churn #82 is about. The `DeviceRevoke` arm of `apply_control_op`
refused exactly one thing: an op naming **its own sender**. Nothing stopped a
revoked device revoking somebody *else*.

It has everything it needs. Its cert is still on the chain, because
`lookup_device_cert` identifies a device without judging its standing and says
so at length. It still holds the vault-meta Stream key for the epoch it was cut
at — `revoke_device` seals its own ops under the old epoch, deliberately, so
that the departing device and the remaining ones still share it. Peers keep old
epoch keys. So the op verifies, decrypts and applies on every replica.

And revocation has no inverse. Nothing in the tree deleted a
`device_revocations` row, and `Engine::is_revoked` is presence and nothing else.
So a laptop the account had already expelled could expel the account, on every
replica, permanently. That is strictly worse than the harm revocation exists to
answer, and it is the gap this record closes.

### Why refusing where the op lands does not work

Refuse a `device_revoke` at the point it arrives and the register depends on
delivery order: a replica that applied it before learning its sender was revoked
keeps the row, one that met the two ops the other way round does not, and
nothing ever reconciles them. That is the divergence ADR-0034 exists to keep
out, and corollary 3 forbids peer-side enforcement from reintroducing it.

## Decision

**Peer-side enforcement is wanted, and it covers the control ops whose effect
this engine can re-derive. It does not cover entity writes, which stay as
ADR-0034 left them.**

Two gates, and a shape.

### 1. `device_revocations` becomes a fold over a kept ledger

Every `device_revoke` op is **stored whatever its sender's standing**, in
`device_revoke_ops` (migration 0027), and the register is recomputed from it
each time one lands. Before the walk the fold builds `revokers_all` — for each
device, the set of *other* devices the ledger records as having revoked it,
over every row and not over any part of one. It then walks the ledger in one
canonical total order — `(op_hlc_ms, op_hlc_logical, sender, revoked_device_id)`,
the order the register's LWW comparator already used, extended by the one
column that makes it total — and:

- **skips a row naming its own sender.** Unchanged, and now held in the fold as
  well as at ingest, because the fold is the sole author of the register and a
  rule enforced only on the way in would be absent for every row already in the
  ledger.
- **skips a row whose sender the ledger revokes anywhere — unless the only
  party to have revoked it is the device that row is about.**
- otherwise lands it, later overwriting earlier, which is the LWW register
  written as the fold it always was.

The order decides the register and no longer decides the gate. That separation
is the whole of this decision's security content, and §"The gate reads the
whole ledger" below is why.

This is the "stored but never folded" idiom the engine already has for a
transition signed by a stranger
(`a_transition_from_a_stranger_is_stored_and_never_folded`) and for a forged
sibling under ADR-0040: the row is kept, it is judged, and the judgement is
re-taken when the truth about it changes. Here it is exactly what corollary 3
asks for, because the answer is a pure function of the op set: two replicas
holding the same ops walk the same sequence and reach the same register, in any
delivery order. `the_register_is_the_same_whichever_order_the_two_revocations_arrive`
is that property, asserted from the losing order.

**The exception is not a softening; it is what keeps the gate from being an
attack.** An HLC sorts first by being dated earlier and nothing charges for
that. A bare rule of "whoever revoked first silences the other" would hand a
stolen laptop the account: back-date a revocation of the owner's Mac by a year,
and the Mac's answer sorts second, is skipped, and never lands. So "B says A is
out" is not on its own a reason to disbelieve "A says B is out" — the two claims
are symmetric, and the engine's existing resolution, that both devices end up
revoked, is the safe one. It becomes a reason the moment A reaches for a *third*
party, or the moment anyone other than B has also revoked A. Both are the one
condition. `a_gate_on_the_sender_does_not_let_a_back_dated_revocation_silence_its_target`
is the test that would have caught the bare rule; it did, during this change.

#### The gate reads the whole ledger, because the sort key is the sender's

An earlier draft of this decision judged a sender against the **prefix** of the
walk: a row was skipped when some revocation of its sender sorted below it. That
is one subtraction away from no gate at all. The sort key is the op's HLC, the
HLC is whatever its sender wrote, and nothing bounds it downwards — `Hlc`'s
`MAX_DRIFT_MS` bounds only the future, `crates/sunrise-core/src/engine/sync.rs`
says outright that a reading in the past is fine and common, and nothing ties an
op's stamp to that sender's own `seq`, to its meta epoch, or to any earlier
stamp it sent. So a revoked device dated its `device_revoke` ops a millisecond
below its own cut, they sorted first, the prefix had not yet revoked their
sender, and they landed. Iterating the remaining device ids revoked the whole
account — the exact outcome §Alternatives (f) rejects a rival design for
admitting, and the one #82 exists to stop.

Judging the sender over the whole ledger closes it, and closes it completely: a
set built from every row is not a number an attacker can pick, and
`revokers_all[X]` holds X's own revoker by construction, so there is no
no-history corner either. It is still a pure function of the ledger's row set
rather than of delivery order, which is corollary 3's requirement — if anything
more obviously so, because the answer no longer depends on where in the walk a
row sits. `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`
is the test.

**The gate reads no clock at all — not this device's, and not the op's.**
`is_revoked`'s doc explains at length why comparing a stored `cut_ms` against
this device's own HLC is unsound — `MonotonicHlc` is not persisted, so after a
restart the reading is 0 and the test collapses to a bare wall clock. This gate
does not compare times; it asks a set question about who revoked whom, and the
answer is the same on every replica holding the same ops.

#### What that gives up: revocation here **is** retroactive

An earlier draft of this decision also claimed revocation stays
**non-retroactive** — a revoked device's revocations from before its own cut
still stand, because they sort below it. That property is retired, deliberately
and for this op family only, because it *was* the vulnerability. "Before its own
cut" is not a fact the ledger holds: the cut and the op stamp are both numbers
the same sender chose, and nothing distinguishes an honestly-earlier revocation
from one back-dated a minute ago. A rule that preserved the distinction only for
senders with prior history is evaded by a sender that files none.

So a revoked device's revocations are unwound whatever date they carry
(`a_revocation_written_before_the_senders_own_cut_is_unwound_when_the_sender_is_revoked`).
Revocation stays non-retroactive everywhere else in the module — a cert issued
under a superseded identity still identifies its device — and this family is the
exception for the same reason §3 gives for scoping the gate to control ops: its
effect can be re-derived, and a task edit's cannot. What is lost is an
administrative act by a device the account has expelled, which a human can do
again from a device the account still trusts.

The visible consequence is that `core.device.revocation_unwound` is a routine
signal rather than an exotic one: a revocation this replica already believed
stops being believed the moment it learns its author had been revoked. That is
the correct thing to say out loud, and
`a_revocation_the_fold_stops_believing_is_announced` pins that it is said.

### 2. A revoked device's third-party `key_envelope` claim is not recorded

`key_envelope_recipients` is what `backfill_key_envelopes` reads to decide a
device has already been served, and the `Recipient::Device(other)` arm files a
row on the sender's word alone — it holds no key for that ciphertext, so nothing
about the claim is checkable. The comment there already named the sender class
it could not argue away: a revoked device's reads are bounded by the rotation
and its writes were bounded by nothing, so it could file these rows and could
not read what they withhold. It is now refused.

This gate needs no fold and raises no convergence question, because the table is
a **hint and not state**: a replica that declines the row finds no row and emits
the backfill, so the gate can only ever cause *more* key distribution, never
less. Two replicas disagreeing about one row cannot withhold a key from anybody.

### 3. What is deliberately not gated

- **Entity writes.** ADR-0034 stands. There is no projection rebuild in this
  engine, so a fold gate over task ops needs an un-materialization that does not
  exist, and building one would make a cut correction destructive — the thing
  the move away from "earliest cut wins" was for. After #80 the relay is the
  bound for these, where it is in force.
- **`device_cert` publication.** A cert is a fact about a device, not a grant of
  standing: `lookup_device_cert` identifies without judging, and the separation
  is what makes the identity fix convergent. Membership is
  `devices.identity_id == head`, tested at every point of use, and a revoked
  device gains nothing by republishing its own cert because it is a recipient of
  nothing. Gating here would re-merge the two questions the code keeps apart.
- **`Command::RevokeDevice` locally.** No "this device is revoked, refuse the
  command" guard. The one revocation a revoked device must still be able to make
  is of the device that revoked it — that is the fold's exception, and it is the
  recovery path when a compromised device gets its op in first. A local guard
  would close the recovery path to buy a clearer error for the case that does
  not matter.

## What a user sees when an op is refused

Nothing they typed is ever refused, and that is the point of the scope. What can
be refused is an administrative act by a device the account has expelled, and
there are four visible consequences:

1. **The attack produces no effect.** The device list is unchanged: the device
   the expelled one tried to revoke stays current. There is a `warn` line,
   `core.device.revoke_refused` with `reason = "revoked_sender"`, for an
   operator reading NDJSON.
2. **A revocation can disappear, and says so.** A re-fold can *remove* a row: a
   revocation of S arriving now skips every row S wrote — whenever S dated them
   — so a device S had revoked becomes current again. That is the fold being a
   pure function of the op set rather than a ratchet, and since the gate reads
   the whole ledger it is a routine outcome rather than a corner. It emits
   `core.device.revocation_unwound`, because a device list that quietly changed
   back is the one outcome of this design a user could be surprised by. **The
   remedy is to revoke that device again, from a device the account still
   trusts.**
3. **A cut correction does not recover a skipped revocation.** #82 offers two
   honest options — re-request the op, or accept the loss and say so where a
   user can see it — and this takes the second, because the first is not even
   needed: the op was never discarded, it is simply still gated. Correcting a
   cut appends a second revocation of the same sender by the same party, which
   changes which row wins the register and changes nothing about who has
   revoked whom — so the gate answers the same question the same way.
   `a_cut_correction_does_not_re_fold_a_skipped_revocation` pins it.

   This is tolerable **here and would not be tolerable for a task edit**, and
   the difference is the whole reason for the scope. What is lost is an
   administrative act by a device the account has expelled: a human can make it
   again from a device that is still trusted, and it is an act they would want
   to look at again anyway. A sentence the user never typed twice cannot be
   re-typed.

4. **A device that has been in a mutual revocation can no longer revoke
   anybody else.** The mutual exception's cost, stated here because a user can
   reach it: once X and O have revoked each other, each one's only revoker is
   the other, so each is forgiven for revoking the other and gated for
   revoking a *third* party. That reaches the honest device of the pair too,
   and it is permanent, because revocation has no inverse
   ([#241](https://github.com/justin13888/Sunrise/issues/241)). One op from a
   device the account has already expelled therefore costs the device that
   expelled it its third-party administrative capability, for good.

   **The remedy is a third current device**, and the reason there is one is
   that the loss is narrow. Revocation is not gated on `ID_S_priv` anywhere:
   identity rotation and pairing sponsorship are untouched, and every other
   current device in the account still revokes whoever it likes. The lockout
   is total only in a two-device account, where there is no third device to
   ask — and there the survivor has nothing left to revoke but itself, which
   `Command::RevokeDevice` refuses anyway.

   It is recorded rather than repaired because no ledger-only rule can do
   better. After a mutual revocation the two devices are symmetric in the
   ledger; nothing distinguishes the honest one from the compromised one, so
   ungating both hands an attacker the account, and exempting from a device's
   revoker set any revoker the final register revokes reopens §Decision 1's
   bypass with the arrow reversed. §Alternatives (f) and (h) price both.
   `a_mutual_pair_locks_both_devices_out_of_third_party_revocation` pins the
   behaviour so that it stays deliberate.

## Alternatives considered

**(a) The relay is the only place it belongs.** #82's first question, and it
fails on #80's own deployment table: with `require_device_sig` at its default of
false an unsigned request is bound to no device at all, so the relay enforces
nothing. Under (a), the default deployment has no bound anywhere, and the
revoked-device-revokes-the-account hole stays open in it. Rejected.

**(b) Gate every op, advance the cursor.** #82's first horn. Refusal becomes
permanent, and a cut correction cannot bring honest work back. Rejected, for the
reason ADR-0034 rejects it.

**(c) Gate every op, do not advance the cursor.** #82's second horn. The stall
is unbounded, eviction raises `evicted_through` past the frozen cursor, and
`mark_degraded()` latches on every device in the account within retention.
Rejected. Note that #80 has already taken most of its premise away — a revoked
device's uploads are refused where the bound is in force — but it remains true
in the default deployment, which is precisely the deployment (a) fails in.

**(d) Gate every op, rebuild the projection.** ADR-0034's option (a), and the
only shape under which an entity-op gate converges. `ops` keeps the envelope
bytes, so an entity-scoped replay is imaginable rather than impossible. Rejected
for the reason it was rejected before — the machinery does not exist, the kind
table in `materialize_remote` is wide, and the UI story for applied, visible
user data vanishing when a revocation lands is unwritten — and because this
record's scope makes it unnecessary for the harm actually found. ADR-0034's
trigger 2 still stands: if a projection rebuild ever appears for another reason,
this trade should be re-taken.

**(e) Rank `device_revoke` ops by meta epoch, as ADR-0040 ranks siblings.**
Attractive, and it does not work here. ADR-0040's rank closes its attack because
a device cut by a rotation holds no vault-meta key above the epoch it was cut
at. But `revoke_device` seals its own ops under the **old** meta epoch — "the
epoch the departing devices and the remaining ones all still share" — so the
cutting op and the cut device's ops sort equal on epoch, and the rank decides
nothing. It would also be the wrong instrument: the register is keyed on the
revoked id, so a rank can only order two revocations *of the same device*, and
the hole is a revoked device revoking a *different* one.

**(f) Fold with the LWW cut instead of the prefix.** Tempting, because it would
make a cut correction recover a skipped op. It is not well founded: the gate
would consult a register that itself depends on the gate, so it is a fixpoint
rather than a fold, and the obvious iteration is not monotone. Computing the
gate against the *ungated* register instead terminates and is exploitable — a
revoked device could file a back-dated revocation of an honest device purely to
strip that device's own administrative acts out of the result. Rejected.

**(g) Order the ledger by the sender's own `seq` instead of by the op's HLC.**
The natural answer once §Decision 1's prefix rule is seen to be bypassable, and
the one a review of this change preferred: carry the sender's per-`(stream,
device)` `seq` into `device_revoke_ops` and refuse a row whose HLC inverts
against a lower-`seq` op from that sender. It is priced here because it is the
option a reader will reach for, and it fails twice.

*Not reachable.* The `seq` is in hand on the remote path, but not on the local
one. `revoke_device` must run `apply_control_op` before `ensure_stream_epoch`,
because the register has to be written before anything can mint a key that
would otherwise be sealed to the device being revoked; and `next_seq_tx` must
run *after* `ensure_stream_epoch`, because resolving the epoch can emit
`key_envelope` ops into the meta stream that each take a `seq`, and a number
read beforehand is already spent by the time this op reaches the log. That is
the `60ee61d` bug, and `ops`' `UNIQUE(stream_id, device_id, seq)` is what
catches it. The ordering is a security property with a test on it, not an
accident of writing.

*And ineffective even if it were.* The ledger holds `device_revoke` ops and
nothing else, so a sender that has never filed one has no lower-`seq` row for a
later op to invert against. The attacker's adaptation is to file none —
the first revocation a compromised device ever writes is the one that takes the
account. Widening the anchor to `ops` would answer that and is forbidden: `ops`
is a per-replica set, so a gate reading it is delivery-order-dependent, which is
what ADR-0034 corollary 3 exists to keep out.

Judging `revokers` over the whole ledger, which §Decision 1 takes, closes the
same hole with no schema column, no wire change and no `seq`. Rejected.

**(h) Discount from a device's revoker set any revoker the final register
itself revokes.** The natural answer to §"What a user sees" item 4, and it
**reopens §Decision 1's hole with the arrow reversed**: X, revoked by O, emits
`device_revoke(O)`; the mutual exception lands it; the first pass's register
revokes O; the second pass empties X's revoker set; X's third-party rows land.
That is (f)'s stated exploit, so (f)'s exploitability objection rejects this
too, and not only its monotonicity one.

The narrower variant — discount `s` from `v`'s revoker set only when `s` is
revoked by somebody other than `v` — converges and does not reopen the hole. It
also never fires in the case item 4 is about, because there X's only revoker
*is* O. It is a real improvement for a three-party variant and no help for this
one, so it is deferred to #241's un-revoke rather than shipped alone. Rejected
for now.

## Consequences

- **`key-rotation.md` §Revocation gains the write bound it did not have**, and
  the interim paragraph about the cursor waiting and the relay churning is
  replaced by what actually happens now.
- **`threat-model.md` A3's revocation mitigation is narrower and truer**: a
  revoked device cannot revoke another device, and cannot claim a key was
  delivered to a third one.
- **#82 closes.** Its three questions are answered here: enforcement is wanted
  and is not the relay's alone; an op skipped under a cut that later moves is
  kept and stays gated, and the remedy is to make the act again; a self-naming
  `device_revoke` is still refused, now in the fold as well as at ingest.
- **Revocation is retroactive inside this one op family**, and nowhere else. A
  revoked device's revocations are unwound whatever date they carry, so
  `core.device.revocation_unwound` is a routine signal rather than an exotic
  one. That is the price of the gate not resting on a number the sender picks;
  §Decision 1 is the argument and `key-rotation.md` §"What converges, and what
  does not" is where a reader of the crypto docs meets it.
- **The register is rewritten on every `device_revoke`.** `DELETE` plus one
  `INSERT` per surviving row, inside the transaction that is already open. The
  ledger is bounded by the number of revocation ops an account ever makes, which
  is a handful, and both are in the same order of magnitude as the device list.
- **An upgraded vault folds to what it already held.** 0027 seeds the ledger
  from the register. The ops that lost an LWW contest before the migration were
  never written down — that is the defect — so the fold's input is a subset of
  the true op set, which can only fail to skip a revocation it has no record of
  and never invent one.

## What would force revisiting this

1. **`require_device_sig` ceasing to default to false**, or single-tenant
   self-host gaining device rows. Both change which line is load-bearing, and
   the argument in §Alternatives (a) turns on it.
2. **A projection rebuild appearing in the engine**, for the reason ADR-0034
   gives: option (d) stops being expensive and the scope of the gate should be
   re-taken.
3. **A third control-op family growing an effect that cannot be re-derived.**
   The scope here is not "control ops" as a category but "effects this engine
   can re-derive". A control op that upserts an entity row would not qualify,
   and adding one to the gate on the strength of the word "control" would be the
   mistake this paragraph exists to prevent.
4. **A revocation register that needs to be a ratchet.** The unwind in
   §"What a user sees" is the price of the register being a pure function of the
   op set. If a deployment cannot tolerate a revocation reverting, the answer is
   not to special-case the fold but to re-take the trade, because a ratchet and
   a fold cannot both be true of the same table.
