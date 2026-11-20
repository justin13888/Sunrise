# 0074 — A replica that bounds a device says so in an op, and an ungated sender's word bounds it everywhere

**Status:** accepted

**Answers** [#1167](https://github.com/justin13888/Sunrise/issues/1167), the
half of [#411](https://github.com/justin13888/Sunrise/issues/411) that
[ADR-0058](./0058-the-account-identity-is-the-membership-authority.md) left
open: two replicas that already exist, holding the same ledger, settle on
different read bounds when the decisive revocation is an ordinary row.

**Built by** [#1246](https://github.com/justin13888/Sunrise/issues/1246).
Until it lands, the divergence stays pinned as current behaviour in
`the_register_is_the_same_whichever_order_the_two_revocations_arrive`
(`crates/sunrise-core/src/engine/tests/revocation_fold.rs`).

**Amends** the read-bound section of `refold_device_revocations`
(`crates/sunrise-core/src/engine/revocation.rs`), which says no rule over the
ledger alone closes this. That stays true. This record adds an input the
ledger does not carry.

**Depends on** [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) (the
fold, its gate and its discount),
[ADR-0045](./0045-schema-identity-and-feature-gating.md) §4 and §7 (parked
ops, built by #320; `vault_requires` and `DeviceFeatures`, built by #324),
[ADR-0056](./0056-a-revocation-is-withdrawn-only-by-its-author.md) §2 and §4
(pair heads, and the three conditions that release a bound) and ADR-0058 §4
(authority rows).

## Context

`device_read_bounds` is a ratchet. The fold writes a row for every device its
register names and never deletes one for a device with a cert here. It gates
every key-distribution site: `emit_key_envelopes`, `backfill_key_envelopes`,
the survivor roster of `rotate_identity`, and the readmission signal in the
`DeviceCertPublish` arm.

A ratchet over the registers one replica computed is monotone along that
replica's arrival order, and it does not converge across replicas. Take A
revoking C at `h1`, then B revoking A at `h2 > h1`, both ordinary rows:

- A replica that applies `A -> C` first folds the register `{C}` and bounds C.
  Then `B -> A` gates `A -> C`, the register becomes `{A}`, and the bound ends
  `{C, A}`.
- A replica that applies `B -> A` first never puts C in a register, because
  every later fold already gates `A -> C`. Its bound ends `{A}`. It seals C
  every epoch it mints, and one `DeviceCertPublish` from C drives
  `backfill_key_envelopes` to hand C every epoch it holds.

The second replica is the confidentiality failure that
`migrations/0028_device_read_bounds.sql` split the bound out of the register
to close, surviving for a replica that met the ops the other way round.

What is already closed:

- **A joiner** adopts its sponsor's bound at vault creation (#282), so a new
  device starts where its sponsor stands.
- **An authority row** (ADR-0058) is never gated, so every replica bounds its
  target whatever the order.
  `the_read_bound_is_the_same_whichever_order_an_authority_revocation_arrives`
  pins it.

What is open is the ordinary row: a paired device that is not the holder
revokes, or the holder revokes before the vault requires
`core.revoke_authority`.

No rule over the ledger alone closes it, and #411 says why. The honest ledger
above and a revoked device X back-dating `X -> V` below its own cut are the
same rows with the names changed. A rule that bounds C from those rows also
lets any revoked device bound any honest device, on every replica, for the
price of one op. The information that separates the two cases is *when* each
replica met the rows, which is exactly what the ledger forgets. One replica
did see `A -> C` while A was still a member. That observation has to travel.

## Decision

### 1. The bound travels as its own control op

A new control op, `device.read_bound`, on the vault-meta stream, carries:

- `device_id`: the device bounded;
- `basis`: the device id of the author of the ordinary revocation pair whose
  row put `device_id` in this replica's register.

It is a claim about this replica's fold, and nothing else: "my fold bounded
`device_id`, from the pair `(basis, device_id)`." It writes no register row,
names no cut and no reason, and is not a revocation. The register stays the
fold ADR-0041 defines, a pure function of the ledger.

### 2. Who emits one, and when

A replica emits `device.read_bound` in the same transaction in which its own
fold first writes a `device_read_bounds` row for a device, under all of these
conditions:

- **The row came from this replica's own fold**, from an ordinary row that was
  ungated at that fold. That is every row the fold's upsert inserts. A row
  adopted from a sponsor (`from_sponsor = 1`) is not emitted, because the
  sponsor folded it, and the sponsor emits it.
- **The register names the device through an ordinary row.** A device the
  register names through an authority row (`by_authority`) is not emitted,
  because ADR-0058 already makes every replica bound it.
- **A row a claim wrote is never re-emitted** (§5). A replica speaks only for
  what its own fold saw.
- **This replica is not itself gated at that fold.** A replica the ledger
  already revokes would emit a claim every receiver discards, and it would
  emit it under the key the account has just stopped trusting.
- **The vault requires the feature** (§7).

When the vault first requires the feature, each replica emits one claim for
every bound its own fold wrote earlier (`from_sponsor = 0`, not from a claim)
whose device some live ordinary pair in its ledger names, with that pair's
author as `basis`. Without that backlog the divergence that already exists in
a vault never heals, because no fold will bound C again. A bound whose device
no live pair names is not emitted: it is either an orphan that
`release_orphan_read_bounds` will take back, or one a withdrawal released.

### 3. Who is believed

A replica stores every `device.read_bound` op in a ledger of its own, keyed
on `(sender, device_id, basis)`, whatever the sender's standing. The fold
reads that ledger, as it reads `device_revoke_ops`, so whether a claim is
believed does not depend on whether it arrived before or after the rows
around it. At each fold a stored claim bounds `device_id` when all of these
hold:

- **(i) The sender is ungated at this fold.** Its revoker set, after the
  discount ADR-0041 defines, is empty. There is no mutual-pair exception: a
  claim names no revoker, so there is no "only party to have revoked it" to
  exempt.
- **(ii) The basis pair is live in this replica's ledger.** The ordinary pair
  `(basis, device_id)` exists and its head is a revocation (ADR-0056 §2),
  gated or not. Until that pair arrives the claim waits in the ledger and
  bounds nothing, and once the pair's head is a withdrawal the claim is
  inert.
- **(iii) The sender does not name itself.** `device_id != sender`, refused
  at apply, like a self-naming revocation.

A believed claim upserts into `device_read_bounds` like the fold's own rows,
with the ratchet's rules unchanged: no later fold releases it, except the
withdrawal release ADR-0056 §4 defines. That release still fires only when no
live pair names the device. Condition (ii) means that a claim can then bound
nothing, so the two rules never fight over a row.

### 4. What converges, stated as the condition

The read bound converges for a device T when at least one replica that bounded
T from its own fold emitted a claim, and that replica's revoker set is empty
in the final ledger. In #1167's case that is any replica other than A that
applied `A -> C` before `B -> A`, so both replicas in
`the_register_is_the_same_whichever_order_the_two_revocations_arrive` end
with C bounded, and its pinned divergence becomes `assert_eq!`.

What it does not close: if every replica that bounded T from its own fold is
itself revoked, and a replica learns of that revocation before the claim, it
discards the claim and keeps T unbounded. Claim (i) reads the sender's
standing at the fold, which depends on arrival order, as gate (c) of the
withdrawal release does. It is a strictly smaller gap than #1167's: it needs
every witness to be expelled, not merely the revoker.

### 5. Claims are not re-broadcast

A bound written by a claim is marked as such in `device_read_bounds`. It is
never emitted, neither by §2 nor by the backlog. Re-broadcast would launder a
claim past its sender's revocation. S claims T; R believes it while S is
ungated and re-emits it as R; S is later revoked; every replica still believes
T on R's word. That makes gate (i) worthless. A claim is evidence of one
replica's own observation and stays attributed to that replica.

The mark does not change the readmission count in the `DeviceCertPublish`
arm, which counts every row with `from_sponsor = 0`. A believed claim needs a
live basis pair in this replica's own ledger (ii), so the account has expelled
something by the same evidence the count already reads. A later fold of this
replica that bounds the same device clears the mark, as it clears
`from_sponsor`, and the row becomes this replica's own.

### 6. Bounded storage

A sender keeps at most `REVOKE_TARGETS_PER_SENDER` rows in the claim ledger,
evicted by the rule `cap_device_revoke_targets` applies to a sender's
revocation pairs. An honest replica emits one claim per device its own fold
bounds, plus at most one per device in §2's backlog, so it reaches the cap
only when the account has revoked that many devices. The claim ledger is then
at most the cap times the number of senders, and so is each fold's extra
work.

### 7. The op is emitted only behind a structural feature

The op belongs to a new structural feature, and is emitted only into a vault
that requires it, following ADR-0045 §7 the way ADR-0056 §7 does for the
withdrawal. Unlike the withdrawal, a build that parks the op loses nothing it
had. It keeps the bound it had, and it converges when it upgrades and drains
the parked op. So enabling the feature needs no refusal while another device
lacks it, and no user confirmation. The feature gate is there so that a
fingerprinted schema version names the op kind, as every op kind has one. A
new `DOC_SCHEMA_V`, fingerprint and storage migration land with the
implementation, each taking the next number free on master when it merges.

## Threat analysis

The op can only ever *withhold* keys. It writes `device_read_bounds` and
nothing else, and no rule here releases a bound. So nothing below can disclose
a key to anyone. Every cost is availability: an honest device stops being
sealed new epochs. The failure this record closes is a disclosure, so where
the two trade, this record takes the availability cost.

**T1 — An honest replica amplifies an over-bound.** This is the threat #1167
named. X, a device the account is about to expel, revokes the honest device V
while it is still a member. An honest replica R applies `X -> V` before it
learns of `O -> X`. R's fold bounds V, and R, which is ungated, emits a
claim. Every replica then bounds V, including the ones that met `O -> X`
first and would have left V unbounded. Before this record the over-bound was
R's alone; now it is the account's. V reads `current` in the device list
(`DeviceRow::read_bounded` says why it receives no keys) and has to be paired
again under a new device id to receive new epochs.

How wide the window is:

- R has to apply `X -> V` before `O -> X`. A replica that syncs one relay in
  that relay's order meets them in the order the relay accepted them. Where
  the relay bound is in force (ADR-0063), the relay refuses X's later ops once
  `O -> X` drains into the relay's revocation. So the window is ops X wrote
  before its revocation reached the relay, plus whatever deferral or parking
  reorders. On the one line where the relay bound is off (a single-tenant
  self-host with no issuer), X's later ops still reach the relay, but a
  replica syncing in relay order still meets `O -> X` before them.
- Inside that window X was a member in every replica's view. Every replica
  online then applied `X -> V` the same way R did and bounded V on its own.
  What the claim adds is the replicas that were offline, met `O -> X` first,
  and would otherwise have kept sealing to V.
- No condition the receiver can check separates this case from #1167's. `A ->
  C` before `B -> A` and `X -> V` before `O -> X` are the same shape, as #411
  shows for the ledger. This record chooses to bound in both cases. A device
  that was not meant to be bounded loses access to new epochs. A device that
  was meant to be bounded receiving every epoch is the worse outcome.

There is no in-band remedy short of pairing V again. ADR-0056's withdrawal
releases a bound only on the word of an ungated author, and X is gated. A
withdrawal by the identity (ADR-0058 §6,
[#1239](https://github.com/justin13888/Sunrise/issues/1239)) is the
candidate for one, and this record does not build it.

**T2 — A revoked device emits claims.** Every replica that knows X is revoked
discards X's claims by (i). A replica that does not know yet needs the basis
pair live in its ledger (ii). X can supply one by writing `X -> V` itself, but
that replica would apply `X -> V` ungated and bound V from its own fold in the
same state. So a claim from X bounds nothing that X's own revocation row would
not, at the same replica at the same moment. It also cannot reach a device no
revocation row names.

**T3 — A claim is laundered past its sender's revocation.** Closed by §5:
claims are not re-broadcast, so a claim is only ever believed on its own
sender's standing.

**T4 — A sender floods the claim ledger.** Bounded by §6's cap, which is the
one the revocation ledger already applies to the same sender.

**T5 — A claim is replayed after the bound was released.** A withdrawal
releases a bound only when no live pair names the device (ADR-0056 §4 (b)).
In that state every claim for the device fails (ii), because its basis is a
pair naming the device. A claim stored before the release cannot bound the
device again unless a revocation naming it goes live again. If one does, the
fold would bound the device from that row anyway.

**T6 — A claim forges its sender.** `sender` is the device the sync path
authenticated, not a field of the op. That is the same binding
`device_revoke_ops.sender` rests on, and `refold_device_revocations`'
§"Why `sender` is the row's author" cites where it is enforced.

**T7 — An older build.** It parks the op (ADR-0045 §4) and keeps the bound it
had, so it is never worse off than today. It converges once it upgrades and
drains the parked op. The feature gate keeps the op out of a vault until
every device can at least park it.

## Alternatives considered

- **Leave it, and document the divergence.** This is the status quo. It keeps
  a disclosure that one arrival order produces and the other does not, on
  replicas that already exist, with no remedy short of revoking C again from
  every replica that missed it. No user can tell which replicas those are.
- **Believe a claim from any sender, whatever its standing.** This converges
  outright: the bound becomes a monotone function of the op set. It also lets
  a revoked device bound every honest device on every replica with one op
  apiece, which is the lockout ADR-0041's gate exists to prevent, moved to the
  keys.
- **Bound every device any live pair names, with no new op.** This is a
  ledger-only rule. #411 shows it is the same as the last alternative without
  even needing an op: a revoked device writes `X -> V` and V is bounded
  everywhere.
- **Rely on the authority signature alone (ADR-0058).** This is the preferred
  path where it applies, and it is why §2 does not emit for authority rows.
  It does not apply in an account with no reachable holder, or before the
  vault requires `core.revoke_authority`, and those are exactly #1167's cases.
- **Re-broadcast believed claims, so the bound floods to every replica.** This
  is rejected by §5. It turns gate (i) into a check on the first hop only.
- **Make the bound a fold again, released when the register unwinds.** This
  is the readmission disclosure `0028_device_read_bounds.sql` closed. An
  unwound device becomes a recipient again, and one `DeviceCertPublish`
  backfills it every held epoch.
- **Name the basis by the exact row (its HLC) rather than by its pair.** The
  compaction keeps only each pair's head (ADR-0056 §2), so a claim naming a
  superseded row would wait forever for a row the ledger dropped. The pair is
  what the fold reads, and its head is what (ii) asks about.

## Consequences

- `the_register_is_the_same_whichever_order_the_two_revocations_arrive`
  asserts that both replicas bound C, with `assert_eq!` on the bound as well
  as on the register.
- A vault that already diverged heals once it requires the feature, through
  §2's backlog, provided a replica that bounded the device from its own fold
  is still ungated.
- The over-bound of T1 becomes account-wide. A device revoked by a device the
  account later expels, inside the window above, stops receiving new epochs
  on every replica and has to be paired again. The device list already shows
  that state through `DeviceRow::read_bounded`.
- The release half of the per-replica bound is not changed. A replica that saw
  a withdrawal before learning its author was revoked has released the bound,
  and one that learned the order the other way keeps it (ADR-0056 §4,
  `release_withdrawn_read_bounds`). That divergence errs the other way and is
  not answered here.

## What would force revisiting this

1. An identity-signed withdrawal (#1239) lands. T1's remedy then exists in
   band, and this record should say so.
2. A relay-side ordering guarantee is added that makes `O -> X` reach every
   replica before any op X wrote afterwards. T1's window then shrinks to
   deferral alone.
3. Evidence that T1 fires in practice more often than the divergence it
   replaces. The trade in §Threat analysis assumes the opposite.
