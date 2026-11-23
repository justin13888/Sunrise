---
status: accepted
---

# Key Rotation

Three key types rotate, each with a different cost and cascade. Throughout this spec, "the rotating device" is the device the user initiated rotation from; it MUST be a paired, currently-authorized device.

## Implementation status

Each line names what exists; the section it points to holds the rule, and the ADR holds the rationale.

**Built:**

- §Stream key rotation, in full, over the hierarchy of [ADR-0024](../11-adr/0024-key-hierarchy.md): `Command::RotateStreamKey`.
- Backfill of every held epoch to a newly certified device: §A device certified after an epoch was minted.
- §Slow peers and out-of-order epochs, as written: no per-epoch barrier, a keyless op waits in `deferred_ops`.
- §Revocation steps 1 to 3: the read bound, per replica ([ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md)), and the relay write bound under the three conditions of step 3.
- Peer-side refusal of two control ops, `device_revoke` and a read-bounded sender's recipient claim: §Revocation ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md)). The share fold also counts no read-bounded sender's `share_grant` ([`sharing-with-others.md`](./sharing-with-others.md)).
- Disclosure of an incomplete rotation (`CommandResult::unrotated_streams`) and of a relay-pending revocation at the moment of the act: §Revocation steps 2 and 3.
- §Identity rotation, in full, with the cert admission rule ([ADR-0037](../11-adr/0037-identity-transition.md)), and carrying or retiring the recovery code ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)).
- Sponsor-issued certs, so no paired device holds `ID_S_priv`: [`pairing-and-onboarding.md`](./pairing-and-onboarding.md) ([ADR-0032](../11-adr/0032-revocation-cannot-bound-cert-issuance.md) records the rejected countersignature).
- Identity-signed revocation from the device that holds `ID_S_priv`: §Revocation ([ADR-0058](../11-adr/0058-the-account-identity-is-the-membership-authority.md)).
- Withdrawing an ordinary revocation, by its author only: §Revocation ([ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md)).
- Republishing the account's key bundle after every new key: §Revocation ([ADR-0069](../11-adr/0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md)).

**Not built:**

