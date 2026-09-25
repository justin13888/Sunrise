---
status: accepted
---

# Key Rotation

Three key types rotate, each with a different cost and cascade. Throughout this spec, "the rotating device" is the device the user initiated rotation from; it MUST be a paired, currently-authorized device.

## Implementation status: Stream-key rotation, revocation and identity rotation are built

[ADR-0024](../11-adr/0024-key-hierarchy.md) landed the hierarchy these procedures assume: Stream keys are 32 random bytes per `(stream_id, epoch)`, wrapped under the vault root in `stream_keys`, and distributed by HPKE `key_envelope` ops.

**Built:**

* **§Stream key rotation**, in full. `Keychain::mint_epoch` draws a fresh key, `Engine::emit_key_envelopes` seals it to every **unrevoked** sibling device's `D_D_pub` and to the identity's `ID_D_pub`, and past epochs are retained: `stream_keys` is keyed `(stream_id, epoch, key_id)` and a decrypt tries every key at `(stream_id, epoch)`, so two devices minting one epoch concurrently both keep theirs. `Command::RotateStreamKey` is the narrow entry point.
* **§Revocation bounds a revoked device's reads, and its *entity* writes only at the relay — no replica refuses one of those, which is what keeps the *effect* convergent ([ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md)). Two of its **control** writes are refused at the peer, because their effect is a register the engine re-derives ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md)) — with one exception and two residuals that §Revocation states, which is why nothing in this document promises anything about what a revoked device may still revoke.** `Command::RevokeDevice` emits a `device_revoke` op, records the register, rotates every stream in the rotation set — the vault-meta stream and the Inbox included — and queues the relay `DELETE` as a durable intent the sync driver drains. An earlier slice's direct call was reverted because that route named an id the client cannot know; step 3 below has the one that does not.

  * *Reads* are bounded in the vault — **on a replica that applied the revocation while its sender was still ungated there** — and it takes two changes that are each vacuous alone. **The qualification is load-bearing and is not the propagation one.** Propagation is the obvious half: the register is per-replica and propagates like any other op, so a device that has not yet seen the `device_revoke` will seal a new epoch to the revoked device, and a replica that has been offline while Streams were created on it delivers those epochs when it reconnects. That half closes as ops arrive. The other half does not close at all: the bound is a ratchet over the registers *this* replica computed along its own arrival order, so a replica that learned of the *sender's* own revocation first gates that row, never writes the bound, and never will — having applied the revocation is not enough. Retire laptop C from desktop A, then months later retire A from phone B, and a replica that met `B -> A` first goes on sealing keys to C forever while a replica that met `A -> C` first does not. **The guarantee is per-replica, not per-account**, and a device that pairs today inherits no revocation state of any kind and so is the weakest replica in the account. See [#282](https://github.com/justin13888/Sunrise/issues/282), which carries the counterexample and the two design questions it turns on. Revocation is eventually consistent for the propagation half, and the "cut" is the point after which *informed* replicas withhold. `emit_key_envelopes` anti-joins `device_read_bounds`, so a revoked device is sealed no envelope for any epoch minted at or after its cut; and `PairingPayload` no longer carries `ID_D_priv`, so there is no identity-sealed copy for it to open instead. While pairing handed every device the identity's unwrapping key, excluding a device from the recipient list withheld nothing — which is why an earlier slice removed the exclusion and filed [#76](https://github.com/justin13888/Sunrise/issues/76) rather than shipping half of it. Sealing needs only the public half, so the identity copy is still emitted for every epoch and recovery still reaches all of them.

    Removing that fallback opens a gap and `Engine::backfill_key_envelopes` closes it: a device certified *after* an epoch was minted was left out of that epoch permanently, and minting happens whenever a Stream is created. A replica applying a `device_cert` now seals **every** epoch it holds to the newly certified device, and `key_envelope_recipients` (migration 0018) records who has been sent what so a backfill emits only what is missing and a re-published cert emits nothing. Every epoch and not the live one per stream: current-epoch-only was the first shape and it was wrong wherever a rotation landed between a device's pairing and its certificate, which leaves that device holding the payload's epochs and the live one with a silent permanent hole in between — ops sealed under the missing epochs are deferred in `deferred_ops` and expire at the TTL, presenting as "some items from around when I set up this device never arrived". That was [#107](https://github.com/justin13888/Sunrise/issues/107). A **read-bounded** device is skipped, or revocation would be undone by re-sending a cert. The test is `device_read_bounds` and not the register, and at this site the difference is the largest in the engine: the register is a fold and can stop calling a device revoked, at which point this early return stopped firing and one republished cert handed that device *every held epoch of every stream* rather than only the ones minted since. Migration 0028 gives the bound its own table so it cannot be released.

    **One exception, and it is real.** A device paired while `STORAGE_V` was 17 wrapped the `ID_D_priv` its payload carried into its own `identity` row, and migration 0018 could not clear it: on the account's creator that column holds the only copy of the key and the schema recorded nothing that distinguished the two. It recorded nothing under that name. `stream_keys.source` did — only a founding vault mints the vault-meta stream's first epoch, so a `local` or `legacy` row at epoch 1 is proof of having minted the account identity and its absence is proof of the opposite. Migration 0019 reads that, records the answer in `identity.minted_by_device_id` so nothing has to infer it again, and clears the column everywhere else; `Keychain::load` applies the same test at open, so a copy that reaches the column by any other route is inert. That was [#87](https://github.com/justin13888/Sunrise/issues/87).

    The exception that remains: the device that *created* the account, and any device restored from the recovery code, hold `ID_D_priv` and can open the identity copy of any epoch. `Command::RevokeDevice` refuses to revoke the device it runs on, so this is reachable only by revoking the account's creator from another device. The recovery blob is the only other place that key is allowed to live, and it is ciphertext behind the user's code rather than a device the account can revoke — so the bound is stated rather than claimed.

    The same fact has a second consequence, in the other direction, and it is the one a user feels: **where no recovery blob has been sealed, that vault is the only place `ID_D_priv` exists, and losing it destroys the key permanently.** No recovery feature added later can retrieve it, because sealing a blob needs the key it would carry. `sunrise bootstrap` seals one at account creation and, since #181, so do the Apple clients: `SunriseCore::bootstrap_account` is the same ceremony behind one FFI call, and `RecoveryCodeModel` runs it the moment a vault is created. See [`recovery.md`](./recovery.md) §Implementation status. `Keychain::holds_only_copy_of_identity_key` answers it in the core API, `Core::holds_identity_key` passes it through, and the seam exports it — and since [#144](https://github.com/justin13888/Sunrise/issues/144) every client states it as well as clearing it. `sunrise devices` prints the consequence under the listing and the Apple device list puts it in the Devices section of Settings, on the devices it is true of and nowhere else.

  * *Writes are bounded **at the relay, conditionally**.* The relay cannot learn the revocation from the op stream and must not be able to — `device_revoke` is sealed under the vault-meta Stream key, and promoting the revoked id into the cleartext envelope header would tell the relay which of an account's devices had been revoked and when, for every account it serves — so it is told out of band. `Command::RevokeDevice` queues a `relay_revocation_intents` row in the same transaction as the op and the sync driver drains it to `DELETE /api/v1/devices/by-vault-id/{id}`, retrying on every session until the relay answers. That route takes the vault-side device id, because the relay's own `device_id` is a ULID it mints at registration and never sends back through the op stream — which is why the older route could not express a revocation at all, and why [#80](https://github.com/justin13888/Sunrise/issues/80) was a relay API change before it was a client one. §Revocation step 3 has the mechanism.

    **Three conditions.** The relay only enforces against a *device-bound* request, so with `[auth] require_device_sig` at its default `false` a revoked device that stops signing keeps uploading. A device that registered before clients sent `vault_device_id` has nothing to match and is still accepted, logged as `sync.device.revoke_unknown_to_relay`. And nothing bounds a device on any other transport, or on a relay this account does not use. Peers still apply a revoked device's **entity** ops whatever the relay did; two of its **control** ops they do not, which is the bullet below.

  * *Peers do not refuse a revoked device's **entity** ops*, and that is a decision rather than a gap. Refusing one at apply time is not convergent — a replica that applied an op before the revocation reached it cannot un-apply it, and this engine has no projection rebuild — so two replicas with the same op set would disagree permanently. It also freezes the refusing replica's sync cursor for that device, which within retention latches an unclearable data-loss warning on every peer. That is [ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md), and it is still the decision for a task edit, a note or a focus session.

    *Two of its **control** ops they do refuse*, because the effect of those is a register the engine re-derives rather than a row it could never un-write: `device_revoke`, and a read-bounded sender's claim that a third device has already been sent a key. That is the scope, and the scope is the whole of what this sentence states. Neither is refused where the op lands — the ledger keeps every row the fold reads and the register is a fold over it — so nothing turns on a cursor and no replica's delivery order decides who is revoked. That is [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md), which closed [#82](https://github.com/justin13888/Sunrise/issues/82); §Revocation below has both gates in full. Converging the *effect* rather than the record was [#78](https://github.com/justin13888/Sunrise/issues/78), closed by ADR-0034 without code.

  What the register provides is agreement. The cut is the **HLC of the `device_revoke` op itself**; there is no `effective_at` field for an emitter to choose or for anyone to bound. Concurrent revocations resolve as an LWW register on that op's own `(hlc, device_id)` — the rule [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) already resolves every other concurrent write in this engine with — so every replica reaches the same cut whatever order it saw them in, a cut that landed wrong is correctable by revoking again, and a device cannot move its own cut.

  What no rotation can do, however complete: take back what the device already had. Revocation is forward-only.

  * *The rotation can also be **incomplete**, and says so when it is.* `Keychain::rotation_set` builds the set from the `stream_id` columns of `stream_keys`, `streams` and `ops`, and a column that is not 16 bytes names no stream to mint an epoch for. Such a row is not padded out — that would file a key against the vault-meta stream — and it is no longer dropped in silence either, because the revoked device goes on holding whatever key it was last given for whatever the row refers to. `Command::RevokeDevice` rotates what it can and returns the rest on `CommandResult::unrotated_streams`; it also emits `core.device.revoke_incomplete`. A client must disclose a non-empty list rather than print "revoked", which is the same rule the relay half already follows ([#160](https://github.com/justin13888/Sunrise/issues/160)). Failing the whole revocation instead would be worse: the device that is gone is the entire scenario.

* **§Identity rotation**, in full, and it is what makes revocation stick
  ([ADR-0037](../11-adr/0037-identity-transition.md)). The account identity is
  an append-only **chain** folded from `identity.genesis_identity_id`;
  `InnerOp::IdentityTransition` carries a whole hand-over — the successor's
  public halves, a re-issued `DeviceCert` for every surviving device, and a
  per-device share sealed by HPKE to each of their `D_D_pub`.
  `Command::RotateIdentity` is the explicit entry point and
  `Command::RevokeDevice` does the same with the revoked device left out.

  **The share carries the successor's `ID_S_pub`, not its `ID_S_priv`.** It used
  to carry the secret, and that undid #105's fix at the first revocation after
  any pairing: the device pairing had deliberately withheld a signing key from
  would be handed one by the next rotation. A rotation moves `ID_S_priv` to
  nobody. The one exception is the share addressed to the *emitting* device,
  which minted the successor and already holds the outgoing key — without it a
  device would lose the ability to rotate again on the rotation it just
  performed. The share's job is to say who is in the roster, and
  `shares_digest` binds the set into the signed body; it is not a way to
  distribute a capability.

  The rule is: **a `DeviceCert` is admitted if it verifies under any identity on
  the chain, and confers membership only while the device's row names the
  chain's head.** Applying stays unconditional, which is what keeps it
  convergent (ADR-0034); standing is derived at every point of use from two
  stored values, so two replicas holding the same op set always agree. Two
  guards enforce it and they do different jobs:
  `Engine::backfill_key_envelopes` refuses the initial hand-back, and
  `emit_key_envelopes`' `identity_id` clause bounds every *subsequent* epoch —
  which is the failure ADR-0032's alternative 3 could not close.

  This was the **remedy** for
  [#105](https://github.com/justin13888/Sunrise/issues/105): a revoked device
  could still mint a device id and sign a valid cert for it with the `ID_S_priv`
  it kept, but the cert was genuine under an identity the account had retired,
  so the row was admitted, the device was current under nothing, and it was a
  recipient of nothing.
  `engine::tests::a_revoked_device_cannot_rejoin_under_a_fresh_device_id` is the
  test that used to assert the bypass, with its setup unchanged.

  It bought one revocation at a time, which is why it was not the end of it: the
  device paired *after* a rotation held the new `ID_S_priv`, so revoking that
  one replayed the whole trick one identity along.

* **`ID_S_priv` does not travel, and that is the fix.** `PairingPayload` carried
  it, and a `DeviceCert` names only its subject and carries one signature — the
  identity's — so every paired device could mint a valid cert for any device id
  it invented. It does not travel now: the sponsoring device issues the joining
  device's cert, which is what makes pairing a three-message exchange
  ([`pairing-and-onboarding.md`](./pairing-and-onboarding.md)). The sponsor
  cannot sign a cert for keys the joiner has not minted yet, so there is no
  one-shot form of it.

  So **#105 is closed outright rather than bounded, for a device admitted by
  pairing.** Such a device cannot produce a certificate at all — before its
  revocation or after it, for the second device removed or the tenth — and
  rotation is no longer what stops it, which is exactly why it keeps working.
  The **creator** is outside that scope and is not closed by it: the creator is
  the one device that holds `ID_S_priv`, so a revocation run from anywhere else
  leaves it holding the signing key (§Revocation step 2, and its
  `core.identity.rotation_unavailable`). That residual is what ADR-0034
  §Consequences names as the unmitigated bypass, and it is the creator's alone.
  `keychain::tests::a_paired_device_cannot_issue_a_cert_for_a_fresh_device_id`
  asserts the absence three ways that fail independently: behaviourally through
  `issue_cert_for`, at rest through an empty `identity.id_s_priv_wrapped`, and
  across a restart.

  This is **not** [ADR-0032](../11-adr/0032-revocation-cannot-bound-cert-issuance.md)'s
  rejected alternative 2. That one put a *sponsor countersignature* on the
  certificate and refused a cert whose sponsor was revoked, which permanently
  locks out every device a revoked one ever paired — including, down a chain of
  pairings, devices the user never associated with it. Sponsor-*issued* certs
  carry no sponsor binding at all: byte for byte the same `DeviceCert` shape,
  one signature, the identity's, no issuer field. A verifier cannot tell which
  device held `ID_S_priv` when the cert was signed and does not need to, so
  revoking a sponsor locks nobody out. The sponsor is a gate at issue time and
  leaves no trace on the artifact.

* **§Slow peers and out-of-order epochs**, as written. There is no per-epoch barrier: an op whose key has not arrived is deferred in `deferred_ops` and retried after every absorbed key, so arrival order across epochs changes nothing.

**Not built, and named here rather than discovered later:**

* **Rotating the identity from a device that did not create the account.** Signing a transition needs the outgoing `ID_S_priv`, and since #105 only the account's creator holds it. `Command::RotateIdentity` refuses elsewhere by name, and `Command::RevokeDevice` completes without rotating and logs `core.identity.rotation_unavailable`. Refusing the whole revocation would be worse: the device a user is revoking is often the one they lost, and the creator may be the one they lost. What is given up is bounded — a revoked device that was itself paired holds no signing key either way, so there is nothing for the rotation to have retired. The case that genuinely needs it is revoking the creator, and that has to be done from the creator.

  A replica applying a certificate for a device id it has never seen, in an account that has revoked something, logs `core.device.admitted_after_revocation`; one issued under a retired identity logs `core.device.cert_superseded_identity`. Both are what an ordinary pairing can look like too, so both disclose and neither gates. **This is the case where the first of those is the only thing a user can see.** A revocation that rotated leaves the fresh cert under a retired link, so `Query::DeviceList` reports `current: false` and the CLI marks it "not active on this account"; a revocation that could not rotate leaves it under the identity in force, so `revoked`, `current` and the certificate all read exactly like an honest member's. Since #144 the readmission predicate is therefore recorded as well as logged — `devices.admitted_after_revocation`, written by `DeviceCertPublish` at apply time and surfaced on `DeviceRow::admitted_after_revocation` — because it is the only column left that distinguishes the two.
* **§Device key rotation.** No `device_rotate`; a device's `D_S` / `D_D` are minted once at open and never replaced. The one exception is a **recovery**, which is a fresh vault and therefore mints fresh device keys and a certificate signed by the restored `ID_S_priv` as a matter of course — `sunrise recover`, and [`recovery.md`](./recovery.md) §Recovery flow step 6. That is not device-key rotation: the old device is not superseded, it is gone.

  **The rule is enforced, not only observed.** `device_id` is derived from `D_S_pub` alone, so the id does not pin `D_D_pub`, and before [#281](https://github.com/justin13888/Sunrise/issues/281) a second cert for a known id under a fresh `D_D_pub` rebound `devices.d_d_pub` through the `DeviceCertPublish` upsert, and the backfill that follows it sealed every held epoch to the new key; a roster entry in an `identity_transition` did the same through `apply_roster`. Both are now set-once: a cert that would move a stored `d_d_pub` is refused whole (`core.device.cert_rejected`, `reason = "rebinds_key"`), a roster entry that would is skipped (`core.identity.roster_entry_rejected`), and both upserts write the column through `COALESCE(devices.d_d_pub, excluded.d_d_pub)` so a NULL left by a legacy adoption is still filled. A device that needs a new `D_D` needs a new `D_S` too, which is a new `device_id` — the recovery path above. Building `device_rotate` later means replacing this rule with signed evidence from the old key, not relaxing it.
* **`share_grant` / `share_revoke`.** Sharing is unbuilt, so every "and shared peers" clause below describes nothing.
* **§Revocation step 4.** Nothing wipes the revoked device's local database, and no UI explains the situation to whoever is holding it.

  Step 3 is built, under the three conditions in the implemented list above, and **so is peer-side refusal** — for the two **control** ops whose effect the engine can re-derive ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md), [#82](https://github.com/justin13888/Sunrise/issues/82), closed). What is not built, and is a decision rather than a gap, is peer-side refusal of a revoked device's **entity** writes: a task edit, a note, a focus session. The dilemma is real for that family and ADR-0041 declines it on exactly these grounds — refusing one without advancing the cursor stalls that stream against relay retention until eviction latches a data-loss warning on every device in the account, and advancing the cursor makes the loss permanent, because a later revocation changes only what is sealed *next* and gives back nothing the cursor has already passed. Note what that reason is **not**: it is not that the cut cannot be corrected. It can, in both directions — see §Revocation below — and the control-op gate does not read the cut at all. The bound for entity writes is therefore the relay (step 3), in force only under the three conditions above; with `[auth] require_device_sig` at its default `false` there is no write bound anywhere, so the control-op refusal is the only line there is rather than defence in depth over ops that should not have been accepted. See [ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md) §Why re-adding a peer-side refusal is not free, which is now scoped to entity writes alone.

  **What the revoking user is shown is built at the moment of the act, and not on the device list afterwards; the two states are different guarantees.** "Revoked locally" is true the moment the op commits; "the relay has stopped accepting it" is true only once the queued `DELETE` has been answered, which needs a network the user may not have — and the second is the one someone pressing the button believes they are getting. The log distinguishes them (`sync.device.revoke_relayed` against `sync.device.revoke_not_relayed`), and so does the core: `Core::relay_revocation_pending` answers it for one device (`crates/sunrise-core/src/relay_intents.rs:54#relay_revocation_pending`). Both clients ask it and say which state the revocation is in — the CLI on the revoke path (`crates/sunrise-cli/src/main.rs:788#dispatch`) and the shared Apple account view in its revocation disclosure (`apps/apple/Sunrise/Views/DeviceListSection.swift:218-224`). What is **not** built is standing disclosure: `DeviceRow` carries no pending flag, so a device list opened the next day cannot say which of its revoked entries the relay has actually been told about. That was [#160](https://github.com/justin13888/Sunrise/issues/160).

  The Apple notice's own lifetime is worth stating exactly, because it is not the "one-shot sheet" it reads as. It is an inline `VStack` inside `Section("Devices")` (`apps/apple/Sunrise/Views/DeviceListSection.swift:31`, `apps/apple/Sunrise/Views/DeviceListSection.swift:176`) — the *Settings container* is the sheet, not the notice — and the `lastRevocation` it renders lives on `DeviceListModel`, which `VaultModels` owns (`apps/apple/Sunrise/App/VaultModels.swift:57`) and which outlives that sheet. Nothing clears it but the "Done" button, and `DeviceListModel.refresh()` re-reads the device list without re-reading `Core::relay_revocation_pending`. So the notice survives closing and reopening Settings within a session, and it goes stale in the direction a reader would not guess: it keeps saying "The relay has NOT been told yet" after a sync has drained the intent, rather than the other way round.

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
two **control** ops and for nothing else: `device_revoke`, and a read-bounded
sender's claim that a third device has been keyed
([#82](https://github.com/justin13888/Sunrise/issues/82), closed). That is the
scope; §Revocation below states both gates in full. A revoked device's *entity*
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
3. For every still-authorized shared peer, it produces a `share_grant` op for the new epoch (HPKE single-shot to the peer identity's `ID_D_pub`).
4. From the next emitted op forward in this Stream, `epoch` in the envelope is the new value; ciphertext is encrypted under `stream_key_<epoch+1>`.
5. The previous epoch's key MUST be retained by the rotating device (and any still-authorized recipient) so historical ops remain readable. Past epochs are never deleted from local storage.

A rotation does **not** re-encrypt historical ops. Historical ops remain readable by anyone who held the previous epoch's key when those ops were created. This is an inherent limit of E2EE. If forward secrecy of historical content is required, the user must export the affected Stream, delete it, and re-create.

### Why we keep epochs

The op log for a Stream may contain ops from multiple epochs interleaved (a slow peer might still be emitting under the old epoch when the rotation lands, until they catch up). Receivers select the decryption key by `(stream_id, epoch)` from the envelope. Past epoch keys are kept in `stream_keys` indefinitely; they are encrypted under `vault_root` like the current key.

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
   device, and the successor's `ID_S_priv` sealed by HPKE to each of their
   `D_D_pub`. One op, because the parts are not independently applicable: a
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
6. The recovery code is carried forward by default: the successor's
   `ID_S_priv || ID_D_priv` is sealed to the **outgoing** `ID_D_pub`, so the
   user's existing BIP-39 code keeps working. `keep_recovery_code: false` skips
   it, which is what to use when the *code* is the thing suspected. Revoking the
   device that holds `ID_D_priv` forces no-carry regardless — the carry share is
   sealed to the very key being excluded — and the user must be told
   (`core.identity.recovery_code_invalidated`).
7. When sharing exists, a fresh `share_grant` to every shared peer for every
   Stream. The peer's `ID_D_pub` is unchanged; only the granting identity has
   new keys, so the share signature changes. Nothing implements sharing yet.

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
per-replica, as §Implementation status above and §Revocation below both state —
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
`ID_S_priv` share out of the roster — with no recovery code and no re-pairing.

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
3. The server stops accepting ops signed by the revoked device. **It cannot learn this from the op**, and it is not asked to. `device_revoke` is an inner op sealed under the vault-meta Stream key, and the relay holds no Stream keys — so `revoked_device_id` is ciphertext to it, not metadata it can read. The alternative, promoting `revoked_device_id` into the **cleartext envelope header**, is refused: it would tell the relay which of an account's devices had been revoked and when, for every account it serves, on an unauthenticated channel — the metadata leak the blind-relay property exists to prevent. So the relay is told **out of band**, by `DELETE /api/v1/devices/by-vault-id/{id}` ([`../06-server/api.md`](../06-server/api.md) §Two names for one device). The route takes the *vault-side* device id because that is the only name a revoking device holds; the older `DELETE /api/v1/devices/{id}` names a ULID the relay mints at registration and never sends back through the op stream, so no vault can address a peer with it.

   The call is queued, not made. `Command::RevokeDevice` writes a row to `relay_revocation_intents` in the same transaction as the op, and `sync_driver::drain_relay_revocations` sends it once a session is up — because a revocation has to work with no network at all, which is the situation a user is in the moment they notice a device is gone. A refusal keeps the row and retries on the next session, logging `sync.device.revoke_not_relayed`, which says outright that the relay is still accepting the device. See [`../10-cross-cutting/log-events.md`](../10-cross-cutting/log-events.md).

   **Two conditions bound what this buys.** The relay only refuses a *device-bound* request: with no `X-Sunrise-Device-Sig` and `[auth] require_device_sig` false — which is the default — no device is resolved and no revocation check runs, so a revoked device that stops signing keeps uploading. And a device that registered before clients sent `vault_device_id` has nothing on its relay row to match; the `DELETE` answers `404`, logged as `sync.device.revoke_unknown_to_relay`, and that device goes on being accepted.
4. The revoked device, when next online, sees the revocation in its inbox; the UI explains the situation and the local DB is wiped on first launch.

A revoked device that never reconnects retains whatever plaintext it had at the moment of revocation. We are honest about this in the UI.

**A revocation cannot be undone today.** No command, seam export or client surface withdraws one. A device revoked by mistake is readmitted only by pairing it again as a new device. That mints a fresh `device_id`, and the new device holds no `ID_S_priv` (#221). The CLI's `device revoke` says so; the device list's Remove confirmation does not yet ([#384](https://github.com/justin13888/Sunrise/issues/384)).

[ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md) decides the inverse, and [#383](https://github.com/justin13888/Sunrise/issues/383) builds it:

- **The device that made a revocation can withdraw it, and no other device can.** A withdrawal is a new control op, folded as the head of its `(sender, target)` pair in the ledger. A ledger with a withdrawn pair is the ledger in which that claim was never made, so the register stays a pure function of the op set.
- **The fold releases the device's read bound on a replica only under three conditions.** The register must no longer name the device, no live claim may name it, and at least one withdrawal of it must come from an ungated sender.
- **The withdrawing device then re-seals every epoch it holds to the reinstated device.** This includes the epochs minted while it was revoked, and the result says so. The rotation itself is not undone.
- **The relay's `revoked = 1` is reversed only on the signed word of the device the relay recorded as the revoker.**

The op waits for [#320](https://github.com/justin13888/Sunrise/issues/320) and [#324](https://github.com/justin13888/Sunrise/issues/324). Until an unknown op kind is parked rather than dropped as corruption, an older build would never see a withdrawal, and the register would split between builds. Parking does not reach a build that predates it, so a withdrawal is also emitted only behind the `core.revoke_withdraw` feature, and the command refuses while any non-revoked device, or the device being reinstated, does not advertise it (ADR-0056 §7).

**Step 2 also rotates the account identity, when the revoking device can.** Only the device the account was created on holds `ID_S_priv`, so a revocation run from a paired device cuts every future Stream key and leaves the identity where it is, logging `core.identity.rotation_unavailable`. That costs less than it sounds: since #105 a revoked device holds no signing key unless it is the creator, so for every other device there is no certificate for the rotation to have invalidated. Revoking the **creator** is the case that needs the rotation, and it has to be run from the creator.

**What converges, and what does not.** Every replica applies every **entity** op it can decrypt — a task edit, a note, a focus session — whatever its sender's revocation state and whatever order the `device_revoke` and the op arrive in. There is no refusal record for those anywhere in the tree, so delivery order is not an input to the materialized state: two replicas holding the same op set hold the same task table. What the apply path does consult is the read bound, and only to decide key distribution: applying a `DeviceCertPublish` runs `backfill_key_envelopes`, which returns early on a read-bounded device — it tested `Engine::is_revoked` until migration 0028 gave the bound its own table — so what is settled there is which device is sealed key material, never whether the op applies. Two consequences follow and are worth stating outright, because they are what makes the rest of this section safe.

* **A cut correction is lossless for user work.** Nothing a user typed was refused under the old cut, so a revocation re-issued from a healthy device changes what is sealed *next* and destroys nothing already applied. That is what lets the register move in both directions — the LWW rule above — instead of being an irreversible `MIN`.
* **The write bound for entity ops belongs at the relay.** Refusing one at apply time cannot be made convergent without a projection rebuild this engine does not have. Step 3 above is that bound. Note what it does *not* reach: a device on any other transport, a relay the account does not use, and the two conditions named in step 3.
* **`device_revoke` is the one family where revocation reaches backwards.** Every other op a revoked device ever wrote still stands, including a cert it issued under an identity since retired. A `device_revoke` is the one the fold can discard, and no date saves a row from it: the date is a number its sender chose, and nothing in the ledger separates an honestly-earlier revocation from a back-dated one, so the fold reads no clock for this at all. *Which* rows it discards is the "**A revoked device cannot revoke another device**" bullet below, with the gate's one exception and the discount pass's two residuals; this bullet names the family and no more. The visible form of that is `core.device.revocation_unwound` — a device coming back off the revoked list because the replica learned who revoked it — and the remedy, as everywhere else in this section, is to make the revocation again from a device the account still trusts. **What the unwind does not give back is the keys.** The read bound is a second, monotone table (`device_read_bounds`, migration 0028) that the four key-distribution sites read — and a fifth that distributes no key, the third-party recipient claim in the bullet below — and that nothing ever deletes from, so an unwound device shows as current while receiving nothing. That asymmetry is deliberate — a bound a later op could release would not be a bound — and it is disclosed rather than left to be inferred: `DeviceRow::read_bounded` puts it on every device list, and `CommandResult::revocation_unwound` names the devices in that state on the next revocation. It is tolerable only because the effect can be re-derived, which is the same reason the gate stops at control ops.

**Two control ops are bounded at the peer** ([ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md), [#82](https://github.com/justin13888/Sunrise/issues/82)), because their effect is a register the engine can re-derive rather than a row it could never un-write.

* **A revoked device cannot revoke another device — with the one exception and the two residuals stated below, which is the whole of what this bullet promises.** It could, until ADR-0041: its cert is still on the chain, it still holds the vault-meta key for the epoch it was cut at, and revocation has no inverse — so a laptop the account had expelled could expel the account, permanently, on every replica. `device_revocations` is now a **fold** over `device_revoke_ops` (migration 0027). The ledger keeps every such op except where two ops name the same `(sender, target)` pair: then it keeps only the one with the higher stamp, because the fold reads nothing from the other ([#250](https://github.com/justin13888/Sunrise/issues/250)). It also keeps at most 256 distinct targets per sender, the ones with the newest stamps, and drops the rest of that sender's pairs; that bounds a sender naming ids nobody has, and it drops only the flooding sender's own claims ([#315](https://github.com/justin13888/Sunrise/issues/315)). A row whose sender the ledger revokes **anywhere in it** is skipped — unless the only party to have revoked that sender is the device the row is about, which is what keeps two devices revoking each other converging on *both* revocations rather than letting a back-dated op silence its target. Anywhere in it, rather than in the part of the ledger sorting below the row, and that is the security content of the rule: the sort key is the op's HLC, an HLC is whatever its sender wrote, and a sender that could clear the gate by dating an op a millisecond below its own cut would have no gate at all. The fold reads no clock for this — not this device's and not the op's — it asks who has revoked whom.

  The exception has a cost a user can reach, so it is stated rather than deduced: **after two devices have revoked each other, neither can revoke anybody else**, because each one's only revoker is the other, so each is forgiven for revoking the other and gated for revoking a third party. That reaches the honest device of the pair. It lasts until a third current device revokes the half it believes compromised. That revocation gates the compromised half's revocation of the honest one and discounts it out of the honest one's revoker set, so the honest device is current and ungated again ([ADR-0056](../11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md) §3). Its read bound stays on any replica that had taken it. The loss is narrow, and narrow is a bound rather than a hope: a revoked device enters the revoker set of a device V only when V is the **only** device that revoked it, so a device it merely named is untouched and so is a second device that also revoked it. Revocation is not gated on `ID_S_priv` anywhere either, so identity rotation and pairing sponsorship are untouched and **any current device outside the pair still revokes whoever it likes**. The lockout is total only in a two-device account, where there is no third device to ask. What that bound gives up is that a device whose sole revoker is itself revoked by a third party stops being gated. **That third party need not be a bystander**, which is the reading a threat model has to take: one attacker holding two devices the same revoker expelled reaches it with a single op. O revoked X1 and X2; X1 revokes O; the mutual exception lands that op, and O going out both unwinds O's revocation of X2 and discounts O out of X2's revoker set, so X2 revokes the rest of the account. The remedy is the mutual pair's and no better — a device X2 reaches revokes it back and is left revoked itself — so what the account needs is a current device the attacker never reached. Extend the same chain by one link and the second shape appears: a device on the revoked list while being ungated. Neither is reachable out of the revoked device's own rows, since a device authors only rows whose sender is itself. See §"What a user sees" item 4 in [ADR-0041](../11-adr/0041-peer-side-revocation-is-a-fold.md) for both, with the tests that pin them.
* **A read-bounded device cannot claim a third device has already been sent a key** — which is every revoked device and, after an unwind, more. The `key_envelope_recipients` row the `Recipient::Device(other)` arm files is what `backfill_key_envelopes` reads to decide a device is served, and filing one for somebody else is a way to withhold a key from them. A **read-bounded** sender's claim is no longer recorded: the gate asks `device_read_bounds` and not the register, because the class worth refusing is the one whose *reads are bounded*, and an unwound device (`read_bounded: true, revoked: false`) is in it while the register has stopped naming it. The table is a hint rather than state, so declining a row can only cause *more* key distribution and never less — which is why this one gate can afford a predicate that does not converge across replicas, where the fold's could not.

Both skips are **stored, not dropped**, and both are visible: `core.device.revoke_refused` with `reason = "revoked_sender"`, and `core.device.revocation_unwound` when a re-fold stops believing a revocation somebody has already seen. A cut correction does **not** un-skip a revocation — the gate asks who has revoked whom and reads no cut, so a correction changes which row wins the register and leaves the gate's answer exactly as it was, whichever direction it moves the cut — and the remedy is to make the revocation again from a device the account still trusts. That is tolerable for an administrative act and would not be for a task edit, which is exactly why the gate stops where it does.

So the guarantee a user's device may promise is one clause, and the scope is part of the promise: **on a replica that applied the revocation while its sender was still ungated there, the device reads nothing written afterwards.** Not that it stops writing ordinary content, and not that what it wrote before the revocation reached everyone is rolled back — and **not that the account promises it, only this replica.** That last is the scope and not a hedge: the bound rests on `device_read_bounds`, which is a ratchet over what *this* replica's own folds produced and does **not** converge, so a sibling replica that met the same two ops in the other order may still be sealing that device keys. `Engine::is_read_bounded` says the same thing at the function: a caller may read it as "has *this* replica bounded the device" and may not read it as "has the account". [#282](https://github.com/justin13888/Sunrise/issues/282) is the half that carries the counterexample, and closing it is what would let this sentence quantify over the account.

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
| `sunrise.account_email_hash.v1` | three frozen bucket keys |
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
