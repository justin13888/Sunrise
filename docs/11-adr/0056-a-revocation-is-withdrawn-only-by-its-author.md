# 0056 — A revocation is withdrawn only by the device that made it, and the withdrawal waits for unknown op kinds to be parked

**Status:** accepted

**Built by** [#383](https://github.com/justin13888/Sunrise/issues/383), which
waits on §7. Nothing in the engine emits a withdrawal yet.

**Amends** [ADR-0041](./0041-peer-side-revocation-is-a-fold.md) where it says an
un-revoke op "is what would settle" the residuals of §"What a user sees" item 4.
It would not settle them, and they do not need it. §3 below explains why. The
fold, the gate, the discount and the read bound are unchanged.

**Depends on** [ADR-0045](./0045-schema-identity-and-feature-gating.md) §4
(parked ops), which [#320](https://github.com/justin13888/Sunrise/issues/320)
builds, and ADR-0045 §7 (`vault_requires` and `DeviceFeatures`), which
[#324](https://github.com/justin13888/Sunrise/issues/324) builds. Parking
protects only builds that have it. A build that predates #320 still drops the
op kind this record adds, and only §7's feature gate keeps the op away from
such a build.

**Answers** [#241](https://github.com/justin13888/Sunrise/issues/241).

## Context

A revocation has no inverse at HEAD. `device_revocations` is a fold over
`device_revoke_ops` (ADR-0041, migration 0027). No command, seam export or
client surface removes a row from that ledger or cancels one. A device revoked
by mistake stays revoked on every replica. The only remedy is to pair it again
as a new device. Pairing mints a fresh `device_id` and leaves the old row in
the ledger. Since #221, a paired device also holds no `ID_S_priv`, so the
re-paired device cannot sponsor, rotate the identity or seal a recovery blob.

The fold already removes revocations, but only when nobody asked it to. Once a
revocation's author is itself revoked, `core.device.revocation_unwound`
reports that the revocation is no longer believed. So a user can watch a
revocation vanish because a peer's op arrived late, and cannot remove one on
purpose.

#241 asks five questions. Each gets a numbered answer below:

1. Should an un-revoke exist?
2. Is it an inverse or a new fact?
3. Who may make one?
4. What happens to keys?
5. Does the relay's half reverse too?

## Decision

### 1. It exists, as a withdrawal, and it is narrow

**A device may withdraw a revocation it made itself. It cannot withdraw a
revocation another device made.** Nothing else in this record reinstates a
device.

"Pair it again" stays the remedy in exactly three cases:

- the device that made the revocation is lost or revoked;
- the revocation rotated the account identity away from the device (§4);
- the device was revoked by several devices and some of them will not
  withdraw.

### 2. An inverse of one claim, folded as a last-writer-wins pair

The withdrawal is a new control op, `DeviceRevokeWithdraw { revoked_device_id }`,
with inner kind `device.revoke_withdraw`. It is sealed under the vault-meta
stream like `device_revoke`. The ledger stores it as a row of the same
`(sender, revoked_device_id)` pair, with a `kind` column that says whether the
row is a revocation or a withdrawal. The fold reads each pair's **head**, which
is its greatest-stamped row:

- **If the head is a revocation, the pair is live.** It is treated exactly as
  ADR-0041 treats a row today.
- **If the head is a withdrawal, the pair is inert.** It contributes nothing
  to `revokers_all`, nothing to the discount and nothing to the walk.
- **A later revocation from the same sender makes the pair live again.**
- **If a revocation and a withdrawal carry the same stamp, the revocation
  wins.** This fails closed. It also makes the head a function of the row set
  rather than of which row `INSERT OR IGNORE` met first.

`kind` is part of the row's identity, so that tie cannot collapse two ops onto
one key. Migration 0027 explains the same argument for `revoked_device_id`.

**This keeps the register a pure function of the op set.** A withdrawn pair
reads exactly like a pair its sender never wrote. Two replicas that hold the
same ops fold the same register in any delivery order, as ADR-0034 corollary 3
requires. A tombstone that deleted ledger rows would not converge, because a
replica that received the revocation after the tombstone would have nothing
left to delete. §Alternatives (c) covers it.

The pair compaction (#250) and the per-sender cap (#315) already work on pair
heads. Their proofs in `crates/sunrise-core/src/engine/revocation.rs` only
need "a lesser row never wins" to stay true, and it does: the fold reads heads
only. A withdrawal counts as one of its sender's pairs toward the cap, like
any row.

### 3. The authority is the pair's own sender, and no standing check is needed for the register

The authority rule is **the sender of the pair**. It is enforced by the same
fact that bounds everything in ADR-0041: `sender` is the authenticated
`env.device_id`, and a device authors only rows whose sender is itself. A
withdrawal therefore changes only its author's own claims.

**The register needs no standing check.** A ledger with a withdrawn pair is
the ledger in which that sender never made the claim. So a withdrawal can only
reach a register its author could have reached by never revoking. That holds
whatever the author's standing, and it holds whether the withdrawal is dated
honestly or back-dated. The per-sender cap already let a device drop its own
claims by naming 256 new ids, so dropping one's own claims is not new
authority. The withdrawal is the precise, visible form of something the ledger
already allowed.

The keys are different. §4 gates the release of a read bound on the author's
standing, because keys handed out cannot be taken back.

**A third-party un-revoke is rejected** (§Alternatives (b)). It would be new
authority: any device the fold does not gate could reinstate any device. A
compromised device that is still current would then reinstate an accomplice.
The honest answer, a fresh revocation, would race it on stamps the attacker
chooses, and the attacker's forward-dating bound is `MAX_DRIFT_MS`.

**ADR-0041 did not need a third-party un-revoke either.** ADR-0041 §"What a
user sees" item 4 and the fold's doc said an un-revoke op would let the
account say which reading of a revoked revoker it meant. The account can
already say it, by revoking again from a third current device:

- **The mutual pair.** O and X have revoked each other, so both are out and
  each is gated against third parties
  ([#248](https://github.com/justin13888/Sunrise/issues/248)). A current
  device T revokes X, the one it believes is compromised. X's revoker set
  becomes `{O, T}`, and T is not discounted, so X's revocation of O is gated.
  T's row revokes X from a sender other than O, so X is discounted out of O's
  set. O's set is empty, so O is current and ungated again. The test
  `a_third_current_device_settles_which_half_of_a_mutual_pair_the_account_meant`
  pins this. O stays read-bounded on a replica that had bounded it (§4).
- **The chain** (O revokes X, P revokes O, Q revokes P). X is on the revoked
  list and ungated. A current device that revokes X adds a revoker nobody
  discounts, so X is gated again.

A withdrawal adds the one reading the ledger could not express: the author
itself saying "I did not mean it". So #248's "permanently" holds only in an
account with no third current device. ADR-0041 §"What a user sees" item 4
already names that condition for the lockout.

### 4. Keys: the register is not the bound, and a withdrawal releases the bound only under three conditions

A withdrawal changes the register. It does not, by itself, change
`device_read_bounds`, the per-replica ratchet the four key-distribution sites
read (migration 0028). A withdrawn device would otherwise show as current and
receive nothing, which is exactly an unwound device. So the fold releases a
device's read bound on this replica only when all three of these hold:

- (a) the device is absent from the register the fold just computed;
- (b) no live pair names the device anywhere in the ledger, whether that pair
  is gated or not;
- (c) at least one withdrawal naming the device comes from a sender the fold
  does not gate. The predicate is the walk's own: the sender has no revoker
  other than the device the row names.

Each condition keeps something out:

- **(b) keeps ADR-0041 item 5 intact.** An unwound device still has the live,
  gated claim of its expelled revoker, so it stays bounded. Only a device that
  every claimant has withdrawn from is released.
- **(c) keeps a revoked author from releasing an accomplice.** Suppose O
  revoked a stolen X while O was honest, and O was later stolen and revoked by
  P. O's withdrawal of X changes nothing in the register, because X was
  already unwound. It must not hand X keys either, and (c) is what refuses it.
  A compromised device that is still current can release the bound. That is
  no escalation: such a device holds every stream key and can hand them over
  directly.

**The withdrawing command re-seals what it holds, and says so.** In the same
transaction, the withdrawing device runs `backfill_key_envelopes` for the
device. That hands it every epoch the withdrawing device holds, including the
epochs minted while the device was revoked. This is the one place the missed
epochs come back. It is an explicit act by the device the user is operating,
so the command's result and the UI must say it. **Nothing about the rotation
is undone.** The epochs that were minted stay minted, and the device reads
them because it was sent them now, not because the cut was reversed. A
replica that never re-seals does not hand them over.

**What stays on this replica, stated rather than hoped away:**

- **The release is per-replica, like the bound.** A replica that learned the
  author was revoked before it saw the withdrawal fails (c) and keeps the
  bound. One that saw the withdrawal first has already released it. This is
  the non-convergence [#282](https://github.com/justin13888/Sunrise/issues/282)
  owns, reached one more way. It is disclosed on `DeviceRow::read_bounded`
  as the unwind already is.
- **Keys handed out are not taken back.** If a withdrawal's author is later
  found compromised, revoking the author does not re-bound the device it
  released. Revoke that device again.
- **A rehabilitated half of a mutual pair stays bounded.** Condition (b) is
  what keeps it bounded, because its revoker's claim is still live. §3's
  third-party settlement restores that device's standing and not its keys.
  #282 is where that belongs.
- **The identity is not restored.** Suppose the revocation rotated the account
  identity, which happens when the account's creator makes it (see
  `docs/03-crypto/key-rotation.md` §Revocation). The device's certificate
  then names a retired identity, and membership is
  `devices.identity_id == head` (ADR-0037). A withdrawal cannot issue a new
  certificate, so the device is not current and must be paired again. The
  command reports this case rather than claiming a reinstatement.

### 5. The relay's half reverses, and only on the word of the device that revoked it

A vault-only withdrawal leaves a device that the vault calls current and that
the relay still refuses with 401 where `require_device_sig = true`. So the
withdrawal reverses the relay's half too:

- **The command reverses the queued telling.** It deletes an undrained
  `relay_revocation_intents` row for the device in the same transaction, and
  queues a reinstatement intent that the sync driver drains the same way.
- **The relay records who revoked the device.** `DELETE
  /api/v1/devices/by-vault-id/{id}` records the calling device on the row when
  the request is device-signed.
- **The relay accepts a reinstatement only from that recorded device, and only
  signed.** It then sets `revoked = 0`.
- **Otherwise the relay refuses.** That covers a row the relay revoked with no
  recorded device: an unsigned `DELETE`, or a row revoked before the column
  existed. The client reports that the relay still refuses the device, and
  the remedy is to pair it again.

That is §3's authority rule restated where the relay can check it. A relay
that let any account device reinstate any other would be the third-party
un-revoke that §3 rejects.

Push tokens deleted by the revocation are not restored. The device registers
them again on its next session, as it does after any reinstall.

**The seam separates the two halves.** `Core::relay_revocation_pending` lets a
client tell "revoked locally" from "the relay has stopped accepting it"
(#160). The withdrawal gets the same pair of states.

### 6. What the user is told, before and after

**Until the withdrawal ships**, a revocation cannot be undone, and the
surfaces that revoke must say so:

- the CLI's `device revoke` help and its result, which this record's pull
  request changes;
- the Remove confirmation in the device list, which comes from
  `i18n/en.toml` and is
  [#384](https://github.com/justin13888/Sunrise/issues/384).

The sentence to carry is that the device is readmitted only by pairing it
again as a new device.

**Once it ships**, the device list offers "Withdraw revocation" only on rows
this device revoked. The confirmation states three things:

- the device is sent every key this device holds, including the keys minted
  while it was revoked;
- if another device also revoked it, it stays revoked until that device
  withdraws too;
- if the relay was told and cannot be told otherwise, the device must be
  paired again.

### 7. The op waits for #320 and #324, and is emitted only behind a feature id

At HEAD, a build that meets an op kind it cannot decode fails
`decode_inner_op`. `apply_remote_all` turns that failure into
`RemoteOpInvalid`, and `crates/sunrise-core/src/sync_driver.rs#is_corruption`
classes it as damage to the link. The op never reaches `ops`, and it does not
come back after an upgrade (ADR-0045 §Context item 1).

For most op kinds that loses data. For this one it also splits the register
between builds, permanently: an older replica never sees the withdrawal and
keeps the device revoked, while a newer one reinstates it. That breaks the
invariant ADR-0042 puts above every feature: merging vaults across client
versions must never break and must never lose data.

Parking alone does not close this. A parked withdrawal is retained and
replayed when its replica upgrades, which makes the register converge across
builds that have ADR-0045 §4 (#320). A build that predates #320 cannot park:
it drops the withdrawal as corruption, and nothing brings the op back. Those
builds never advertise a feature set, and ADR-0045 §7 ("Enabling a feature")
says the feature gate is the only thing that protects them. So the withdrawal is
gated on both:

- **Neither the op nor the feature lands before #320 and #324.** Parking makes
  the op survivable on a build that has it. `vault_requires` and
  `DeviceFeatures` are what tell the emitter which builds exist.
- **The op belongs to a structural feature, `core.revoke_withdraw`.** The
  feature registry records `device.revoke_withdraw` as the op kind it
  introduces. A client MUST apply and emit `VaultRequires` naming it before
  its first `DeviceRevokeWithdraw`, as ADR-0045 §7's emission order requires.
- **The withdrawing command refuses while any device that could still drop the
  op lacks the feature.** Those devices are every device the register does not
  name as revoked, plus the device being reinstated, which must fold its own
  reinstatement. If any of them has no `core.revoke_withdraw` in its latest
  `DeviceFeatures`, including a device that has never emitted one, the command
  fails with a typed error that names the devices to update.
- **There is no user override.** ADR-0045 §7 lets the user confirm enabling a
  feature over a device that lacks it, because that device then only turns
  read-only for the affected data. Here the device would drop the op and fold
  a different register for good, which is the split this section exists to
  prevent. A device the user cannot update is revoked first. Its revocation
  needs no feature, and a revoked device's register no longer decides who is
  current.

**One residual stays open, and it is ADR-0045's rather than this record's.**
A device paired after a withdrawal, on a build that predates #320, syncs the
vault-meta stream from the start and drops the withdrawal as it would any
other new op kind. The gate above cannot see a device that did not exist when
the command ran. ADR-0045 §7 has no rule that stops a build missing a required
feature from being paired into a vault, and every feature that adds an op kind
shares this hole. It is disclosed here, and closing it belongs to #324.

The same change bumps `DOC_SCHEMA_V`, because the change adds an op kind
(`docs/02-domain/schema-versioning.md`). It adds the `kind` column in a new
migration, and bumps `STORAGE_V` with it. Neither lands before the op does.
§Alternatives (g) explains why.

## Alternatives considered

**(a) No un-revoke. Pairing again is the remedy, and the ADR says so.**
#241 names this as defensible for a security control, and it is the state until
§7 is met. It is rejected as the permanent answer because its cost is not the
cost of a re-pair:

- the new device holds no `ID_S_priv` (#221);
- the old id stays in the ledger for good;
- the account already loses revocations nobody meant to lose, through the
  fold's unwind.

The attack-surface argument for irreversibility does not reach a withdrawal
that §3 limits to its author. It does reach a third-party un-revoke, and that
is rejected below.

**(b) A third-party un-revoke that cancels a named revocation.** It is
rejected on authority and on need. On authority, it grants reinstatement of
any device to any device the fold does not gate. Its fold would need a gate on
the un-revoker. The un-revoker then races the re-revoker on stamps the attacker
chooses, and a revoked accomplice comes back for as long as the compromised
device stays current. On need, §3 shows that the residuals ADR-0041 said it
would settle are already settled by revoking again from a third current
device.

**(c) A tombstone that deletes the ledger's rows for a device.** This is not a
function of the op set. A replica that receives the revocation after the
tombstone has nothing to delete, and it keeps the row. That is the
order-dependence that ADR-0034 corollary 3 forbids.

**(d) Carry the withdrawal in `device_revoke` itself, as a new `RevokeReason`
or an extra payload field.** Both are rejected:

- **A new reason variant** fails to decode on an older build, which is the
  same loss §7 describes, under a less honest name.
- **An extra field** is ignored by serde on an older build. That build then
  reads the withdrawal as a *revocation* and folds the opposite of what the
  author meant. The two builds would each converge on a different register.

**(e) Leave the read bound alone on a withdrawal.** The withdrawal would then
be cosmetic: a device shown as current and sent nothing, which is the unwind
state. The user would get the appearance of a reinstatement without the
substance.

**(f) Release the bound whenever the register stops naming the device.** This
reopens ADR-0041 item 5. The ordinary unwind would hand an expelled device
every epoch through one `DeviceCertPublish`. Conditions (b) and (c) of §4 are
what separate a withdrawal from an unwind.

**(g) Land the `kind` column and the fold now, ahead of the op.** A migration
cannot be taken back, and nothing would write the column until #320 and #324
land. The
fold would then carry a branch no row can reach, and a storage version would be
spent on it. §7 already fixes the order. The option that is cheaper to reverse
is to land the decision now and the schema with its only writer.

## Consequences

- **#241 is answered, and its implementation is**
  [#383](https://github.com/justin13888/Sunrise/issues/383). That
  issue depends on #320 and #324 (§7) and covers the op, the
  `core.revoke_withdraw` feature and its emission gate, the migration, the
  fold's pair heads, the bound release, the backfill, the command, the seam,
  the CLI and the relay reinstatement. The device-list copy that says revocation cannot
  be undone is [#384](https://github.com/justin13888/Sunrise/issues/384),
  and it does not wait for #320.
- **ADR-0041's "an un-revoke would settle it" is withdrawn.** A third current
  device already settles it (§3). The test that pins this sits beside the
  mutual-pair test that ADR-0041 cites.
- **[#248](https://github.com/justin13888/Sunrise/issues/248)'s "permanently"
  is conditional.** It holds only in an account with no third current device,
  and even there the half of the pair that a third device later restores stays
  read-bounded. That is #282's to close.
- **`docs/03-crypto/key-rotation.md` §Revocation** states that a revocation
  cannot be undone at HEAD, and records the withdrawal this ADR decides.

## What would force revisiting this

1. **#320 lands with a different parking contract.** An example is parked ops
   that are not replayed on upgrade. §7's convergence argument rests on replay,
   so this record's sequencing has to be re-derived. The same holds if #324
   lands a feature gate that differs from ADR-0045 §7, or closes the pairing
   residual §7 discloses.
2. **#282 makes the read bound a function of the op set.** §4's per-replica
   release then becomes an account-wide rule, and conditions (b) and (c)
   should be re-derived from that function rather than carried over.
3. **The relay learns the revoking device some other way**, for example by
   moving to signed-only revocation. §5's refusal for rows with no recorded
   revoker would then narrow or disappear.
4. **A second authority for membership appears.** An example is an
   identity-signed "reinstate" that the account's current identity holder
   issues. That is the one party that could legitimately speak for revocations
   it did not make, and §3's rejection of (b) would need re-arguing against it.
