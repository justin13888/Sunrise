# 0058 — The account identity is the authority for membership, and its revocations are never gated and never discounted

**Status:** accepted

**Answers** [#394](https://github.com/justin13888/Sunrise/issues/394).

**Amends** [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) §"What a user
sees" item 4, which says no ledger-only rule settles two expelled devices one
attacker holds. That stays true. This record adds the one input the ledger did
not carry, and leaves the gate, the discount and the walk unchanged for every
row that does not carry it.

**Answers** [ADR-0056](./0056-a-revocation-is-withdrawn-only-by-its-author.md)
§"What would force revisiting this" item 4, which named this authority as the
one candidate that could speak for revocations it did not make. §6 below
re-argues ADR-0056 §3 against it.

**Depends on** [ADR-0045](./0045-schema-identity-and-feature-gating.md) §4
(parked ops, built by #320) and §7 (`vault_requires` and `DeviceFeatures`,
built by [#324](https://github.com/justin13888/Sunrise/issues/324)). The
reason is the one ADR-0056 §7 gives for its own op kind, and §7 below applies
it.

## Context

`refold_device_revocations`
(`crates/sunrise-core/src/engine/revocation.rs`) derives the register from the
ledger `device_revoke_ops`. It builds `revokers_all`, the map of who revoked
whom, from every row. It then discounts `s` from `v`'s set when a row revokes
`s` from a sender other than `v`. The walk skips a row whose sender keeps a
revoker other than the row's own target.

The discount counts every stored row, gated or not. ADR-0041 §"What a user
sees" item 4 works the consequence, and
`a_second_expelled_device_discounts_the_third_device_that_revoked_the_first`
pins it:

1. O revokes X1 and X2. One attacker holds both.
2. X1 revokes O. The mutual exception lands it. O's revocation of X2 unwinds,
   and O is discounted out of X2's set.
3. T, a current device the attacker never reached, revokes X2. X2 is gated
   again.
4. X1 revokes T. That op is gated and revokes nobody. It is still a row
   revoking T from a sender other than X2, so it discounts T out of X2's set.
5. X2 revokes any device, T included, and the revocation lands.

Each honest revoker costs the attacker one op. #394 and ADR-0041 item 4 show
why no rule over the ledger alone closes it:

- **One more level of the gate** on the discounting rows is beaten by one more
  attacker op. The limit is the fixpoint ADR-0041 §Alternatives (f) declines.
- **Requiring the discounting sender to have no other revoker** reopens the
  account-wide lockout of #240 for two attackers.
- **With k attacker devices and k honest ones** revoking each other, the
  ledger is symmetric. A rule that reads only the ledger cannot tell the two
  sides apart, so it either locks the account out or hands it over.

What breaks the symmetry has to come from outside the ledger. The account
already has one asymmetric party: the holder of `ID_S_priv`, the account's
signing key.

- **Since #221 only one place holds it.** That is the device that created the
  account, the device that emitted the identity transition now at the chain's
  head (the share addressed to the emitter carries the successor's secret,
  `docs/03-crypto/key-rotation.md` §Identity rotation), or a vault restored
  from the recovery code (`docs/03-crypto/recovery.md`). A device admitted by
  pairing never holds it.
- **No replica can name that device.** `identity.minted_by_device_id`
  (migration 0019) is written on the founding vault and left `NULL` on every
  paired one. A `DeviceCert` carries no issuer field. So a rule of the form
  "rows whose sender is the creator" cannot be evaluated by the replicas that
  need it.
- **Every replica can check a signature by it.** The identity chain
  (ADR-0037) gives every replica the `ID_S_pub` of the identity in force, and
  the chain is a fold of the op set.

## Decision

### 1. The authority is the identity in force, proven by its signature

**A revocation signed by the `ID_S_priv` of the identity at the head of the
chain is an authority revocation.** The authority is the key, not a device.
The fold never asks which device holds the key. It asks whether the row's
signature verifies under the head's `ID_S_pub`.

Every other revocation is an ordinary revocation, and ADR-0041 governs it
unchanged. That includes an unsigned revocation from the device that holds the
key.

### 2. The wire: a new op kind that names the identity and signs the claim

The authority revocation is a new control op,
`DeviceRevokeByIdentity { revoked_device_id, reason_code, identity_id, identity_sig }`,
with inner kind `device.revoke_by_identity`. It is sealed under the vault-meta
stream like `device_revoke`.

The signature covers the claim and the envelope that carries it:

```text
claim        = canonical_cbor({identity_id, sender_device_id, revoked_device_id,
                               hlc_physical_ms, hlc_logical, reason_code})
identity_sig = Ed25519(ID_S_priv, "sunrise.device_revoke.identity.v1" || BLAKE3(claim))
```

`sender_device_id` and the HLC are the authenticated envelope's, so the
signature cannot be lifted into another device's op or re-dated. The fold
recomputes `claim` from the ledger row, which already stores all of them.

It is a new kind rather than a field on `device_revoke`, for ADR-0056
§Alternatives (d)'s reason. An older build ignores an unknown field and folds
the op as an ordinary revocation, which gates it differently. The two builds
would then converge on different registers.

### 3. The ledger keeps the signature, and the fold verifies it

`device_revoke_ops` gains `authority_identity_id` and `authority_sig`, both
`NULL` on an ordinary row. **`authority_identity_id` joins the row's identity
and the pair key.** Without it, the pair compaction (#250) would keep only the
greater-stamped of an ordinary row and an authority row from one sender to one
target, and could delete the authority row. The per-sender cap (#315) counts
an authority row toward its sender's cap, like any row.

**The fold verifies the signature, not the apply.** A replica can hold an
authority revocation signed under an identity whose transition has not
reached it yet. That op is legitimate and must be stored now. ADR-0037 §5
verifies transition signatures in the fold for the same reason. A row is an
authority row **at a given fold** when both of these hold:

- `authority_identity_id` is the head that `Engine::chain_identities` folds;
- `authority_sig` verifies under that head's `ID_S_pub` over the recomputed
  `claim`.

A row that fails either check is folded as an ordinary row from its sender.

The head is a function of the op set (ADR-0037 §2), so the fold stays a pure
function of the op set, as ADR-0034 corollary 3 requires. Applying an
`identity_transition` therefore re-runs the revocation fold, because a moved
head changes which rows are authority rows.

### 4. What an authority row does in the fold

**It is never gated.** The walk does not ask about the sender's standing. The
authority is the key, and a device's standing says nothing about the key.

**It seats a revoker that is never discounted.** In `revokers_all`, an
authority row adds the sentinel `IDENTITY` to its target's set. The discount
never removes `IDENTITY`. `IDENTITY` is no device's id, so a device with an
authority revocation always has a revoker other than the device its own row
names. **The mutual exception therefore never reaches it**, and every row it
writes is gated.

**It discounts like any row.** An authority revocation of `s` is a row
revoking `s` from a sender other than `v`, for every `v`. So it discounts `s`
out of every set `s` sits in.

The five steps of §Context, with O holding the key and signing its two
revocations:

1. O's authority rows seat `IDENTITY` in the sets of X1 and X2.
2. X1's revocation of O is gated, because X1 has the revoker `IDENTITY` and
   `IDENTITY` is not O. O stays current. X1 is discounted out of O's set by
   the authority row revoking X1.
3. Steps 3 to 5 change nothing. X1's and X2's rows stay gated and revoke
   nobody. They can discount ordinary revokers, but never `IDENTITY`, so
   X2 keeps a revoker nothing removes.

The holder can also close the shape after the fact. O is a paired device in
the original sequence and cannot sign, but the holder H revokes X1 and X2 by
authority at any later point. Nothing X1 or X2 writes can take `IDENTITY`
out of either set.

**It lands in the register like any ungated row.** The register still takes
each target's greatest ungated row in the canonical order, so the cut and
`revoked_by` still follow the LWW rule. Whether the target is revoked no
longer depends on that order once an authority row names it.

### 5. When the key holder itself is lost or stolen

**Revoking the holder's device does not revoke the authority.** An ordinary
revocation of that device gates the device's ordinary rows, as today. Its
authority rows still verify under the head. Only an identity rotation retires
the key, and only a rotation can strip the authority.

**A stolen key gains nothing from this record.** Whoever holds `ID_S_priv`
can already rotate the identity with every honest device left out of the
roster, and can issue certificates. That is the creator residual ADR-0034
§Consequences and `docs/03-crypto/key-rotation.md` §Revocation already name.
An authority revocation is strictly less than that.

**The remedy is the recovery code.** A vault restored from it holds
`ID_S_priv`. It revokes the stolen device by authority and rotates the
identity with that device left out. ADR-0037 §4 orders the restored holder's
rotation above the stolen device's: `revoke_device` rotates the vault-meta
stream before it rotates the identity, so the transition is sealed above the
epoch the cut device holds. The stolen device's authority rows name the
retired identity, so from that fold on they are ordinary rows from a revoked
device, and the gate skips them.

**An account with no reachable holder has no authority.** That is an account
whose creator is lost and that has no recovery code. ADR-0041 item 4's residual
then stands as it does at HEAD.

**A rotation carries the authority forward.** A row signed under a retired
identity is ordinary from the first fold that sees the new head. Without a
carry-forward, every rotation would undo every authority revocation, and
step 2 of §Context would reopen. So `rotate_identity` emits, in the transaction
that emits the transition, one fresh authority revocation under the successor
for each device that both of these describe:

- the fold before the rotation holds an authority row naming it;
- the rotation's roster does not re-certify it.

The rotating device holds both keys, so it can sign the new rows. A device
the roster re-certifies is a member under the new head, and carrying a
revocation for it would contradict the roster the same transition signs.

The carry-forward does not judge the rows it carries. A vault restored after
a theft carries forward the authority revocations the thief signed before the
rotation, since the restored holder cannot tell them from honest ones. Those
devices are left out of the roster either way, because the register revokes
them, so they are not members under the new head (ADR-0037). The remedy is to
pair them again. That is the outcome a stolen creator already has.

### 6. The authority withdraws only its own revocations

ADR-0056 §3 rejects a third-party un-revoke as new authority. Its "What would
force revisiting this" item 4 asks whether the identity changes that. It does
not, and the authority gets no un-revoke of ordinary rows:

- **The identity is the author of its authority rows.** It may withdraw them,
  under ADR-0056's rule, with a withdrawal signed the same way under the
  current head. A withdrawn authority pair is inert, exactly as ADR-0056 §2
  folds any withdrawn pair.
- **It may not withdraw an ordinary row a device made.** That would be the
  third-party un-revoke §Alternatives (b) of ADR-0056 rejects. The identity
  does not need it: a device it wants reinstated, it can pair again, and a
  device it wants out, it revokes by authority.

The identity-signed withdrawal is built by whichever of #383 and this
record's implementation lands second.

### 7. The op waits for #324 and is emitted only behind a feature id

ADR-0056 §7 applies unchanged, with this record's op in place of the
withdrawal. A replica that dropped an authority revocation would fold a
different register for good. That is the split ADR-0042 forbids.

- **The op kind belongs to a structural feature, `core.revoke_authority`.**
  The feature registry records `device.revoke_by_identity` as the op kind it
  introduces. A client applies and emits `VaultRequires` naming it before its
  first authority revocation, as ADR-0045 §7's emission order requires.
- **The holder emits authority revocations only while every device that is
  not revoked advertises the feature.** Until then `Command::RevokeDevice` on
  the holder emits an ordinary `device_revoke`, as it does at HEAD, and its
  result says the revocation carries no authority and names the devices to
  update. Revoking is never refused for want of the feature. The device being
  revoked is often the one the user lost, and an ordinary revocation still
  does everything ADR-0041 gives it.
- **On the holder, a revocation is an authority revocation by default.** The
  user does not choose. `Command::RevokeDevice` signs when this device holds
  the head's `ID_S_priv` and the feature is required. A paired device cannot
  sign and emits an ordinary revocation.
- **The device list says which kind revoked a device.** `DeviceRow` gains
  whether the register's row for the device is an authority row, so a user
  can tell a revocation nothing can unwind from one a later op can.

The same change adds the two ledger columns in a new migration and bumps
`STORAGE_V`, and bumps `DOC_SCHEMA_V` for the op kind
(`docs/02-domain/schema-versioning.md`). Neither lands before the op, for
ADR-0056 §Alternatives (g)'s reason: a migration cannot be taken back, and a
fold branch no row can reach spends a storage version on nothing.

### 8. The relay checks nothing new

The relay cannot verify an authority revocation. Envelopes are opaque to it,
and it holds no `ID_S_pub`. An authority revocation queues the same
`relay_revocation_intents` row as an ordinary one, and the sync driver drains
it into the same `DELETE`. A relay check would need the relay to follow the
identity chain, and nothing in this record needs one: the authority decides
the register on every replica, and the register decides whether the intent is
queued.

## Alternatives considered

**(a) Authority by device: rows whose sender is the account's creator.** No
replica but the founding vault can evaluate it, because
`identity.minted_by_device_id` is `NULL` on every paired vault and certificates
carry no issuer. Publishing the creator's id would make it a claim, and a
claim needs a signature by the key anyway. It also keeps the authority with a
device after a rotation moved the key elsewhere.

**(b) Authority under any identity on the chain, not only the head.** A
rotation exists to retire a key. If a retired key kept the authority, a stolen
creator that the account rotated away from would go on revoking every device
it likes, for good. §5's carry-forward keeps what the account meant without
keeping the retired key's power.

**(c) Gate every row whose sender's certificate does not name the head,
instead of carrying authority rows forward.** After a rotation, every device
the roster left out would be gated, so the carry-forward would be unneeded.
It changes the standing of every ordinary revocation, not only authority rows.
It also ties the fold to certificate arrival as well as to the ledger. The
residual it addresses is only a rotation demoting authority rows, and §5
closes that inside this record's own op.

**(d) A field on `device_revoke` instead of a new op kind.** ADR-0056
§Alternatives (d) rejects it, and §2 above gives the reason for this op.

**(e) Land the columns and the fold now, and the op after #324.** ADR-0056
§Alternatives (g) rejects the same ordering for the same reason.

**(f) A quorum of current devices as the authority.** In the symmetric ledger
of §Context, k attacker devices and k honest ones produce equal counts. A
quorum needs a membership count it can trust, and that count is the register
the quorum would decide. It is the fixpoint ADR-0041 §Alternatives (f)
declines.

## Consequences

- **#394's attack closes wherever the holder signs.** In an account whose
  holder signs its revocations, two expelled devices cannot neutralise them,
  however many ops they write.
  `a_second_expelled_device_discounts_the_third_device_that_revoked_the_first`
  keeps pinning the ordinary case. The implementation adds a test that runs
  the same five steps with authority rows and asserts X2 stays gated.
- **The ordinary fold is unchanged.** Every row without a verifying signature
  is folded as ADR-0041 folds it today, so every test that pins ADR-0041
  stands.
- **The read bound converges for authority-revoked devices.** An authority row
  is ungated on every replica at every fold. So every replica that holds the
  row puts its target in the register and bounds it, whatever order the ops
  arrived in. That is a subset of
  [#411](https://github.com/justin13888/Sunrise/issues/411), not a fix for it.
- **The implementation is filed as its own issue**, linked from the pull request
  that adds this record. It depends on #324, like
  [#383](https://github.com/justin13888/Sunrise/issues/383). It covers the op,
  the `core.revoke_authority` feature and its emission gate, the migration,
  the fold's verification and `IDENTITY` revoker, the refold on a moved head,
  the carry-forward in `rotate_identity`, the `DeviceRow` disclosure, and the
  test above.

## What would force revisiting this

1. **Signing moves off a single holder.** An example is a threshold
   `ID_S_priv`, or one shared with paired devices again. §1's "the key, not a
   device" still holds, but §5's stolen-holder analysis would have to be
   redone.
2. **#324 lands a feature gate that differs from ADR-0045 §7.** §7's
   sequencing rests on it.
3. **The identity chain gains a fork rule other than ADR-0037 §4's.** §5's
   recovery argument relies on the restored holder's rotation outranking the
   stolen device's.
4. **The relay learns the identity chain.** §8 could then check the authority
   at the relay too.
