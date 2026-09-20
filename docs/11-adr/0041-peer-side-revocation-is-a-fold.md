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

**Storage:** `STORAGE_V` 26 → 28, migrations `0027_device_revoke_ops.sql` (the
ledger the register folds from) and `0028_device_read_bounds.sql` (the monotone
read bound the register can no longer be). No new op, no wire change, no
primitive change: `DOC_SCHEMA_V`, `CRYPTO_SUITE_V` and `ENVELOPE_FORMAT_V` stay
where they are.

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
over every row and not over any part of one — and then makes a **second pass
over that frozen map, discounting `s` from `v`'s set when the ledger holds a
row revoking `s` whose sender is not `v`**. Both passes read the row set and
neither reads the walk. It then walks the ledger in one
canonical total order — `(op_hlc_ms, op_hlc_logical, sender, revoked_device_id)`,
the order the register's LWW comparator already used, extended by the one
column that makes it total — and:

- **skips a row naming its own sender.** Unchanged, and now held in the fold as
  well as at ingest, because the fold is the sole author of the register and a
  rule enforced only on the way in would be absent for every row already in the
  ledger.
- **skips a row whose sender survives in the discounted map as a revoked
  device — unless the only party to have revoked it is the device that row is
  about.**
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

**And the discount is what stops that set from being a weapon of its own.**
Building from every row while only the walk judges one means a row the walk
*gates* has already seated its sender in its target's set — so a revoked device
named each remaining device in one ordinary op apiece, every op correctly gated
and revoking nobody, and gated the whole account out of revoking anything for
good. The second pass above removes exactly the claims the ledger itself shows
disowned, and `sender != v` is what keeps it from becoming §Alternatives (h)'s
wider form. §Alternatives (i) is the argument;
`a_gated_revocation_does_not_seat_its_sender_in_its_targets_revoker_set` is the
test, and §"What a user sees" item 4 states the bound and the residual.

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

### 2. A read-bounded device's third-party `key_envelope` claim is not recorded

`key_envelope_recipients` is what `backfill_key_envelopes` reads to decide a
device has already been served, and the `Recipient::Device(other)` arm files a
row on the sender's word alone — it holds no key for that ciphertext, so nothing
about the claim is checkable. The comment there already named the sender class
it could not argue away: a revoked device's reads are bounded by the rotation
and its writes were bounded by nothing, so it could file these rows and could
not read what they withhold. It is now refused.

