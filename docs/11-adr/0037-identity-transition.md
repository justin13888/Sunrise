# 0037 — The account identity is a chain, and membership is derived from its head

**Status:** accepted

**Supersedes** [ADR-0032](./0032-revocation-cannot-bound-cert-issuance.md)'s
*decision* — it required identity rotation and deferred it — and keeps its
analysis intact. **Amends**
[`docs/03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) §Identity
rotation, which is corrected rather than implemented: as drawn it is
unimplementable.

Adds one op family (`identity_transition`), two migrations (0022, 0023), and
moves `DOC_SCHEMA_V` 5 → 6, `STORAGE_V` 21 → 23. No primitive changes, so
`CRYPTO_SUITE_V` and `ENVELOPE_FORMAT_V` stay where they are.

## Context

ADR-0032 established that revocation cannot bound certificate issuance, worked
through five candidate fixes, and concluded that only identity rotation removes
the capability rather than refusing its output. It then deferred it, and the
bypass stayed open with
`a_revoked_device_rejoins_under_a_fresh_device_id` asserting the round trip.

The capability a revocation would have to remove is `ID_S_priv`, which every
paired device holds. Rotating it means the account's trust root stops being a
value and becomes a **sequence**, and that is the whole of the design problem:
every membership question in the tree was phrased against a single identity
that could never change.

`key-rotation.md` §Identity rotation had a procedure for this, written before
anything implemented it. Two things in it cannot hold:

- It keeps `identity_id` **constant** across a rotation. It cannot be:
  `identity_id` is `BLAKE3.derive_key("sunrise.identity_id.v1", ID_S_pub)[..16]`,
  a derivation and not a field, so a new `ID_S` *is* a new id by construction. A
  body that asserted otherwise would be a claim no verifier could check.
- It carries an `effective_at`. `DeviceRevokePayload` already records why an
  emitter-chosen cut was tried and removed, and every reason applies harder
  here: an emitter-chosen cut on a revocation decided which of one device's ops
  to refuse; on a transition it decides which *identity* every op in the account
  verifies under. Bounded ahead of the op's HLC it takes effect nowhere;
  bounded behind it has nothing to anchor to, because `Hlc::observe` bounds the
  future and leaves the past open by design.

Its step 6 — "re-publish to the server's identity registry" — names a registry
that does not exist and a relay change that is not needed.

## Decision

**The account identity is an append-only chain folded from a fixed point. A
`DeviceCert` is admitted if it verifies under ANY identity on that chain;
it confers membership only while the device's row names the chain's head.**

The split is the decision. Everything else follows from it.

1. **Applying is unconditional.** A transition, and a cert issued under any
   chain identity, apply on every replica without a standing check. This is
   [ADR-0034](./0034-revocation-bounds-reads-not-writes.md)'s rule and the same
   argument `Engine::apply_remote` step 2 already makes: a replica that applied
   an op before the revocation arrived cannot un-apply it, and this engine has
   no projection rebuild, so a refusal at apply time makes two replicas with the
   same op set disagree forever.
2. **Standing is derived at every point of use**, from two stored values —
   `devices.identity_id` and the fold's head. It is never stored as a verdict,
   so it cannot go stale and two replicas holding the same ops always agree.
3. **The fold is `Engine::chain_identities`.** It walks genesis → head, taking
   each link's successors greatest-first by
   `(meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id)` and
   verifying both signatures before extending. A link that fails either is not a
   link. It takes the first candidate that *verifies* rather than verifying only
   the greatest, so a forged row cannot suppress an honest one by sorting above
   it.
4. **`meta_epoch` sorts first, and that is the security component.** A revoked
   device provably cannot raise it: `Engine::revoke_device` writes the
   revocation register *before* it mints, so `emit_key_envelopes`' anti-join
   excludes that device from every epoch minted in the same transaction, and it
   therefore holds no key above the one it was cut at. An HLC is a claim; an
   epoch is a key you either hold or do not. The HLC components break ties
   between honest concurrent rotations, which is all they are asked to do.

   Two things this rests on that are not stated in the sentence, and that an
   adversarial reading finds first:

   - **The honest rotation must sit *above* the shared epoch, not at it.** If
     the transition that a revocation drives were sealed under the epoch the
     departing device still holds, both rows would carry the same `meta_epoch`
     and the decision would fall to `hlc_physical_ms` — which is attacker-chosen
     within `MAX_DRIFT_MS`, and which the revoked device can therefore win.
     What prevents that is an ordering across two transactions:
     `revoke_device` rotates every stream (the vault-meta stream included) and
     commits, and only then calls `rotate_identity`, so the transition is sealed
     at E+1 while the excluded device holds nothing above E. Nothing in the type
     system says so, and the comment in `rotate_identity` asserted the opposite
     until this was checked, so
     `a_revocations_transition_is_sealed_above_the_epoch_the_cut_device_holds`
     pins it.
   - **A row's `meta_epoch` is not a free claim, even though an envelope's
     `epoch` field is.** `sunrise_core::engine`'s `DEFERRED_TOTAL_CAP` says
     "`epoch` is attacker-chosen", and it is — on the *deferral* path, which is
     reached because no key at that `(stream, epoch)` is held, so nothing has
     been opened. A row in `identity_transitions` is written only after the
     envelope opened under a key this replica holds at that exact epoch. The
     two statements are about the same field and different facts.

   What this does **not** claim: a revoked device's *writes* are bounded. They
   are not (ADR-0034, and `Engine::apply_remote`'s step 2 says so in terms). It
   can go on emitting ops, including transitions, at every epoch it holds. The
   claim is only that none of them outranks a rotation minted after its cut.
5. **Signatures are verified in the fold, not at apply.** `prev_sig` can only be
   checked against a predecessor the verifier has already established, and a
   replica may hold a transition two links ahead of what it knows. That op is
   legitimate and must be stored now and verified when its predecessor lands.
6. **The genesis is the account's stable name.** `identity.genesis_identity_id`
   (migration 0022) with `genesis_id_s_pub` beside it (0023). The identity *in
   force* moves on every rotation, and a chain read from a moving anchor is not
   a chain: a replica two transitions in would fold from its own second identity
   and call the third a stranger's. The genesis is what a user is shown as
   "your account" and what two devices compare to decide they belong together.

`RevokeDevice` keeps its shape and changes meaning: register, rotate every
Stream key, then rotate the identity with the revoked device left out. The
recovery code is carried forward by default — the successor is sealed to the
outgoing `ID_D_pub` — **except** when the device being revoked is the account's
creator, which holds `ID_D_priv`: carrying it would seal the successor to the
very key the rotation exists to exclude.

## Alternatives considered

**1. Keep `identity_id` constant and rotate only the keys, as key-rotation.md
draws it.** Impossible, as above: the id is a derivation of the key. Making it a
free-standing field instead would make it a value the emitter picks and no
verifier can check — exactly ADR-0032 alternative 1's defect, moved up a level.

**2. Store membership as a column, written once when a device is admitted.**
Rejected. It is the same class of mistake as a cached revocation decision: a
transition that lands later has to rewrite every row, and a replica that applied
the ops in a different order writes them at different moments. Deriving it costs
one integer comparison at each point of use and cannot drift.

**3. Refuse a transition at apply time unless it succeeds the current head.**
Tempting and wrong, for ADR-0034's reason. A transition naming an identity two
links ahead is legitimate — the replica simply has not seen the intermediate one
— and refusing it loses the op, because the relay does not redeliver. It is
stored, and the fold ignores it until its predecessor arrives.

**4. A per-transition `effective_at`, as key-rotation.md draws it.** Rejected
for `DeviceRevokePayload`'s reasons, worse. See Context.

**5. Rotate Stream keys as part of an identity rotation.** Not done, and
key-rotation.md is right about this: an attacker holding `ID_S_priv` can forge
ops going forward but cannot read content without a Stream key. A user who wants
both does both, and `RevokeDevice` does do both because a departing device *is*
assumed to hold Stream keys.

**6. Carry the predecessor's `ID_S_pub` in the transition body** instead of
storing the genesis key (migration 0023). Rejected: it would be a signed field
the verifier has no independent way to check, which is the same mistake as
trusting `DeviceCert.identity_id` without recomputing it. The verifier must
already know who it trusts; that is what makes `prev_sig` mean anything.

## Consequences

- **`a_revoked_device_rejoins_under_a_fresh_device_id` flipped**, with its setup
  byte for byte unchanged. Every step of #105's round trip still happens and the
  cert still verifies; it is genuine under an identity the account has retired.
  The row is admitted, `current` is false, and it is a recipient of nothing.
- **Two guards, two jobs.** The backfill membership test stops the *initial*
  hand-back; the recipient query's `identity_id` clause stops every *subsequent*
  epoch — which is the failure ADR-0032 alternative 3 could not close. Each is
  independently constrained by tests: neutralising either fails a different one.
- **A device excluded from a roster is not told it was excluded.** It finds no
  share, adopts nothing, and reads as not-current — which is also exactly what
  an honest device looks like between applying a rotation and receiving its
  roster cert. `core.identity.not_in_roster` discloses it and nothing gates on
  it, for ADR-0032's reason: the two are indistinguishable inside the vault.
- **`ID_S_priv` still travels in `PairingPayload`**, so a revoked device can
  still self-certify — until the next rotation, which is now certain and
  automatic. The residual gap is bounded rather than open. Closing it entirely
  means the sponsor issuing the joining device's cert, which needs a second
  message the CLI's file-drop pairing and the one-shot UniFFI seam do not have.
  ADR-0032 alternative 2's over-block does **not** apply to that change as
  scoped — there would be no sponsor countersignature and no sponsor binding on
  the cert, so revoking a sponsor locks nobody out.
- **The device list gained `current`**, which is not the negation of `revoked`
  and must not be rendered as one.
- **The fold bounds work, not chain length, and that is a correction to this
  ADR.** As accepted, the walk stopped after `MAX_TRANSITION_CHAIN = 64` links
  and the note here called a truncated chain a fail-closed outcome. It is not an
  outcome an account can leave. Any holder of `ID_S_priv` — every paired device,
  and every device that ever was one — reaches 64 by rotating, and past it the
  head is pinned: later transitions are rows the walk never reaches, so no
  rotation takes effect, so no *revocation* takes effect, because a revocation
  is a rotation. Nothing recovers from it and nothing reports it; the head is a
  real identity with a real set of current devices under it.

  Length never needed a bound. The visited set is already a termination
  guarantee — each step adds a `to_identity_id` no step has added before, and
  that column is the table's primary key — so the walk cannot outlast the table
  whatever the table contains. What needed bounding was **work**, and it is now
  bounded where it is spent: `MAX_SIBLING_CANDIDATES` rows verified per link in
  the fold, `MAX_ROSTER_ENTRIES` devices per transition refused at ingest on
  length before any cert is decoded, and `MAX_SIBLINGS_PER_PREDECESSOR` rows
  stored per predecessor — the same number the fold will verify, so a stored row
  is never one the walk could not reach.

  The sibling cap needs one thing to not become a suppression tool of its own: a
  row that the fold will certainly reject must not be able to occupy a
  predecessor's place. So `prev_sig` is checked at ingest whenever this replica
  has already established the predecessor, in addition to the fold's check,
  which stands unchanged for the case the ADR's item 5 describes — a transition
  two links ahead of what this replica knows. A member can still write competing
  siblings; what it can no longer write is a sibling that is not a candidate.
  See `docs/03-crypto/key-rotation.md` §Verification.

## What would force revisiting this

1. **`ID_S_priv` leaving `PairingPayload`.** It removes the capability this ADR
   bounds rather than removes, and would make the rotation-on-revocation a
   defence in depth rather than the whole defence.
2. **A relay-side write bound** ([#80](https://github.com/justin13888/Sunrise/issues/80)).
   A revoked device that cannot upload cannot publish a cert, which bounds the
   bypass before the fold ever sees it.
3. **Sharing with peers outside the account.** `share_grant` is signed by the
   account identity, so a rotation invalidates every outstanding grant.
   key-rotation.md's step 5 says to re-emit them; nothing implements sharing
   yet, and when it does this ADR's fold is where "which identity signed this
   grant" has to be answered.
