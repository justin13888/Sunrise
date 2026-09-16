# 0040 — A predecessor's successor places are held by rank, not by arrival, and are re-judged when the predecessor is established

**Status:** accepted

**Amends** [ADR-0037](./0037-identity-transition.md) §Consequences, whose final
paragraph claims the ingest `prev_sig` check keeps the sibling cap from becoming
a suppression tool. It does not, and the paragraph's own "whenever this replica
has already established the predecessor" is where the claim fails. Amends
[`docs/03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) §Verification
for the same sentence.

No schema change, no migration, no new op, no primitive change: `STORAGE_V`,
`DOC_SCHEMA_V`, `CRYPTO_SUITE_V` and `ENVELOPE_FORMAT_V` all stay where they
are. What changes is which rows one predecessor keeps, and when they are
re-judged.

## Context

`MAX_SIBLINGS_PER_PREDECESSOR = 16` bounds how many `identity_transitions` rows
one predecessor may accumulate, and `MAX_SIBLING_CANDIDATES = 16` bounds how
many the fold verifies at that link. ADR-0037 §Consequences justified the first
by the second and by a third thing: that a row the fold will certainly reject
cannot take one of the sixteen places, because ingest checks `prev_sig`.

That check runs inside `if let Some((_, from_pub)) = chain.iter().find(…)`. It
runs only when the predecessor is already on this replica's chain — and it has
to, because ADR-0037 item 5 requires a replica to accept a transition two links
ahead of what it knows and there is no key to check against until it catches up.

The cap below it counted **rows**. So the check was absent in exactly the case
where a forged sibling is cheapest, and the cap was a first-sixteen-to-arrive
queue over rows nothing had verified.

Anyone who can write to the account — every device the vault has admitted,
including one a rotation is about to cut, since ADR-0034 bounds a revoked
device's reads and not its writes — could emit sixteen transitions naming a
predecessor the target replica had not yet folded to. `prev_sig` on each is
whatever its author chose; `next_sig` covers it, and is taken under a successor
key the author mints, so a forgery is one Ed25519 signature over an empty
roster. All sixteen stored. The honest successor then arrived and was refused,
and the refusal was **permanent**: the op is recorded in `ops`, the sync cursor
advances, and nothing ever offers it again.

The fold was never fooled — it steps past a row whose `prev_sig` does not verify
— so no forged identity was adopted. The account simply could not adopt the real
one. A rotation could not land, and since a revocation *is* a rotation, a
revocation could not land either: the same unleavable state
`MAX_TRANSITION_CHAIN = 64` produced, one level down.
([#232](https://github.com/justin13888/Sunrise/issues/232).)

Reading the arrival-order queue adversarially turns up a second defect with no
adversary in it. ADR-0037 item 2 says standing is a pure function of the op set.
It was not: two replicas holding the same twenty transitions off one predecessor
kept the sixteen that reached them first, which is a different sixteen on each,
so they could fold to different heads and stay there.

## What cannot be fixed, and why the shape below is the best available

**No admission rule can tell an honest successor from a forgery while the
predecessor is unknown.** That is what "unknown" means: the one thing separating
them is a signature by a key this replica does not have. So any bounded buffer
of unjudgeable rows can be monopolised, and refusing the sixteen-and-first row is
a coin toss between the honest one and a forgery. Moving the buffer does not
help — a parking area for refused transitions is a bounded buffer of
unjudgeable rows too, and the argument recurses.

What *is* available is to make the sixteen places cost something an adversary
cannot mint, and to stop the loss being permanent.

## Decision

**A predecessor's sixteen places are held by the sixteen greatest rows in the
fold's own order. A row arriving at a full register displaces the weakest if it
outranks it. When a predecessor becomes established, every row stored under it
is re-verified and the ones that fail are deleted.**

1. **`SIBLING_ORDER_DESC` is one string**, used by `Engine::chain_identities`'
   `LIMIT` and by `Engine::admit_sibling`'s eviction. The row ingest discards is
   by construction one the fold would never have reached, which is what makes
   `MAX_SIBLING_CANDIDATES >= MAX_SIBLINGS_PER_PREDECESSOR` sufficient rather
   than merely necessary. `SiblingRank`'s derived `Ord` says the same order in
   Rust; `the_rank_type_and_the_sql_order_agree` is what stops the two drifting.

2. **`to_identity_id` is appended to that order.** Without it the order is
   partial — two rows from one device at one HLC tie — and both SQLite's choice
   among ties and an eviction decision would be arbitrary, differently on two
   replicas holding identical rows. It is the table's primary key, so with it
   the order is total.

3. **`meta_epoch` sorting first is what closes the attack**, and it is ADR-0037
   §4's argument applied one level down. A device cut by a rotation holds no
   vault-meta key above the epoch it was cut at, because `revoke_device` writes
   the revocation register and mints before `rotate_identity` runs. Its rows
   therefore sort below the rotation that cuts it however far ahead it dates its
   HLC — so it can no longer spend the places that rotation needs. An HLC is a
   claim; an epoch is a key you either hold or do not.

4. **Establishing a predecessor deletes what it does not sign.**
   `Engine::purge_unverifiable_siblings` runs over the links an arriving
   transition newly reached, and over the named predecessor before its own row
   is judged. Deleting is permanent and sound: `from_identity_id` is
   `identity_id_from_pub` of the key a row must verify under, so the id
   determines the key, and a row that fails once fails under every chain state
   any replica could ever reach. This is what converges — a replica that
   admitted forgeries while it was behind holds none of them once it catches up,
   and the honest successor is judged against an empty register whichever order
   the two arrive in.

5. **Refusal keeps its event.** A row that is outranked at a full register is
   still `core.identity.transition_rejected` with `reason = "siblings"`; the two
   new facts get their own names, `core.identity.sibling_evicted` and
   `core.identity.siblings_purged`, because "a register is under pressure" and
   "a register was full of rows nobody signed" are different news.

## Consequences

- **The suppression is closed against the adversary it mattered for.** A device
  a rotation excludes cannot outrank that rotation, whatever it emits and
  whenever it emits it, in any delivery order. `forged_siblings_cannot_suppress_the_successor_of_an_unestablished_predecessor`
  and `establishing_a_predecessor_clears_the_siblings_that_verify_against_nothing`
  are the two halves.

- **Which rows a predecessor keeps stopped depending on delivery order**, which
  makes ADR-0037 item 2 true where it had not been.
  `what_ingest_keeps_is_what_the_fold_reads_whatever_order_it_arrives_in` pins
  it, and is red against the previous behaviour for that reason rather than for
  the suppression.

- **Cost.** `admit_sibling` adds one indexed count and, only at a full register,
  one row read plus one delete — no verification. `purge_unverifiable_siblings`
  costs at most sixteen Ed25519 pairs per identity per establishment, which is
  what the fold already pays at one link, and it is spent on the path that
  removes the reason to pay it again: a swept link costs one verification per
  fold thereafter instead of sixteen. The chain diff that triggers it is one
  extra `chain_identities` per ingested transition, alongside the two the arm
  already ran.

- **A residual, stated rather than closed.** A *current, unrevoked* member
  holding the live vault-meta epoch can still tie an honest rotation on
  `meta_epoch` and outrank it on an HLC it dates forward within `MAX_DRIFT_MS`.
  Sixteen such rows placed under an unestablished predecessor still turn the
  honest successor away on a replica that is behind. This is strictly the
  ADR-0034 position — a member's writes are unbounded — narrowed from "any
  device the vault ever admitted, permanently, on every replica" to "a current
  member, against replicas that are behind". It is not closed here because the
  only mechanisms that would close it are the ones §What cannot be fixed rules
  out, plus one that is not free (below).

## Alternatives considered

**1. Separate budgets for verified and unverified rows, with a verified row
displacing an unverified one.** Rejected: it does not address the case. While
the predecessor is unknown the honest successor is *itself* unverified, so it
competes in the unverified budget with the forgeries and is suppressed exactly
as before. The budget split only helps a successor that arrives after the
predecessor is established, which is the case the purge in item 4 already
handles — and handles by deleting rather than by reserving.

**2. Bound the budget per emitter rather than per predecessor.** Attractive and
deliberately not taken. It would close the residual above: a device that holds no
`ID_S_priv` — every device admitted by pairing since #105 — cannot mint further
device ids, so it could place one unverified row per predecessor rather than
sixteen. What it would cost is an honest case nobody has a use for yet but
nothing forbids: a device restored from a backup re-rotating from a predecessor
it has already succeeded would have its second row refused during the window
where nothing can check it, and refusal in that window is permanent for the same
reason the defect was. Rejecting a shape for an edge case is a weaker argument
than the three mechanisms above, so this is recorded as available rather than
wrong — and it is what a future revisit should reach for first.

**3. Park a refused transition and retry it when its predecessor lands.** This is
the only shape that would recover a successor that was *already* turned away,
and it is the one §What cannot be fixed rules out: the parking area is a bounded
buffer of rows nothing can judge, so it is filled by the same sixteen forgeries
and evicts the same honest row. It would move the problem, not solve it.

**4. Remove the cap and bound the fold alone.** Rejected, and it is the F6
finding in reverse. Unbounded rows per predecessor is unbounded storage from one
member; and the fold's `LIMIT` would then make the rows past sixteen permanently
unreachable, which is `MAX_TRANSITION_CHAIN = 64` again.

**5. Refuse a transition whose predecessor is unknown.** Rejected: it breaks
ADR-0037 item 5. A replica catching up on a chain it receives newest-first
would discard every link it has not yet reached, and since the discard is
permanent it would never reach them.

## What would force revisiting this

1. **The residual being exercised, or a bound on member writes landing.**
   [#82](https://github.com/justin13888/Sunrise/issues/82) (a convergent
   peer-side check) or [#80](https://github.com/justin13888/Sunrise/issues/80)
   (a relay-side write bound) would each remove the current-member half. Failing
   those, alternative 2 is the next mechanism to price.
2. **A device legitimately emitting two transitions from one predecessor.**
   Nothing produces it today — `rotate_identity` refuses unless the emitter's own
   identity is the head, and it adopts in the same transaction — and alternative
   2 depends on it staying that way.
3. **`meta_epoch` ceasing to be unforgeable upward.** Every part of this rests on
   ADR-0037 §4, which rests on the two-transaction ordering in `revoke_device`
   that `a_revocations_transition_is_sealed_above_the_epoch_the_cut_device_holds`
   pins. If that ordering is ever reversed, this ADR's item 3 collapses with it
   and the suppression returns against the cut device.