- Rotating the identity from a device that did not create the account: §Revocation, `core.identity.rotation_unavailable`.
- §Device key rotation (`device_rotate`); a device's `D_D_pub` is set once, as that section states.
- `share_revoke`, so no shared peer's access is revoked by a rotation: the re-seal leaves out only a peer whose grant has expired. `share_grant` and the re-seal are built ([#1318](https://github.com/justin13888/Sunrise/issues/1318), [`sharing-with-others.md`](./sharing-with-others.md)).
- A re-seal by a device that does not hold `ID_S_priv`: its rotation reaches no shared peer (`core.share.reseal_skipped`).
- §Revocation step 4: nothing wipes the revoked device's database or explains the situation on it.
- Peer-side refusal of a revoked device's entity writes, which is a decision rather than a gap ([ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md) §Why re-adding a peer-side refusal is not free).
- Standing disclosure of a relay-pending revocation: `DeviceRow` carries no pending flag, and the Apple notice keeps saying the relay has not been told after a sync drained the intent ([#160](https://github.com/justin13888/Sunrise/issues/160)).
- Standing disclosure of a refused reinstatement: `sunrise device withdraw` and the Apple withdrawal report read `Core::relay_reinstatement_pending` once, right after the withdrawal, when the answer is pending; the refusal the drain records later reaches neither `sunrise devices` nor a device row ([#1332](https://github.com/justin13888/Sunrise/issues/1332)).

## Device key rotation (cheap)

**Triggers.**

- User-initiated voluntary rotation (e.g. before international travel).
- Automatic rotation on a schedule (default off; opt-in; cadence values 30 / 90 / 365 days).

**Procedure.**

1. The rotating device generates new `D_S'`, `D_D'` keypairs.
2. It signs a fresh `DeviceCert'` for itself using `ID_S_priv` (still held by this device).
3. It emits a `device_cert` op (control envelope, signed by the **new** `D_S_priv'`).
4. It re-wraps each Stream key currently stored in `stream_keys` for this device under the new `D_D_pub'` (this is local-only; no peer involvement).
5. **When the overlap window closes**, it emits a `device_revoke` op for the **old** key. Not at rotation time with a future date: `device_revoke` has no `effective_at` field, and takes effect at the HLC of the op declaring it.

Ops signed by the old device key stand until that op is emitted; after it, every replica refuses them.

**A device's `D_D_pub` is set once, and a replica enforces it.** `device_id` is derived from `D_S_pub` alone, so the id does not pin `D_D_pub`. Before [#281](https://github.com/justin13888/Sunrise/issues/281) a second cert for a known id under a fresh `D_D_pub` rebound `devices.d_d_pub`, and the backfill that follows a cert sealed every held epoch to the new key; a roster entry in an `identity_transition` did the same. A cert that would move a stored `d_d_pub` is now refused whole (`core.device.cert_rejected`, `reason = "rebinds_key"`), a roster entry that would is skipped (`core.identity.roster_entry_rejected`), and both write the column through `COALESCE(devices.d_d_pub, excluded.d_d_pub)`, so a NULL left by a legacy adoption is still filled. A device that needs a new `D_D` therefore needs a new `D_S`, which is a new `device_id`. Building `device_rotate` means replacing this rule with signed evidence from the old key, not relaxing it. A recovery is not device-key rotation: it is a fresh vault with fresh device keys, and the old device is gone rather than superseded.

**Why rotate-now-revoke-later rather than a forward-dated cut.** A future-dated cut needs an emitter-chosen field in the payload, and that field is the whole of a revocation's meaning — so every bound on it has to be right, and none of them were (see [`DeviceRevokePayload`](../../crates/sunrise-core/src/control_op.rs)). Deferring the *op* instead needs no field, no bound and no new failure mode: the window is a local timer on the rotating device, and if that device never emits the op the old key simply stays valid, which is exactly the state it was in before the rotation started. It also means the overlap is what the rotating device actually observed rather than what it predicted 24 hours earlier.

**Where each half is enforced.** The relay's gate and the vault's are different
mechanisms and the spec has previously conflated them. At the relay, revocation
arrives through the account API and never through the op: `DELETE
/api/v1/devices/{device_id}` sets `revoked = 1` on the relay's own `devices` row
(`crates/sunrise-server/src/store/devices.rs`), `Store::active_device` filters
on it, and that refuses every subsequent signed request
(`crates/sunrise-server/src/api/signed.rs`) and ends a live SSE session with
`AUTH_DEVICE_REVOKED` (`crates/sunrise-server/src/api/sync/stream.rs`). It is a
time-less binary flag on relay metadata the relay already holds, not a content
check, and it refuses the *device's credential* rather than ops signed by a
superseded key. In the vault, the matching check is the receiving client's, and
since [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md) it exists for
three **control** ops and for nothing else: `device_revoke`, a read-bounded
sender's claim that a third device has been keyed
([#82](https://github.com/justin13888/Sunrise/issues/82), closed), and a
read-bounded sender's `share_grant`, which the share fold counts for nothing
([`sharing-with-others.md`](./sharing-with-others.md)). That is the
scope; §Revocation below states the first two gates in full. A revoked device's *entity*
ops a replica still applies like any other, which is a decision rather than a gap
([ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md)). The relay
cannot make either check for it: it does not open envelopes, and
`device_revoke` is an inner op sealed under the vault-meta Stream key.

## Stream key rotation (medium)

**Triggers.**

- A device with access to the Stream is revoked.
- A shared peer's access is revoked.
- User-initiated periodic rotation.

**Procedure.**

1. The rotating device generates `stream_key_<epoch+1>` (32 random bytes).
2. For every still-authorized sibling device, it produces a `key_envelope` op (HPKE single-shot to that device's `D_D_pub`).
3. For every still-authorized shared peer, it produces a `share_grant` op for the new epoch (HPKE single-shot to the peer identity's `ID_D_pub`). Built: `emit_key_envelopes` re-seals each epoch it distributes to the Stream's shares, leaving out any whose grant has expired at the rotating device's clock, and only a device holding `ID_S_priv` can sign one ([`sharing-with-others.md`](./sharing-with-others.md) §Revoking access).
4. From the next emitted op forward in this Stream, `epoch` in the envelope is the new value; ciphertext is encrypted under `stream_key_<epoch+1>`.
5. The previous epoch's key MUST be retained by the rotating device (and any still-authorized recipient) so historical ops remain readable. Past epochs are never deleted from local storage.

A rotation does **not** re-encrypt historical ops. Historical ops remain readable by anyone who held the previous epoch's key when those ops were created. This is an inherent limit of E2EE. If forward secrecy of historical content is required, the user must export the affected Stream, delete it, and re-create.

### Why we keep epochs

The op log for a Stream may contain ops from multiple epochs interleaved (a slow peer might still be emitting under the old epoch when the rotation lands, until they catch up). Receivers select the decryption key by `(stream_id, epoch)` from the envelope. Past epoch keys are kept in `stream_keys` indefinitely; they are encrypted under `vault_root` like the current key. The table is keyed `(stream_id, epoch, key_id)` and a decrypt tries every key at `(stream_id, epoch)`, so two devices minting one epoch concurrently both keep theirs.

### A device certified after an epoch was minted

Step 2 seals a new epoch to the devices certified when it is minted, and minting happens whenever a Stream is created, so a device certified later would be left out of that epoch permanently. A replica applying a `device_cert` therefore seals **every** epoch it holds to the newly certified device (`Engine::backfill_key_envelopes`), and `key_envelope_recipients` (migration 0018) records who has been sent what, so a backfill emits only what is missing and a re-published cert emits nothing.

- **Every epoch, not the live one per stream.** Current-epoch-only was the first shape, and it was wrong wherever a rotation landed between a device's pairing and its certificate: the device held the payload's epochs and the live one with a permanent hole between them, and ops sealed under the missing epochs sat in `deferred_ops` until the TTL expired them ([#107](https://github.com/justin13888/Sunrise/issues/107)).
- **Never to a read-bounded device**, or re-sending a cert would undo a revocation. The test is `device_read_bounds` and not the revocation register: the register is a fold and can stop calling a device revoked, and at that point one republished cert would hand the device every held epoch of every stream rather than only the ones minted since. Migration 0028 gives the bound its own table so it cannot be released that way.

### Slow peers and out-of-order epochs

- A device MAY emit ops under any epoch for which it holds the key.
- The relay does not look at the epoch at all — it knows of none. Epoch sits inside the sealed payload, and the only envelope type the relay can reach is `EnvelopeHeader {stream_id, device_id, seq}` ([`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md) §Why the boundary holds structurally). Nothing is validated against a "current" epoch because nothing on the relay could be.
- Receivers maintain a per-`stream_id` decryption-key cache keyed by epoch; ops are decrypted on receipt with the matching epoch's key.
- Out-of-order arrival across epochs: ops are applied in arrival order regardless of epoch; the LWW comparison key `(hlc, device_id, seq)` resolves any reordering, so arrival order does not change the converged state. There is **no per-epoch barrier**.
- The owner garbage-collects an old epoch's key only when **all** active devices' cursors have advanced past the last op signed under it (same rule as blob GC).

## Identity rotation (expensive)

**Triggers.**

- Suspected recovery code compromise.
- Suspected identity-key extraction (rare; requires escape from OS keystore).

Implemented (ADR-0037). `Command::RotateIdentity { keep_recovery_code }` is the
explicit form; `Command::RevokeDevice` does the same thing with the revoked
device left out of the roster.

**The account's stable name is `genesis_identity_id`, not `identity_id`.** The
identity *in force* changes on every rotation — `identity_id` is
`BLAKE3.derive_key("sunrise.identity_id.v1", ID_S_pub)[..16]`, a derivation, so
a new `ID_S` is a new id by construction. The genesis is what every replica
folds the chain from, what a user is shown as "your account", and what two
devices compare to decide they belong together. It lives in
`identity.genesis_identity_id` with `genesis_id_s_pub` beside it (migrations
0022 and 0023) and travels in `PairingPayload` fields 10 and 11.

**Procedure.**

1. The rotating device generates new `ID_S'`, `ID_D'` keypairs. The account's
   `created_at_ms` is carried forward unchanged: a rotation does not begin a new
   account.
2. It emits one `identity_transition` control op carrying the whole hand-over —
   the successor's public halves, a re-issued `DeviceCert` for every surviving
   device, and a per-device share sealed by HPKE to each of their `D_D_pub`.
   The share carries the successor's `ID_S_pub`, not its `ID_S_priv`: a
   rotation moves the signing key to nobody, or it would hand one to every
   device pairing withheld it from ([#105](https://github.com/justin13888/Sunrise/issues/105)).
   The one exception is the share addressed to the emitting device, which
   minted the successor and already holds the outgoing key. A share's presence
   is the roster signal, and `shares_digest` binds the set into the signed
   body; it does not distribute a capability (ADR-0037). One op, because the parts are not independently applicable: a
   replica that learned the new `ID_S_pub` without the certs would reject every
   device in the account. The wire shape is
   `sunrise_core::IdentityTransitionPayload`; the CDDL is in
   [`data-encryption-format.md`](./data-encryption-format.md) §Control ops.
3. The body both signatures are taken over is
   `{from_identity_id, to_identity_id, to_id_s_pub, to_id_d_pub, roster_digest,
   shares_digest}`, and the roster and shares are committed by BLAKE3 digest
   rather than carried inside it — so the signature covers the whole transition
   without putting kilobytes through Ed25519.

   ```text
   body_hash = BLAKE3(canonical_cbor(BODY))
   prev_sig  = Ed25519(OLD ID_S_priv, "sunrise.identity_transition.v1"      || body_hash)
   next_sig  = Ed25519(NEW ID_S_priv, "sunrise.identity_transition.succ.v1" || body_hash || prev_sig)
   ```

   `prev_sig` is the outgoing identity authorizing the hand-over, `next_sig` the
   successor accepting it. `next_sig` covers `prev_sig`, so the pair cannot be
   split: a successor signature lifted from one transition does not fit another
   transition of the same body signed by a different predecessor.
4. Receivers apply the op **unconditionally** and check only what is
   self-contained (ADR-0034, and see §Verification below). The signatures are
   checked by the *fold*, not at apply time, because `prev_sig` can only be
   verified against a predecessor the receiver has already established — and a
   transition naming an identity two links ahead is a legitimate op that must be
   stored now and verified when its predecessor lands.
5. A surviving device opens its share, adopts the successor, re-wraps its
   `identity` row under the new id and re-issues its own cert. It does **not**
   touch `D_S`/`D_D`: those are unchanged by a rotation and their at-rest AAD
   binds to `device_id`, not to `identity_id`.
6. What happens to the recovery code is the one choice
   ([ADR-0070](../11-adr/0070-a-recovery-code-survives-a-rotation-that-carries-it.md)).
   `keep_recovery_code: true` (`sunrise identity rotate --keep-code`, and every
   revocation) seals the successor's `ID_S_priv || ID_D_priv` to the
   **outgoing** `ID_D_pub` as well, so the user's existing BIP-39 code keeps
   working: a vault restored from it walks each rotation's carry share to the
   identity in force and certifies itself there. That carry reaches every
   holder of the outgoing `ID_D_priv`, so it is never what to use when one of
   them is the reason for rotating. `keep_recovery_code: false` (the CLI's
   default) retires the code: the transition is sealed under a vault-meta epoch
   minted for it with no identity copy, every Stream then rotates, the rotating
   device keeps the successor's `ID_D_priv`, and it seals a new recovery blob and
   replaces the relay's. Revoking the device that holds `ID_D_priv` forces
   no-carry regardless — the carry share is sealed to the very key being
   excluded — and the user must be told
   (`core.identity.recovery_code_invalidated`); `sunrise identity new-code` is
   how they get a working code back.
7. A fresh `share_grant` to every shared peer, for every Stream a rotation
   that retires the recovery code re-mints: those re-mints go through
   `emit_key_envelopes` and re-seal under the successor, whose signature the
   share fold accepts as the chain's head. The peer's `ID_D_pub` is
   unchanged; only the granting identity has new keys, so the share
   signature changes. A rotation that keeps the code re-mints no Stream and
   so re-seals nothing, and a share whose grant the retired identity signed
   still counts. Either way a grant counts only from a sender whose reads are
   not bounded ([`sharing-with-others.md`](./sharing-with-others.md)).

**A `DeviceCert` is admitted if it verifies under any identity on the chain,
and confers membership only while the device's row names the chain's head**
([ADR-0037](../11-adr/0037-identity-transition.md)). Applying stays
unconditional, which keeps it convergent (ADR-0034); standing is derived at
every point of use from two stored values, so two replicas holding the same op
set agree. Two guards enforce it: `Engine::backfill_key_envelopes` refuses the
initial hand-back, and `emit_key_envelopes`' `identity_id` clause bounds every
later epoch. So a cert a revoked device signs with an `ID_S_priv` the account
has since retired is admitted, current under nothing, and a recipient of
nothing. Since the sponsor issues every paired device's cert
([`pairing-and-onboarding.md`](./pairing-and-onboarding.md)), no device admitted by pairing holds `ID_S_priv` at all, before its
revocation or after it; the residual belongs to the device that created the
account (or restored it from the recovery code), and §Revocation states it.

### There is no `effective_at`, and no server-side identity registry

Both were in this section before anything implemented it, and neither survives.

An `effective_at` chosen by the emitter is the same mistake
[`DeviceRevokePayload`](../../crates/sunrise-core/src/control_op.rs) records
having tried and removed, and worse here: an emitter-chosen cut on a revocation
decided which of one device's ops to refuse, while on a transition it decides
which *identity* every op in the account verifies under. Bounded ahead of the
op's HLC it takes effect nowhere; bounded behind it has nothing to anchor to,
because `Hlc::observe` bounds a reading from the future and leaves the past open
by design. The cut is the op's own HLC, read off the envelope by every replica.

The step that re-published `{identity_id, ID_S_pub, ID_D_pub}` to "the server's
identity registry" named a registry that does not exist. It is not deferred: it
is not needed. Every replica learns the new identity from the op log, verifies
it against the chain it already holds, and the relay — which holds no Stream
keys — could not read the transition if it were sent one.

### Ordering, and why `meta_epoch` decides

Two devices can rotate the same identity concurrently, neither having seen the
other. Both transitions are real and both are retained; the fold picks the
winner by `(meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id)`,
greatest first.

**`meta_epoch` sorts first and that is the security component.** It is the
vault-meta Stream epoch the op was sealed under, and a revoked device cannot
raise it wherever the read bound reached it: `revoke_device` writes the
revocation register *before* it mints, so the recipient anti-join excludes that
device from every epoch minted in the same transaction, and it holds no key
above the one it was cut at. The scope is `device_read_bounds`'s and no wider —
per-replica, as §Revocation below states —
so this is the same scope as every other statement here about what a revoked
device cannot do, and not a stronger one. An HLC is a claim anybody can make; an
epoch is a key you either hold or do not. The HLC components break ties between
honest concurrent rotations, which is all they are asked to do.

The argument needs the honest rotation to be sealed **above** the shared epoch
rather than at it. A transition sealed under the epoch the departing device
still holds would tie on `meta_epoch`, and the tie is broken by
`hlc_physical_ms`, which that device chooses freely inside `MAX_DRIFT_MS`.
`revoke_device` gets this right by ordering two transactions: it rotates every
stream — the vault-meta stream among them — and commits, then calls
`rotate_identity`, which seals the transition under the epoch that rotation
minted. See [ADR-0037](../11-adr/0037-identity-transition.md) §4 for the two
assumptions this rests on and what it does not claim.

### Verification, and what a replica checks when

At **apply** time, structurally and nothing else: `to_identity_id` is the
derivation of `to_id_s_pub`, the roster decodes with unique device ids, every
share is the right width, and every roster cert verifies under the *successor*
(a roster is the successor's statement about who survives, not a list the
emitter wrote). Failures are logged and dropped — never returned — because the
envelope has already been accepted and failing the delivery would put a
well-formed op into the refusal path.

At **fold** time: both signatures, against the predecessor the walk has already
established. A link that fails either is not a link.

The walk **terminates** on its visited set — every step adds a `to_identity_id`
no step has added before, and that column is the table's primary key, so the
walk cannot outlast the table. It bounds its **work** at
`MAX_SIBLING_CANDIDATES` rows per link. It does *not* bound the chain's length,
and it did once, at `MAX_TRANSITION_CHAIN = 64`: past 64 rotations no later
transition was ever reached, so no rotation took effect, so no revocation took
effect either — a state an account could not leave and nothing reported. Length
was never what needed bounding.

Ingest holds the rest: a transition may name at most `MAX_ROSTER_ENTRIES`
devices (refused on length, before the first cert is decoded) and one
predecessor keeps at most `MAX_SIBLINGS_PER_PREDECESSOR` rows, the same number
the fold will verify, so no stored row is one the walk could never reach. A
transition whose `prev_sig` does not verify against a predecessor this replica
has already established is refused rather than stored.

Which `MAX_SIBLINGS_PER_PREDECESSOR` rows those are is the other half, and for a
while it was the wrong half ([ADR-0040](../11-adr/0040-sibling-admission-is-a-rank.md)).
The `prev_sig` refusal above cannot run for a predecessor this replica has not
established, which is exactly where a forged sibling is cheapest, and while the
cap kept the first sixteen rows to arrive, sixteen forgeries could take every
place and refuse the honest successor permanently. The places are now held by
**rank**: the sixteen greatest rows in the fold's own order
(`meta_epoch`, then the HLC pair, then the emitter, then `to_identity_id`), and
an arriving row that outranks the weakest displaces it. A device a rotation
excludes holds no meta epoch above its cut, so it cannot outrank that rotation
however it dates its HLC — the §Identity rotation ordering argument, applied to
admission rather than to the fold.

The moment a predecessor *does* become established, every row stored under it is
re-verified and the ones that fail are deleted. `from_identity_id` is the
derivation of the key those rows must verify under, so a row that fails once
fails forever; deleting it frees its place and costs no fold anything it could
have used.

### A device that was offline across the rotation

It defers the transition, because that op is sealed under the meta epoch the
rotation minted and the device does not hold it. This is not circular: the
`key_envelope` carrying the new meta epoch is itself sealed under the **old**
one, which the device can always open. So it absorbs that envelope, drains
`deferred_ops`, applies the transition, and takes its re-issued cert and its
share out of the roster — with no recovery code and no re-pairing.

A device that is **not** in the roster finds no share, adopts nothing, and reads
as not-current everywhere. That is the mechanism rather than a failure, and it
is also what an honest device looks like between applying a transition and
receiving its roster cert, so `core.identity.not_in_roster` discloses it and
nothing gates on it.

**Stream keys are NOT rotated as a consequence of identity rotation by itself.** Stream keys rotate only when a device or peer is revoked. If the user's reason to rotate identity is "I think someone has my identity key," that someone could only impersonate me going forward (forge ops); they could not read content unless they also held a Stream key. If the user wants both, they perform identity rotation followed by Stream key rotation per Stream, and the UI offers a single "rotate everything" affordance that does it.

Cost on a typical user (10 devices, 30 Streams, 5 sharing peers): O(devices + streams + peers) = ~45 control ops; wall time on a 4G connection a few seconds.

## Revocation

Revocation = removing a device from the identity. Triggered from any other still-authorized device.

**Procedure.**

1. The trusted device emits a `device_revoke` op for the target device, signed by the trusted device.
2. The trusted device performs a Stream key rotation **for every Stream the revoked device had access to** (all of them, today). This produces fresh `key_envelope` ops for sibling devices and `share_grant` ops for peers under the new epoch — but pointedly NOT for the revoked device.

   **The rotation can be incomplete, and it says so when it is.** `Keychain::rotation_set` builds the set from the `stream_id` columns of `stream_keys`, `streams` and `ops`, and a column that is not 16 bytes names no stream to mint an epoch for. Such a row is not padded out, which would file a key against the vault-meta stream, and it is not dropped in silence either, because the revoked device goes on holding whatever key it was last given for whatever the row refers to. `Command::RevokeDevice` rotates what it can, returns the rest on `CommandResult::unrotated_streams`, and emits `core.device.revoke_incomplete`. A client MUST disclose a non-empty list rather than report the device revoked, the same rule the relay half follows ([#160](https://github.com/justin13888/Sunrise/issues/160)). Failing the whole revocation instead would be worse: the device that is gone is the entire scenario.
3. The server stops accepting ops signed by the revoked device. **It cannot learn this from the op**, and it is not asked to. `device_revoke` is an inner op sealed under the vault-meta Stream key, and the relay holds no Stream keys — so `revoked_device_id` is ciphertext to it, not metadata it can read. The alternative, promoting `revoked_device_id` into the **cleartext envelope header**, is refused: it would tell the relay which of an account's devices had been revoked and when, for every account it serves, on an unauthenticated channel — the metadata leak the blind-relay property exists to prevent. So the relay is told **out of band**, by `DELETE /api/v1/devices/by-vault-id/{id}` ([`../06-server/api.md`](../06-server/api.md) §Two names for one device). The route takes the *vault-side* device id because that is the only name a revoking device holds; the older `DELETE /api/v1/devices/{id}` names a ULID the relay mints at registration and never sends back through the op stream, so no vault can address a peer with it.

   The call is queued, not made. `Command::RevokeDevice` writes a row to `relay_revocation_intents` in the same transaction as the op, and `relay_intents::drain` sends it once a session is up — because a revocation has to work with no network at all, which is the situation a user is in the moment they notice a device is gone. A refusal keeps the row and retries on the next session, logging `sync.device.revoke_not_relayed`, which says outright that the relay is still accepting the device. See [`../10-cross-cutting/log-events.md`](../10-cross-cutting/log-events.md).

   **Revoked locally and refused by the relay are two different states, and a client says which one it is in.** The first is true the moment the op commits; the second only once the queued `DELETE` has been answered, which needs a network the user may not have, and the second is the one someone pressing the button believes they are getting. The log distinguishes them (`sync.device.revoke_relayed` against `sync.device.revoke_not_relayed`), and `Core::relay_revocation_pending` answers it for one device. Both clients ask it at the moment of the act: the CLI on the revoke path, and the Apple account view in its revocation disclosure ([#160](https://github.com/justin13888/Sunrise/issues/160)).

   **A queued intent is owed only while the register agrees.** The register is derived, and a re-fold can unwind a revocation it believed (the "`device_revoke` is the one family where revocation reaches backwards" bullet below). The fold does not touch `relay_revocation_intents`, so an intent can outlive the register row it was queued beside. The drain therefore sends, and `Core::relay_revocation_pending` reports, only the intents whose device `device_revocations` currently holds — one query, `OWED_RELAY_REVOCATIONS_SQL` in `crates/sunrise-core/src/relay_intents.rs` — and holds the rest rather than deleting them, so a later fold that revokes the device again makes the intent owed again instead of leaving the relay behind the register ([#257](https://github.com/justin13888/Sunrise/issues/257)). A `RevokeDevice` the fold discards can then never read as pending, and its "this revocation tells the relay nothing" is exact.

   **An unwind does not reach a revocation the relay has already been told.** The drain deletes the intent on the relay's answer, and only a withdrawal sends the relay's inverse of the `DELETE` (its reinstatement route accepts only the revoker's signed word, ADR-0056 §5, and the `relay_reinstatement_intents` queue a withdrawal writes is what sends it); an unwind queues nothing, so a device whose revocation unwinds after that point stays refused at the relay wherever `require_device_sig` is in force. That is the same direction as the read bound, which an unwind never releases: an unwound device is not back in at either bound, which is what the unwind disclosure tells the user. Re-admitting it is pairing it again, as below.

   **Three conditions bound what this buys.** The relay only refuses a *device-bound* request. `[auth] require_device_sig` is on by default wherever an OIDC issuer is configured, and there a request with no `X-Sunrise-Device-Sig` is refused before any lookup, so a revoked device cannot escape the check by not signing. Where the flag is off — the single-tenant self-host verifier, or an explicit `require_device_sig = false` beside an issuer — no device is resolved and no revocation check runs, so a revoked device that stops signing keeps uploading. And a device that registered before clients sent `vault_device_id` has nothing on its relay row to match; the `DELETE` answers `404`, logged as `sync.device.revoke_unknown_to_relay`, and that device goes on being accepted. And nothing bounds a device on any other transport, or on a relay this account does not use.
4. The revoked device, when next online, sees the revocation in its inbox; the UI explains the situation and the local DB is wiped on first launch.

A revoked device that never reconnects retains whatever plaintext it had at the moment of revocation. We are honest about this in the UI.

**The device that made an ordinary revocation can withdraw it** ([ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md), built by [#383](https://github.com/justin13888/Sunrise/issues/383)): `Command::WithdrawRevocation`, `sunrise device withdraw`, and `withdrawable` on each device-list row. Otherwise a device revoked by mistake is readmitted only by pairing it again as a new device. That mints a fresh `device_id`, and the new device holds no `ID_S_priv` (#221).

- **The device that made a revocation can withdraw it, and no other device can.** A withdrawal is the control op `device_revoke_withdraw`, stored as a row of its sender's `(sender, target)` pair in the ledger (migration 0053), and the fold reads each pair's **head**: the greatest stamp, a revocation winning a tie. A ledger with a withdrawn pair is the ledger in which that claim was never made, so the register stays a pure function of the op set. A later revocation from the same sender makes the pair live again.
- **The fold releases the device's read bound on a replica only under three conditions.** The register must no longer name the device, no live claim may name it, gated or not, and at least one withdrawal of it must come from an ungated sender. The release is per replica, like the bound, and logged as `core.device.read_bound_released`.
- **The withdrawing device then re-seals every epoch it holds to the reinstated device.** This includes the epochs minted while it was revoked, and the result says how many. The rotation itself is not undone.
- **A revocation that rotated the identity away from the device is not undone by a withdrawal.** The device's certificate names a retired identity, so it is not a member; the result says it must be paired again. This is the ordinary case for a revocation run on the device that created the account.
- **A revocation signed by the account identity is withdrawn only by the identity in force** (ADR-0058 §6, [#1239](https://github.com/justin13888/Sunrise/issues/1239)). On the device that holds the head's `ID_S_priv`, the command signs a `device_revoke_withdraw_by_identity`, which lands in the sender's authority pair and counts only while it verifies under the head. A device without that key cannot withdraw an authority revocation, and the identity cannot withdraw a revocation a device made. A rotation does not re-sign a withdrawn authority revocation.
- **The relay half is cancelled while it is unsent, and reversed once it was sent.** The command deletes this device's queued `relay_revocation_intents` row. Where there was none to delete, the relay may already have been told, so the command queues a `relay_reinstatement_intents` row in the same transaction (migration 0055, [#1299](https://github.com/justin13888/Sunrise/issues/1299)), unless a claim of this device's own still stands. The sync driver sends it to the relay's reinstatement route, which accepts only the signed word of the device it recorded as the revoker (ADR-0056 §5), and only while the register does not call the device revoked. `Core::relay_reinstatement_pending` reports it as pending until the relay accepts it, and as refused when the relay answers that this device is not the revoker it recorded: then the relay refuses the device, and the device must be paired again, unless the revoker it recorded withdraws too. Each session start asks the relay again, and the refusal clears once it accepts the device.

A withdrawal is emitted only behind the `core.revoke_withdraw` feature, which the command requires before its first op, and the command refuses with `FeatureUnsupportedByDevices` while any non-revoked device, or the device being reinstated, does not advertise it (ADR-0056 §7). There is no override: a device that dropped the op would keep the device revoked while every other build reinstated it.

**Step 2 also rotates the account identity, when the revoking device can.** Only the device the account was created on holds `ID_S_priv`, so a revocation run from a paired device cuts every future Stream key and leaves the identity where it is, logging `core.identity.rotation_unavailable`. That costs less than it sounds: since #105 a revoked device holds no signing key unless it is the creator, so for every other device there is no certificate for the rotation to have invalidated. Revoking the **creator** is the case that needs the rotation, and it has to be run from the creator. `Command::RotateIdentity` refuses on any other device by name, and the revocation completes without the rotation rather than failing: the device a user is revoking is often the one they lost, and the creator may be the one they lost.

A replica applying a certificate for a device id it has never seen, in an account that has revoked something, logs `core.device.admitted_after_revocation`; one issued under a retired identity logs `core.device.cert_superseded_identity`. An ordinary pairing can look like either, so both disclose and neither gates. After a revocation that could not rotate, the first is the only thing a user can see: a fresh cert from a revoked creator verifies under the identity in force, so `revoked`, `current` and the certificate all read like an honest member's. The predicate is therefore recorded as well as logged, in `devices.admitted_after_revocation`, written by `DeviceCertPublish` at apply time and surfaced as `DeviceRow::admitted_after_revocation` ([#144](https://github.com/justin13888/Sunrise/issues/144)).

**`ID_D_priv` is the read bound's one exception.** The device that created the account and any vault restored from the recovery code hold `ID_D_priv`, and so open the identity copy of every epoch; every device admitted by pairing holds none ([#76](https://github.com/justin13888/Sunrise/issues/76)). `identity.minted_by_device_id` records which device that is, and `Keychain::load` treats a copy anywhere else as inert ([#87](https://github.com/justin13888/Sunrise/issues/87)). `Command::RevokeDevice` refuses to revoke the device it runs on, so the exception is reachable only by revoking the creator from another device, and that revocation cannot rotate the identity (the paragraph above): the bound is stated, not claimed. The recovery blob is the only other place the key may live, and it is ciphertext behind the user's code rather than a device the account can revoke. Where no blob has been sealed, the vault is the only copy of the key: see [`recovery.md`](./recovery.md) §Before a blob is sealed, the vault is the only copy.

**On the device that holds `ID_S_priv`, a revocation is signed by the account identity** ([ADR-0058](../11-adr/0058-the-account-identity-is-the-membership-authority.md), [#454](https://github.com/justin13888/Sunrise/issues/454)). Step 1 then emits `device_revoke_by_identity` instead of `device_revoke`: the same claim, with `identity_id` and an Ed25519 signature by the identity in force over the claim and the envelope's sender and HLC ([`data-encryption-format.md`](./data-encryption-format.md) §Control op payloads). The user does not choose. What it changes is the fold, and only for that row:

- **It is an authority row while the identity that signed it is the head of the chain**, and the signature verifies under the head's `ID_S_pub`. The fold checks both, not the apply, because a replica can hold the op before the transition that makes its identity the head. Every other row, including an unsigned revocation from the same device, is ordinary and folds as below.
- **An authority row is never gated, and it seats a revoker no op can discount.** So no op from any device takes its target back off the revoked list, and every ordinary revocation that target writes is gated. Two expelled devices one attacker holds cannot unwind it, however many ops they write, which is the shape the "A revoked device cannot revoke another device" bullet below leaves open for ordinary rows.
- **A rotation carries it forward.** A row signed under a retired identity is ordinary from the first fold that sees the successor, so `rotate_identity` signs, under the successor and in the transaction that emits the transition, a fresh authority revocation of each device the pre-rotation fold held one for and the new roster does not re-certify. Revoking from the holder rotates the identity right after (the paragraph above), so this runs on every authority revocation.
- **It is emitted only behind the structural feature `core.revoke_authority`.** A build that cannot read the op parks it and would fold a different register. The holder signs while the vault requires the feature, or while every device that is not revoked, leaving out the one being revoked, advertises it; it emits `vault_requires` first in that case. Otherwise it emits an ordinary `device_revoke`, and `CommandResult::authority_awaits_update` names the devices to update. It never refuses. A paired device never signs.

The device list says which kind revoked a device: `DeviceRow::revoked_by_authority`. An account whose holder is unreachable and that has no recovery code has no authority, and every revocation in it stays ordinary. A stolen holder gains nothing from this: it can already rotate the identity with every honest device left out. The remedy is a vault restored from the recovery code, which revokes the stolen device by authority and rotates it out (ADR-0058 §5).

**The rotation reaches the account's key bundle, and so does every other new key** ([ADR-0069](../11-adr/0069-the-accounts-keys-travel-as-an-identity-sealed-bundle.md)). The identity copy of each new epoch is a `key_envelope` op, and the relay trims ops. So the device that holds the new keys first, normally the one that rotated, republishes the account's key bundle before its session reports `Live`: every key it may write under, sealed to the identity in force's `ID_D_pub`, signed by its `D_S`, and pinned on the relay outside op retention. It publishes only a superset of the bundle the relay pins and only over the pin it read, so a device that has not yet absorbed the rotation never puts the old keys back. A key the revoked device delivered is not carried, and a bundle the revoked device pinned is replaced by the next honest device to sync. Sealing to the identity reaches exactly the devices the identity copy of every envelope already reaches; after a revocation from the creator the identity has rotated and the cut device holds no share of it.

**The cut is the HLC of the `device_revoke` op itself.** There is no `effective_at` field for an emitter to choose or for anyone to bound. Concurrent revocations of one device resolve as an LWW register on that op's own `(hlc, device_id)`, the rule [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) already resolves every other concurrent write in this engine with, so every replica reaches the same cut whatever order it saw them in, a cut that landed wrong is correctable by revoking again, and a device cannot move its own cut. What no rotation can do, however complete, is take back what the device already had: revocation is forward-only.

**What converges, and what does not.** Every replica applies every **entity** op it can decrypt — a task edit, a note, a focus session — whatever its sender's revocation state and whatever order the `device_revoke` and the op arrive in. There is no refusal record for those anywhere in the tree, so delivery order is not an input to the materialized state: two replicas holding the same op set hold the same task table. What the apply path does consult is the read bound, and only to decide key distribution: applying a `DeviceCertPublish` runs `backfill_key_envelopes`, which returns early on a read-bounded device — it tested `Engine::is_revoked` until migration 0028 gave the bound its own table — so what is settled there is which device is sealed key material, never whether the op applies. Two consequences follow and are worth stating outright, because they are what makes the rest of this section safe.

* **A cut correction is lossless for user work.** Nothing a user typed was refused under the old cut, so a revocation re-issued from a healthy device changes what is sealed *next* and destroys nothing already applied. That is what lets the register move in both directions — the LWW rule above — instead of being an irreversible `MIN`.
* **The write bound for entity ops belongs at the relay.** Refusing one at apply time cannot be made convergent without a projection rebuild this engine does not have. Step 3 above is that bound. Note what it does *not* reach: the three conditions named in step 3.
* **`device_revoke` is the one family where revocation reaches backwards.** Every other op a revoked device ever wrote still stands, including a cert it issued under an identity since retired. A `device_revoke` is the one the fold can discard, and no date saves a row from it: the date is a number its sender chose, and nothing in the ledger separates an honestly-earlier revocation from a back-dated one, so the fold reads no clock for this at all. *Which* rows it discards is the "**A revoked device cannot revoke another device**" bullet below, with the gate's one exception and the discount pass's two residuals; this bullet names the family and no more. The visible form of that is `core.device.revocation_unwound` — a device coming back off the revoked list because the replica learned who revoked it — and the remedy, as everywhere else in this section, is to make the revocation again from a device the account still trusts. **What the unwind does not give back is the keys.** The read bound is a second, monotone table (`device_read_bounds`, migration 0028) that the four key-distribution sites read — and a fifth that distributes no key, the third-party recipient claim in the bullet below — and that nothing ever deletes from, so an unwound device shows as current while receiving nothing. That asymmetry is deliberate — a bound a later op could release would not be a bound — and it is disclosed rather than left to be inferred: `DeviceRow::read_bounded` puts it on every device list, and `CommandResult::revocation_unwound` names the devices in that state on the next revocation. It is tolerable only because the effect can be re-derived, which is the same reason the gate stops at control ops.

**Two control ops are bounded at the peer** ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md), [#82](https://github.com/justin13888/Sunrise/issues/82)), because their effect is a register the engine can re-derive rather than a row it could never un-write.

* **A revoked device cannot revoke another device — with the one exception and the two residuals stated below, which is the whole of what this bullet promises.** It could, until ADR-0041: its cert is still on the chain, it still holds the vault-meta key for the epoch it was cut at, and revocation has no inverse — so a laptop the account had expelled could expel the account, permanently, on every replica. `device_revocations` is now a **fold** over `device_revoke_ops` (migration 0027). The ledger keeps every such op except where two ops name the same `(sender, target)` pair: then it keeps only the one with the higher stamp, because the fold reads nothing from the other ([#250](https://github.com/justin13888/Sunrise/issues/250)). It also keeps at most 256 distinct targets per sender, the ones with the newest stamps, and drops the rest of that sender's pairs; that bounds a sender naming ids nobody has, and it drops only the flooding sender's own claims ([#315](https://github.com/justin13888/Sunrise/issues/315)). A row whose sender the ledger revokes **anywhere in it** is skipped — unless the only party to have revoked that sender is the device the row is about, which is what keeps two devices revoking each other converging on *both* revocations rather than letting a back-dated op silence its target. Anywhere in it, rather than in the part of the ledger sorting below the row, and that is the security content of the rule: the sort key is the op's HLC, an HLC is whatever its sender wrote, and a sender that could clear the gate by dating an op a millisecond below its own cut would have no gate at all. The fold reads no clock for this — not this device's and not the op's — it asks who has revoked whom.

  The exception has a cost a user can reach, so it is stated rather than deduced: **after two devices have revoked each other, neither can revoke anybody else**, because each one's only revoker is the other, so each is forgiven for revoking the other and gated for revoking a third party. That reaches the honest device of the pair. It lasts until a third current device revokes the half it believes compromised. That revocation gates the compromised half's revocation of the honest one and discounts it out of the honest one's revoker set, so the honest device is current and ungated again ([ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md) §3). Its read bound stays on any replica that had taken it. The loss is narrow, and narrow is a bound rather than a hope: a revoked device enters the revoker set of a device V only when V is the **only** device that revoked it, so a device it merely named is untouched and so is a second device that also revoked it. Revocation is not gated on `ID_S_priv` anywhere either, so identity rotation and pairing sponsorship are untouched and **any current device outside the pair still revokes whoever it likes**. The lockout is total only in a two-device account, where there is no third device to ask. What that bound gives up is that a device whose sole revoker is itself revoked by a third party stops being gated. **That third party need not be a bystander**, which is the reading a threat model has to take: one attacker holding two devices the same revoker expelled reaches it with a single op. O revoked X1 and X2; X1 revokes O; the mutual exception lands that op, and O going out both unwinds O's revocation of X2 and discounts O out of X2's revoker set, so X2 revokes the rest of the account. The remedy is the mutual pair's and no better — a device X2 reaches revokes it back and is left revoked itself — so what the account needs is a current device the attacker never reached, and against a second expelled device even that lasts one op. What settles it is an authority revocation from the holder of `ID_S_priv` (the paragraph on ADR-0058 above), which neither expelled device can discount. Extend the same chain by one link and the second shape appears: a device on the revoked list while being ungated. Neither is reachable out of the revoked device's own rows, since a device authors only rows whose sender is itself. See §"What a user sees" item 4 in [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md) for both, with the tests that pin them.
* **A read-bounded device cannot claim a third device has already been sent a key** — which is every revoked device and, after an unwind, more. The `key_envelope_recipients` row the `Recipient::Device(other)` arm files is what `backfill_key_envelopes` reads to decide a device is served, and filing one for somebody else is a way to withhold a key from them. A **read-bounded** sender's claim is no longer recorded: the gate asks `device_read_bounds` and not the register, because the class worth refusing is the one whose *reads are bounded*, and an unwound device (`read_bounded: true, revoked: false`) is in it while the register has stopped naming it. The table is a hint rather than state, so declining a row can only cause *more* key distribution and never less — which is why this one gate can afford a predicate that does not converge across replicas, where the fold's could not.

Both skips are **stored, not dropped**, and both are visible: `core.device.revoke_refused` with `reason = "revoked_sender"`, and `core.device.revocation_unwound` when a re-fold stops believing a revocation somebody has already seen. A cut correction does **not** un-skip a revocation — the gate asks who has revoked whom and reads no cut, so a correction changes which row wins the register and leaves the gate's answer exactly as it was, whichever direction it moves the cut — and the remedy is to make the revocation again from a device the account still trusts. That is tolerable for an administrative act and would not be for a task edit, which is exactly why the gate stops where it does.

**The read bound has a write side in key distribution, and it is bounded too** ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md) §Decision 4, [#280](https://github.com/justin13888/Sunrise/issues/280)). Withholding keys from a revoked device is half of it. The other half is not writing under a key the revoked device chose. A modified client can mint an epoch above the live one and seal it to an honest device's public `D_D`. While the live epoch was `MAX(epoch)`, that device's next write on the stream was readable by the revoked one. The key is not refused: a replica that declined it could not open ops an honest peer sealed under it before learning of the revocation, and what a replica can read would then turn on delivery order. It is absorbed, and `stream_key_senders` (migration 0029) records who delivered it. `Keychain::current_epoch_tx` then passes over any key a device in `device_read_bounds` delivered, checked when a key is chosen and not when it arrived. On an ordinary stream the live epoch is the highest epoch held, only if it holds such a writable key, and never an older epoch below it, which an earlier-revoked device may hold. Minting counts from the highest epoch held, so a rotation lands above such a key. A stream whose highest epoch the revoked device delivered, by its own rotation or by backfill, holds no live key: an ordinary stream then mints. The vault-meta stream falls back to its highest writable epoch, else its highest held one, because the revocation itself must be sealed where the cut device can read it. Pre-revocation keys minted here or delivered by current devices stay live, and the revoked device holds them until the rotation's envelope arrives. The scope is the read bound's own, per replica: a replica that has not bounded the device still adopts its key, and the local guards on `revoke_device` and `Command::RotateStreamKey` are what stop an unmodified build emitting one. Two further gaps are stated in the ADR. A key a current device re-seals to a newly certified one carries no record of where it came from. And on the vault-meta stream there is a window between applying the revocation and applying the rotation that follows it.

So the guarantee a user's device may promise is one clause, and the scope is part of the promise: **on a replica that applied the revocation while its sender was still ungated there, the device reads nothing written afterwards.** Not that it stops writing ordinary content, and not that what it wrote before the revocation reached everyone is rolled back — and **not that the account promises it, only this replica.** That last is the scope and not a hedge: the bound rests on `device_read_bounds`, which is a ratchet over what *this* replica's own folds produced and does **not** converge, so a sibling replica that met the same two ops in the other order may still be sealing that device keys. `Engine::is_read_bounded` says the same thing at the function: a caller may read it as "has *this* replica bounded the device" and may not read it as "has the account". [#411](https://github.com/justin13888/Sunrise/issues/411) carries the counterexample. A revocation the identity holder signs already converges (ADR-0058), and closing [#1167](https://github.com/justin13888/Sunrise/issues/1167), the unsigned remainder, is what would let this sentence quantify over the account. A device that pairs starts with its sponsor's bound ([#282](https://github.com/justin13888/Sunrise/issues/282)), so the sibling that falls short is one that already existed, never a device freshly paired from a sponsor that had bounded the device.

**There is no second clause about what a revoked device may still revoke or claim, and the bullets above are why.** An earlier form of this sentence promised that on every replica that applied the revocation the device could neither revoke another device nor claim one had been keyed, on the ground that `device_revocations` is a fold over the whole ledger and therefore converges. The convergence holds — two replicas holding one op set agree on the register — but convergence is not the promise. The fold's one exception seats a row whose sender's only revoker is the device the row is about, and the discount pass reaches two shapes further: a rehabilitated device, and a device on the revoked list while being ungated, which is the counterexample stated in the `device_revoke` bullet above. Every replica agrees on that device, and it revokes third parties. So no sentence of the form "it can no longer revoke another device" holds for every ledger, and this document states the residual instead of a universal: §"What a user sees" item 4 in [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md) carries it in full with the tests that pin it, and what may be told to a user is held open by [#252](https://github.com/justin13888/Sunrise/issues/252); the mutual pair's lockout ([#248](https://github.com/justin13888/Sunrise/issues/248)) is settled by a third current device, per [ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md) §3. The recipient-claim half is narrower than it was written too: a **read-bounded** sender's claim is not recorded on a replica that bounded it, which is the read clause's per-replica scope again and not a stronger one. The decisions, their rejected alternatives and what would reopen them are [ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md) and [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md).

## What the format freeze covers

Every value in this document is a **format**, not an implementation detail. A domain-separation string, an AAD prefix, a KDF context and a wrapped-blob length are all the same kind of thing: they are never transmitted and never stored beside what they protect, so a build that spells one differently seals and opens its own data perfectly and nobody else's. Nothing on the wire, on disk or in a log would say so. The user finds out when a second device, a restored recovery code, or the next release cannot read the vault.

That class of change is a **crypto-suite version bump** — a `CRYPTO_SUITE_V` increment landing with a rotation plan — and never a test fix.

### How a constant is anchored

`crates/sunrise-crypto-test-vectors` has **no dependencies at all**. Every value in it is a literal, produced once by running the implementation and then written down. A vector can therefore only agree with the implementation if the implementation still produces the same bytes, and a coordinated edit to a constant *and* to the test beside it cannot pass, because the expectation lives in a different crate and does not move.

Two things are deliberately not done:

- **No vector is computed.** An assertion that called the code under test to produce the value it then checks would prove nothing. That is the exact failure the audit found in the "legacy vault" tests, which wrapped their secrets in-process with the same AAD they then unwrapped with.
- **No vector is asserted only by a round trip.** A seal-then-open test is self-consistent by construction for every constant that is not transmitted, which is all of them.

### What is pinned

| Constant | Anchored by |
|---|---|
| `sunrise.identity_id.v1` | `IDENTITY_ID_VECTORS`, and the committed keychain vault |
| `sunrise.device_id.v1` | `DEVICE_ID_VECTORS`, asserted **twice** — the derivation is spelled out in `sunrise-crypto` and again in `sunrise-core`, and they are separate constants |
| `sunrise.device_cert.v1` | a whole frozen cert: body bytes, signature, outer encoding, and a verification of the frozen bytes under the frozen identity |
| `sunrise.identity_transition.v1`, `…succ.v1`, `sunrise.identity_roster.v1`, `sunrise.identity_shares.v1` | one canonical transition — frozen body CBOR, body hash and signature pair — plus the two digests as their own literals |
| `sunrise.identity_share.v1`, `sunrise.identity_carry.v1` | the two HPKE `info` strings, byte for byte |
| `sunrise.hpke.key_envelope.v1` | `key_envelope::INFO` and a frozen seal |
| `sunrise.hpke.share_grant.v1`, `sunrise.share_grant.v1` | `share_grant::INFO` and a whole frozen viewer grant, which also pins the AAD binding the granter and the payload's CBOR field numbering |
| `sunrise.stream_root.init.v1`, `sunrise.stream_root.step.v1` | the frozen merkle chain |
| `sunrise.op_envelope.v1` | the two whole-envelope vectors |
| `sunrise.blob_chunk_nonce.v2` | the nonce vectors and a frozen sealed chunk |
| `sunrise.stream_key_id.v1` | `KDF_VECTORS` and the truncation assertion |
| `sunrise.wrap.stream_key.v1` | a frozen wrapped key that opens **only** at its own `(stream_id, epoch)`, and the committed keychain vault |
| `sunrise.recovery_blob.v1` | a whole frozen blob, which also pins the magic prefix, the Argon2id parameters and the payload's CBOR field numbering |
| `sunrise.local_identity.v1`, `…dh.v1`, `…identity.sign.v2`, `…identity.dh.v2` | the four AADs as literals, and the committed keychain vault |
| `sunrise.meta_genesis_key.v1` | a frozen key, and the committed keychain vault |
| `sunrise.sqlcipher_key.v1` | the committed keychain vault, and `sunrise-storage`'s committed old-vault fixtures |
| `sunrise.stream_key.v1` (the pre-ADR-0024 derivation) | two frozen legacy keys, through `legacy_derived_stream_key` itself |
| `sunrise.remote_op_id.v1` | three frozen op-log ids |
| `sunrise.pair_sas.v1` | three frozen six-digit codes |
| `sunrise.account_email_hash.v1` | three frozen account tags |
| `sunrise-device-sig-v2` | the canonical string spelled out, and two frozen signatures that also verify |
| `sunrise.import_block.v1` | three frozen Block ids |
| `sunrise.routine_task.v1` | two frozen Task ids |
| `sunrise.relay.batch.v1` | one frozen batch-dedup hash |
| `sunrise.inbox` | the sentinel asserted as ASCII in `sunrise-domain` |
| `sunrise.invalid` | the exported `UID` host, asserted in `sunrise-domain`'s round-trip test |
| `WRAPPED_SECRET_LEN`, `WRAPPED_STREAM_KEY_LEN` | literals in the vectors crate, and the blob lengths in the committed vault |

### The vault that has to open

`crates/sunrise-core/fixtures/keychain_vault_suite_v5.db` is an encrypted `SQLCipher` vault sealed by this build and committed. `crates/sunrise-core/src/keychain/fixture.rs` copies it, opens it through the ordinary `Db::open` and `Keychain::open`, and asserts that every key that comes out is a literal written down elsewhere. Ten of the constants above are needed to get that far and none of them is in the file.

It exists because the wrapping side of the hierarchy had no test that was not self-consistent: both "legacy vault" tests synthesise their vault in-process with the same `wrap_secret` and AAD builders they read it back with, and `sunrise-storage`'s fixtures go through a real `Db::open` but assert *rows* — their wrapped blobs are invented byte strings no keychain ever unwraps.

Regenerate with `mise run keychain-fixture`, and only for a `CRYPTO_SUITE_V` bump — at which point the file is renamed to the new suite version rather than replaced. `crates/sunrise-core/fixtures/README.md` has the full rule. Regenerating it to turn a red test green is the one thing it exists to catch.

### Residue: what is still not pinned

Named rather than left to be discovered.

- **The two `identity_transition` HPKE shares are frozen only as `info` strings, not as sealed blobs.** HPKE draws an ephemeral KEM key per seal, so a byte-exact vector needs a stated CSPRNG state. `key_envelope` has one; these two do not. The `info` literals are what a second implementation has to match, and they are the part that is not transmitted — but the blob layout `enc || ct || tag` is pinned only by `DEVICE_SHARE_LEN` and `IDENTITY_SHARE_LEN`, which are constants rather than literals.
- **No committed *legacy* vault.** `sunrise.stream_key.v1` is anchored as a derivation, but the pre-ADR-0024 adoption path is still exercised only against a vault the test synthesises in-process. A fixture for it would have to be written by a build that no longer exists; the alternative — hand-assembling one — reintroduces the self-consistency the fixture is for.
- **Most length constants are not literals.** `WRAPPED_SECRET_LEN` and `WRAPPED_STREAM_KEY_LEN` are, because nothing else wrote them down. `AEAD_NONCE_LEN`, `HPKE_ENC_LEN`, `HPKE_TAG_LEN`, `STREAM_KEY_ID_LEN`, `DEVICE_SHARE_LEN`, `IDENTITY_SHARE_LEN`, `RECOVERY_SALT_LEN`, `CHUNK_PLAINTEXT_LEN` and `SEALED_CHUNK_LEN` are not — each is either implied by a frozen blob's length or is a chunking-geometry choice with no stored witness. `CHUNK_PLAINTEXT_LEN` is the one worth naming on its own: it decides how a blob is split, two builds that disagree produce different chunk counts for one file, and no frozen vector covers a multi-chunk blob.
- **Nothing pins these constants across languages.** The Apple clients call into the core through FFI and re-implement none of this, so there is no second implementation to disagree — which is also why there is no cross-language vector suite. If one is ever written, this table is the list it starts from.
- **The suite version itself is not in most frozen bytes.** Only the two whole-envelope vectors carry a version field (field 12, the document schema). Every other vector here would survive a `CRYPTO_SUITE_V` bump that changed nothing, and would fail one that changed anything — which is the intended asymmetry, not a gap.

## What rotation does not do

- It does not retroactively un-leak content. A revoked device keeps whatever plaintext it had on disk.
- It does not erase the *fact* of past activity from server-side metadata (timestamps, op counts).
- It does not affect historical ops the rotating side already emitted under prior keys; the op log is by design replayable to converge state.