**The gate is the bounded set and not the revoked one, which is wider than
"a revoked device".** The sentence above names a class by the property that
makes it dangerous — *reads are bounded* — and since migration 0028 that
property is `device_read_bounds`, which the register is only a subset of. So
the refusal reaches the unwound device too: `read_bounded: true, revoked:
false`, a device the fold has stopped calling revoked while the keys stay cut
off, which is precisely the member the sentence describes and precisely the
member the register no longer names. Read every "a revoked device" in this ADR
about *this* gate as "a read-bounded device": each such sentence is true as
written and strictly narrower than the code. Widening costs nothing here for
the reason the next paragraph gives, and that is what makes it affordable to
gate on a predicate that does not converge.

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

  **"A recipient of nothing" holds because of `device_read_bounds`, and did not
  hold without it.** §"What a user sees" item 2 says the register routinely
  stops calling a device revoked — so while the recipient gate read the
  register, an unwound device *was* a recipient again, and this bullet's
  justification failed in exactly the case the fold is designed to produce. The
  contradiction was real and it is repaired in the code rather than talked
  away: migration 0028 gives the read bound its own monotone table, so
  `emit_key_envelopes` and `backfill_key_envelopes` no longer read anything the
  fold can take a row out of. **On a replica that bounded the publisher**, the
  republish restores `d_d_pub` and restores nothing else, and both sentences
  are true at once. That qualification is the whole of what 0028 buys and it
  is not decoration: §"What a user sees" item 5 works a replica that met two
  ordinary retirements in the other order and **never wrote the bound**, and
  says of the same code path that one `DeviceCertPublish` there "recovers
  every held epoch of every stream". The two passages describe one path at two
  replicas, and they must be read together.
  [#282](https://github.com/justin13888/Sunrise/issues/282) is the gap.

  **This bullet's own opening sentence stands as written**, and the reason is
  worth stating rather than leaving as an omission: on that same replica the
  publisher has no `device_revocations` row either, so it is not *locally* a
  revoked device, and "a revoked device gains nothing by republishing its own
  cert because it is a recipient of nothing" is **vacuous** there rather than
  false. What fails on that replica is this paragraph's mechanical claim about
  what the republish restores, which is unconditional, and not the membership
  argument above, which is not. So the qualification lands here and the
  decision itself is unchanged: nothing in the read bound's split touches the
  republish path, and `backfill_key_envelopes`' early return is exactly the
  per-replica test.
- **`Command::RevokeDevice` locally.** No "this device is revoked, refuse the
  command" guard. The one revocation a revoked device must still be able to make
  is of the device that revoked it — that is the fold's exception, and it is the
  recovery path when a compromised device gets its op in first. A local guard
  would close the recovery path to buy a clearer error for the case that does
  not matter.

## What a user sees when an op is refused

Nothing they typed is ever refused, and that is the point of the scope. What can
be refused is an administrative act by a device the account has expelled, and
there are five visible consequences:

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

   It does not stop at NDJSON. `CommandResult::revocation_unwound` carries the
   ids to the caller on the next revocation, the CLI prints them under
   `sunrise devices revoke`, and the Apple device list states the row's own
   condition. That is the same rule `log-events.md` already stated for
   `revoke_incomplete`, applied to a strictly larger consequence.
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

   **The remedy is a third current device**, and the bound that makes one
   enough is the discount pass §Decision 1 carries. For one revision it was
   not enough: the revoker map was built from every row while only the walk
   judged one, so a row the walk gated still seated its sender in its target's
   set, and a revoked device reached every current device with one ordinary op
   apiece. The claim in this paragraph was false for as long as that lasted,
   and it is worth saying so rather than quietly restoring it.

   What it is true of now: **a revoked device X can enter the revoker set of a
   device V only when V is the only device that has revoked X.** X is revoked,
   so some device O revoked it; X survives the discount in V's set only when
   no row revokes X from a sender other than V; so O is V. Two things follow
   that a reader can rely on. A device X merely *named* is untouched, because
   it never revoked X — that is the case
   `a_gated_revocation_does_not_seat_its_sender_in_its_targets_revoker_set`
   asserts. And a second device that also revoked X is untouched, because each
   of the two is then a revoker of X other than the other. So the lockout
   costs the two devices in the relationship and no third.

   The rest of the remedy is unchanged: revocation is not gated on `ID_S_priv`
   anywhere, identity rotation and pairing sponsorship are untouched, and any
   current device outside the pair still revokes whoever it likes. The lockout
   is total only in a two-device account, where there is no third device to
   ask — and there the survivor has nothing left to revoke but itself, which
   `Command::RevokeDevice` refuses anyway.

   **What the discount gives up, with its condition.** It asks its question of
   the ledger — has anybody other than V expelled S? — and not of the
   register, because asking the register is §Alternatives (h)'s wider form.
   The condition for the residual is that question answered yes, and it has
   two shapes.

   *Rehabilitation, of which only half is new.* O revokes X; a third party P
   then revokes O. X was already off the revoked list, because a revoked
   device's revocations are unwound whatever date they carry — that is
   §Decision 1's retroactivity and it predates the discount. What the discount
   adds is that X is no longer *gated*, so it revokes third parties again.
   `the_discount_rehabilitates_a_device_whose_sole_revoker_a_third_party_revokes`
   pins it.

   **P is not required to be a bystander**, which is the reading a threat model
   has to take. One attacker holding two devices the account revoked together
   reaches this with a single op: O revoked X1 and X2; X1 revokes O; the mutual
   exception lands it; O goes out, which both unwinds O's revocation of X2 and
   discounts O out of X2's set; and X2 then revokes the rest of the account.
   The remedy is the mutual pair's and no better — a device X2 reaches revokes
   it back and is left revoked itself, so what the account needs is a current
   device the attacker never reached.
   `the_discount_lets_one_of_two_devices_revoked_together_ungate_the_other`
   pins it. It is narrower than what the discount closes — that cost one op
   from *one* revoked device and was permanent and account-wide — but it is
   not nothing, and #241's un-revoke is what would settle it.

   *The chain, which is the hole.* One link further — O revokes X, P revokes
   O, Q revokes P — and Q's row gates P's, so O's revocation of X stands and X
   is **on the revoked list while being ungated**, which is the pair of facts
   this gate exists to keep apart. It costs three revocations in a chain, and
   X can author none of the two that matter: a device authors only rows whose
   sender is itself, and every discount of S from V's set needs a row from a
   sender that is not V, so X can never discount anything out of its own set.
   The rows that rehabilitate X are written by other devices — honest ones, or
   a second device the same attacker holds.
   `the_discount_leaves_a_revoked_device_revoking_when_a_chain_revokes_its_revoker`
   pins it.

   It is recorded rather than repaired because no ledger-only rule can do
   better. After a mutual revocation the two devices are symmetric in the
   ledger; nothing distinguishes the honest one from the compromised one, so
   ungating both hands an attacker the account, and exempting from a device's
   revoker set any revoker the final register revokes reopens §Decision 1's
   bypass with the arrow reversed. §Alternatives (f) and (h) price both.
   `a_mutual_pair_locks_both_devices_out_of_third_party_revocation` pins the
   behaviour so that it stays deliberate.

5. **An unwound device shows as current and still receives nothing.** The keys
   are *not* given back, and that is the one place this design deliberately
   stops being a pure function of the op set. `device_revocations` answers "is
   this device currently called revoked?" and has to converge, so it is derived
   and reversible; all four key-distribution sites read `device_read_bounds`
   instead, which is written by `INSERT OR IGNORE` and never deleted from
   (migration 0028). Without the split an unwind released the read bound —
   the device re-entered `emit_key_envelopes`' recipient set for every
   subsequent epoch, and one `DeviceCertPublish` from it drove
   `backfill_key_envelopes` to hand back every held epoch of every stream.

   The price is order dependence on the bound, and it is **not** one-directional
   and **does not converge**. A replica that believed a revocation before
   learning it was unwound holds the row; one that met the two ops the other way
   round never held it and never will, because from then on both replicas
   compute the same gated register and the `INSERT OR IGNORE` has nothing new to
   write. Retire laptop C from desktop A, then months later retire A from phone
   B: a replica applying `A -> C` first ends with the bound `{C, A}`, and a
   replica applying `B -> A` first ends with `{A}` — C is never bounded there, so
   it stays a recipient of every epoch that replica mints and one
   `DeviceCertPublish` from it recovers every held epoch of every stream. That is
   the same failure the split closes, at the new site, for a replica that met the
   ops in the other order.

   So the guarantee this decision earns is **per-replica** and is stated that
   way: once a replica has bounded a device, no later fold on that replica gives
   the bound back, which is the whole of the unwind as a single replica can
   observe it. Making the bound a function of the op set — so that two replicas
   converge — needs a derivation this ADR does not have: the naive ledger seed
   lets a revoked device bound the whole account with N ordinary ops, and
   ordering by the cut instead reintroduces the back-dating exposure §Decision 1
   exists to close. [#282](https://github.com/justin13888/Sunrise/issues/282)
   carries the counterexample, the `PairingPayload` consequence — a device that
   pairs today inherits no revocation state and is the weakest replica in the
   account — and the two questions the design turns on.

   The *register* still could not be made the ratchet instead, for the reason
   §"What would force revisiting this" trigger 4 gives: a ratcheted register
   makes two replicas paint different device lists, which is the user-visible
   divergence ADR-0034 corollary 3 forbids. `DeviceRow::read_bounded` is what
   keeps the resulting asymmetry from being invisible.

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
*is* O. It was deferred to #241's un-revoke on the reading that it was no help
for item 4 and only an improvement for a three-party variant. **It is taken, in
§Decision 1.** See (i).

**(i) Taking the narrower discount after all, because item 4 was not what it
was for.** The deferral above priced this rule against the mutual pair, where
it correctly does nothing. What it is actually for is the case the deferral did
not look at: the revoker map is built from every row and only the walk judges
one, so a row the walk *gates* still seats its sender in its target's set. A
device the account had expelled therefore named each remaining device in one
ordinary op apiece — every op gated, revoking nobody — and gated the whole
account out of revoking anything, permanently, for the price of N ordinary ops
and no crafted stamp at all. That is strictly worse than anything item 4
describes, and nothing else in the option set closes it: building the map only
from the rows the walk lands is §Decision 1's prefix rule and its back-dating
bypass, a fixpoint is what (f) declines as non-monotone, and refusing to store
a row from an already-revoked sender is delivery-order dependent, which ADR-0034
corollary 3 forbids.

`sender != v` is the whole of why this is not (h)'s wider form: X's only revoker
is O, and no row revokes O from a sender other than X, so nothing is discounted
and the mutual pair's lockout is preserved exactly. It is a second pass over the
frozen first map rather than a fixpoint — no entry's fate depends on another
entry's — so the register stays a pure function of the ledger's row set and
corollary 3 still holds.

What it does not close is above, in §"What a user sees" item 4, with its
condition: a device whose sole revoker a third party revokes is rehabilitated,
and with a three-link chain a device that is still on the revoked list revokes
third parties anyway. Both are narrower than what it closes — that cost one op
from one revoked device, was permanent, and reached the whole account — and
neither is reachable by a revoked device out of its own rows. Neither is out of
an attacker's reach either, where the attacker holds a second device the same
revoker expelled. #241's un-revoke is still what would settle the question the
discount has to guess at. Taken.

## Consequences

- **`key-rotation.md` §Revocation gains the write bound it did not have**, and
  the interim paragraph about the cursor waiting and the relay churning is
  replaced by what actually happens now.
- **`threat-model.md` A3's revocation mitigation is narrower and truer**: a
  revoked device cannot revoke another device, and a **read-bounded** device —
  which is every revoked one and, after an unwind, more — cannot claim a key
  was delivered to a third one.
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
- **The read bound is a second table and is never rewritten**, only added to
  (`0028_device_read_bounds.sql`). One `INSERT OR IGNORE` per surviving row runs
  immediately before the `DELETE` above, so no row passes through a window where
  it is in neither. `Engine::is_read_bounded` is the read, and the four
  key-distribution sites are **not** its only askers: a **fifth** asks it
  without distributing a key, a sender's authority to claim a third-party
  `key_envelope` recipient row (§Decision 2 above), which is the one this ADR
  first pointed at the register and then moved. `Engine::is_revoked` keeps the
  **two** that ask the convergent question — the device list, which reads
  `device_revocations` through a direct `LEFT JOIN` rather than through the
  function, and `CommandResult::revocation_gated`. Which of the two a new call
  site wants is the first question to ask of it, and `engine/revocation.rs`'s
  module doc is where the two are stated side by side — and is authoritative
  over this bullet, because it sits beside the functions and this does not.
- **A gated revocation now does nothing at all.** It was the op being discarded
  and everything else proceeding: the relay intent was already guarded, but
  every stream still rotated and the account identity still rotated with it —
  and the device doing the minting is, necessarily, the revoked one. It wrote
  each fresh key into its own `stream_keys` and sealed it to every honest peer,
  whose next writes were then readable by it. `Command::RevokeDevice` guards the
  rotation on the same `effective` predicate as the relay intent. This closes
  one route and not the mechanism: `Command::RotateStreamKey` reaches the same
  mint-and-distribute chain with no gate of any kind, and
  `Keychain::absorb_stream_key` checks no sender standing.
- **An upgraded vault folds *from* what it already held, and not necessarily
  back to it.** 0027 seeds the ledger from the register, and what the seed
  preserves is the fold's **input**, not its output. It folds back to the same
  register whenever the seeded one holds no chain of revocations; the moment it
  holds one it does not, and the sequence that produces a chain is ordinary
  rather than adversarial — retire an old laptop from the desktop, and months
  later retire the desktop from the phone. `{C revoked by A, A revoked by B}`
  seeds `A -> C` and `B -> A`; the fold builds `A`'s revoker set as `{B}`, `B`
  is not `C`, so `A -> C` is gated and C shows current again at the next
  `device_revoke`. That is §Decision 1's retroactivity reaching an upgraded
  vault — a *removal*, landing at the next `device_revoke` rather than at
  migration time, with `core.device.revocation_unwound` as the only signal and
  this family's usual remedy. The trace is at the seed, in
  `crates/sunrise-storage/migrations/0027_device_revoke_ops.sql`, and
  `the_0027_seed_carries_a_chain_of_revocations_into_the_ledger` pins the shape
  it hands over.
- **Separately, the ops that lost an LWW contest before the migration are not
  recoverable.** They were never written down — that is the defect — so the
  ledger starts as the surviving register and grows from there. That is the
  conservative direction: the fold's input is a subset of the true op set, so it
  can only fail to skip a revocation it has no record of and never invent one.
  It is not an argument that the output is preserved, which the bullet above
  says it is not.

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
