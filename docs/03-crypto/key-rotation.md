---
status: accepted
---

# Key Rotation

Three key types rotate, each with a different cost and cascade. Throughout this spec, "the rotating device" is the device the user initiated rotation from; it MUST be a paired, currently-authorized device.

## Implementation status: Stream-key rotation and revocation are built; identity rotation is not

[ADR-0024](../11-adr/0024-key-hierarchy.md) landed the hierarchy these procedures assume: Stream keys are 32 random bytes per `(stream_id, epoch)`, wrapped under the vault root in `stream_keys`, and distributed by HPKE `key_envelope` ops.

**Built:**

* **§Stream key rotation**, in full. `Keychain::mint_epoch` draws a fresh key, `Engine::emit_key_envelopes` seals it to every **unrevoked** sibling device's `D_D_pub` and to the identity's `ID_D_pub`, and past epochs are retained: `stream_keys` is keyed `(stream_id, epoch, key_id)` and a decrypt tries every key at `(stream_id, epoch)`, so two devices minting one epoch concurrently both keep theirs. `Command::RotateStreamKey` is the narrow entry point.
* **§Revocation bounds a revoked device's reads. It does not bound its writes, and nothing else does either — and no replica refuses its ops, which is what keeps the *effect* convergent ([ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md)).** `Command::RevokeDevice` emits a `device_revoke` op, records the register, and rotates every stream in the rotation set — the vault-meta stream and the Inbox included. It makes no request of the relay: an earlier slice queued one and it was reverted, because the request names an id the client cannot know.

  * *Reads* are bounded in the vault — **once each replica has applied the revocation** — and it takes two changes that are each vacuous alone. The register is per-replica and propagates like any other op, so a device that has not yet seen the `device_revoke` will seal a new epoch to the revoked device; a replica that has been offline while Streams were created on it delivers those epochs when it reconnects. Revocation is eventually consistent, and the "cut" is the point after which *informed* replicas withhold. `emit_key_envelopes` anti-joins `device_revocations`, so a revoked device is sealed no envelope for any epoch minted at or after its cut; and `PairingPayload` no longer carries `ID_D_priv`, so there is no identity-sealed copy for it to open instead. While pairing handed every device the identity's unwrapping key, excluding a device from the recipient list withheld nothing — which is why an earlier slice removed the exclusion and filed [#76](https://github.com/justin13888/Sunrise/issues/76) rather than shipping half of it. Sealing needs only the public half, so the identity copy is still emitted for every epoch and recovery still reaches all of them.

    Removing that fallback opens a gap and `Engine::backfill_key_envelopes` closes it: a device certified *after* an epoch was minted was left out of that epoch permanently, and minting happens whenever a Stream is created. A replica applying a `device_cert` now seals **every** epoch it holds to the newly certified device, and `key_envelope_recipients` (migration 0018) records who has been sent what so a backfill emits only what is missing and a re-published cert emits nothing. Every epoch and not the live one per stream: current-epoch-only was the first shape and it was wrong wherever a rotation landed between a device's pairing and its certificate, which leaves that device holding the payload's epochs and the live one with a silent permanent hole in between — ops sealed under the missing epochs park in `deferred_ops` and expire at the TTL, presenting as "some items from around when I set up this device never arrived". That was [#107](https://github.com/justin13888/Sunrise/issues/107). A revoked device is skipped, or revocation would be undone by re-sending a cert.

    **One exception, and it is real.** A device paired while `STORAGE_V` was 17 wrapped the `ID_D_priv` its payload carried into its own `identity` row, and migration 0018 could not clear it: on the account's creator that column holds the only copy of the key and the schema recorded nothing that distinguished the two. It recorded nothing under that name. `stream_keys.source` did — only a founding vault mints the vault-meta stream's first epoch, so a `local` or `legacy` row at epoch 1 is proof of having minted the account identity and its absence is proof of the opposite. Migration 0019 reads that, records the answer in `identity.minted_by_device_id` so nothing has to infer it again, and clears the column everywhere else; `Keychain::load` applies the same test at open, so a copy that reaches the column by any other route is inert. That was [#87](https://github.com/justin13888/Sunrise/issues/87).

    The exception that remains: the device that *created* the account, and any device restored from the recovery code, hold `ID_D_priv` and can open the identity copy of any epoch. `Command::RevokeDevice` refuses to revoke the device it runs on, so this is reachable only by revoking the account's creator from another device. The recovery blob is the only other place that key is allowed to live, and it is ciphertext behind the user's code rather than a device the account can revoke — so the bound is stated rather than claimed.

    The same fact has a second consequence, in the other direction, and it is the one a user feels: **where no recovery blob has been sealed, that vault is the only place `ID_D_priv` exists, and losing it destroys the key permanently.** No recovery feature added later can retrieve it, because sealing a blob needs the key it would carry. `sunrise bootstrap` seals one at account creation, so a CLI-created account has the second copy; the Apple clients do not, so an account created there does not. See [`recovery.md`](./recovery.md) §Implementation status. `Keychain::holds_only_copy_of_identity_key` answers it in the core API and `Core::holds_identity_key` passes it through; there is still no binding, so the Apple app cannot ask.

  * *Writes are bounded **at the relay, conditionally**.* The relay cannot learn the revocation from the op stream and must not be able to — `device_revoke` is sealed under the vault-meta Stream key, and promoting the revoked id into the cleartext envelope header would tell the relay which of an account's devices had been revoked and when, for every account it serves — so it is told out of band. `Command::RevokeDevice` queues a `relay_revocation_intents` row in the same transaction as the op and the sync driver drains it to `DELETE /api/v1/devices/by-vault-id/{id}`, retrying on every session until the relay answers. That route takes the vault-side device id, because the relay's own `device_id` is a ULID it mints at registration and never sends back through the op stream — which is why the older route could not express a revocation at all, and why [#80](https://github.com/justin13888/Sunrise/issues/80) was a relay API change before it was a client one. §Revocation step 3 has the mechanism.

    **Three conditions.** The relay only enforces against a *device-bound* request, so with `[auth] require_device_sig` at its default `false` a revoked device that stops signing keeps uploading. A device that registered before clients sent `vault_device_id` has nothing to match and is still accepted, logged as `sync.device.revoke_unknown_to_relay`. And nothing bounds a device on any other transport, or on a relay this account does not use. Peers still apply a revoked device's ops whatever the relay did.

  * *Peers do not refuse a revoked device's ops*, and that is a decision rather than a gap. Refusing at apply time is not convergent — a replica that applied an op before the revocation reached it cannot un-apply it, and this engine has no projection rebuild — so two replicas with the same op set would disagree permanently. It also freezes the refusing replica's sync cursor for that device, which within retention latches an unclearable data-loss warning on every peer. A convergent peer-side check is [#82](https://github.com/justin13888/Sunrise/issues/82); converging the *effect* rather than the record is [#78](https://github.com/justin13888/Sunrise/issues/78).

  What the register provides is agreement. The cut is the **HLC of the `device_revoke` op itself**; there is no `effective_at` field for an emitter to choose or for anyone to bound. Concurrent revocations resolve as an LWW register on that op's own `(hlc, device_id)` — the rule [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) already resolves every other concurrent write in this engine with — so every replica reaches the same cut whatever order it saw them in, a cut that landed wrong is correctable by revoking again, and a device cannot move its own cut.

  What no rotation can do, however complete: take back what the device already had. Revocation is forward-only.

* **§Slow peers and out-of-order epochs**, as written. There is no per-epoch barrier: an op whose key has not arrived is parked in `deferred_ops` and retried after every absorbed key, so arrival order across epochs changes nothing.

**Not built, and named here rather than discovered later:**

* **Identity rotation.** There is no `identity_transition` op, and two consequences follow from that rather than from the revocation design.

  The first is the creator exception in §Revocation: `ID_D_priv` has to stay on a device somewhere, and that somewhere is the account creator's own vault — the recovery blob is a copy behind the user's code, not a device that could hold it instead. Identity rotation is what would let a revocation move the account to an identity the revoked device never held.

  The second is unchanged, is not closed by anything above, and is **unmitigated**: a revoked device still holds `ID_S_priv`, so it can issue itself a fresh valid `DeviceCert` under a new device id. Revocation names a device, and the identity keys are what name devices. `Engine::self_authenticating_signer` admits such a cert, applying a `device_cert` op runs `Engine::backfill_key_envelopes`, and the fresh id is sealed every epoch the applying replica holds — so revocation is undone in one round trip. `engine::tests::a_revoked_device_rejoins_under_a_fresh_device_id` asserts exactly that, so this is a measured property and not a worry. Nothing stands against it today.

  Two claims that stood here are withdrawn. Earlier revisions named the relay declining the revoked device's upload as the mitigation. The relay does decline it now (§Revocation step 3), and it still is not a bound on *this*: the fresh device id is a fresh registration, made with the OIDC bearer the device kept, so it arrives as an unrevoked relay device row and uploads its own certificate normally. Later ones named a narrower fix as available and merely unbuilt — *refuse to backfill a device id first seen in a cert whose signer is already revoked* — and **there is no signer to key that on**. A `DeviceCert` is signed by `ID_S_priv`, which is the account's and not any device's, and the `device_cert` op is signed by the subject's own `D_S_priv`, which on a fresh id the revoked device also minted. Both halves are the attacker's; an issuer field would be a value it chooses. In the honest flow there is no issuer to name either, because `Keychain::create` has the joining device self-issue its own certificate.

  [ADR-0032](../11-adr/0032-revocation-cannot-bound-cert-issuance.md) records the four narrow shapes that were priced against this tree and what killed each: an unverifiable issuer field, a sponsor-countersigned certificate (sound, but it restructures pairing and permanently locks out every device a revoked one ever paired), a check on the epoch a cert arrived under (does not close the round trip, and locks out an honest device whose cert races a rotation), and an apply-time refusal (not convergent). Step 1 of §Identity rotation is the complete fix and the only one: it stops `ID_S_priv` signing anything the account accepts. Until it exists, a replica applying a certificate for a device id it has never seen, in an account that has revoked something, logs `core.device.admitted_after_revocation` — which is what an ordinary pairing looks like too, because inside the vault the two are the same event. This is [#105](https://github.com/justin13888/Sunrise/issues/105).
* **§Device key rotation.** No `device_rotate`; a device's `D_S` / `D_D` are minted once at open and never replaced.
* **`share_grant` / `share_revoke`.** Sharing is unbuilt, so every "and shared peers" clause below describes nothing.
* **§Revocation step 4.** Nothing wipes the revoked device's local database, and no UI explains the situation to whoever is holding it.

  Step 3 is built, under the three conditions in the implemented list above. **What is still not built is peer-side refusal**, and that is a decision rather than a gap: a peer cannot durably decide otherwise, because refusing without advancing its cursor stalls that stream against relay retention, and advancing makes a decision permanent that a corrected cut must be able to undo. With the relay refusing the uploads there are no ops to refuse in the ordinary case, which is what makes peer-side enforcement cheap defence in depth rather than the only line. That is [#82](https://github.com/justin13888/Sunrise/issues/82) and [ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md) §What would force revisiting this.

  **What the revoking user is shown is also unbuilt, and the two states are different guarantees.** "Revoked locally" is true the moment the op commits; "the relay has stopped accepting it" is true only once the queued `DELETE` has been answered, which needs a network the user may not have — and the second is the one someone pressing the button believes they are getting. Today the difference is only in the log (`sync.device.revoke_relayed` against `sync.device.revoke_not_relayed`); nothing surfaces it, and `Core` exposes no query for the pending queue.

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
(`crates/sunrise-server/src/store.rs`), `Store::active_device` filters on it,
and that refuses every subsequent signed request
(`crates/sunrise-server/src/api/signed.rs`) and ends a live SSE session with
`AUTH_DEVICE_REVOKED` (`crates/sunrise-server/src/api/sync.rs`). It is a
time-less binary flag on relay metadata the relay already holds, not a content
check, and it refuses the *device's credential* rather than ops signed by a
superseded key. In the vault, the matching check would be the receiving client's
— and it is **not built**: a replica today applies an op from a revoked device
like any other ([#82](https://github.com/justin13888/Sunrise/issues/82)). The
relay cannot make that check for it: it does not open envelopes, and
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

**Procedure.**

1. The rotating device generates new `ID_S'`, `ID_D'` keypairs.
2. It emits an `identity_transition` control op:
   ```cddl
   IdentityTransition = {
       1: bstr .size 16,    ; identity_id_bytes (unchanged — same identity)
       2: bstr .size 32,    ; new ID_S_pub
       3: bstr .size 32,    ; new ID_D_pub
       4: bstr .size 64,    ; sig by OLD ID_S_priv over fields {1,2,3}
       5: bstr .size 64,    ; sig by NEW ID_S_priv over fields {1,2,3,4}
       6: uint              ; effective_at (ms since epoch)
   }
   ```
   Both signatures are required so receivers can verify the rotation came from someone holding the old key, and that the new key is a willing successor. Receivers MUST verify in order: old signature first, then new signature; reject on either failure with `CRYPTO_TRANSITION_INVALID`.
3. It re-issues a fresh `DeviceCert` for **every other still-authorized device** under the new `ID_S_priv'`. (Other devices' own `D_S` and `D_D` are unchanged; only their cert is replaced.) These `device_cert` ops are emitted by the rotating device on behalf of the others. Each peer device, upon seeing both the `identity_transition` and its updated cert, treats itself as still-authorized.
4. It uploads a fresh recovery blob using the new identity keys (the recovery code itself MAY be rotated at the same time; it is the user's choice).
5. It emits a fresh `share_grant` to **every shared peer** for every Stream (the peer's identity DH `ID_D_pub` is unchanged; only the granting identity has new keys, so the share signature changes).
6. It re-publishes `{identity_id, new ID_S_pub, new ID_D_pub}` to the server's identity registry, signed by the new `ID_S_priv'`. The server replaces the public bundle and retains the previous one for `effective_at + 7 days` so peers in flight can still verify recently-emitted ops.

### Atomicity & recipient buffering

The `identity_transition` op is a synchronization point.

- The rotating device emits `identity_transition` and immediately emits `share_grant` for every shared Stream under the new identity. **All ops are submitted in the same batch; the relay forwards them as a single atomic OpBatch.**
- Recipients buffer `identity_transition` until they have received at least one new `share_grant` for every Stream they previously held. Until then, ops signed by the new identity are **buffered, not applied**.
- If the new `share_grant` batch is incomplete after 24 h (recipient sees the transition but not all grants), the recipient surfaces: *"Some shares from <person> are pending re-grant."*

**Stream keys are NOT rotated as a consequence of identity rotation by itself.** Stream keys rotate only when a device or peer is revoked. If the user's reason to rotate identity is "I think someone has my identity key," that someone could only impersonate me going forward (forge ops); they could not read content unless they also held a Stream key. If the user wants both, they perform identity rotation followed by Stream key rotation per Stream, and the UI offers a single "rotate everything" affordance that does it.

Cost on a typical user (10 devices, 30 Streams, 5 sharing peers): O(devices + streams + peers) = ~45 control ops; wall time on a 4G connection a few seconds.

## Revocation

Revocation = removing a device from the identity. Triggered from any other still-authorized device.

**Procedure.**

1. The trusted device emits a `device_revoke` op for the target device, signed by the trusted device.
2. The trusted device performs a Stream key rotation **for every Stream the revoked device had access to** (all of them, in v1). This produces fresh `key_envelope` ops for sibling devices and `share_grant` ops for peers under the new epoch — but pointedly NOT for the revoked device.
3. The server stops accepting ops signed by the revoked device. **It cannot learn this from the op**, and it is not asked to. `device_revoke` is an inner op sealed under the vault-meta Stream key, and the relay holds no Stream keys — so `revoked_device_id` is ciphertext to it, not metadata it can read. The alternative, promoting `revoked_device_id` into the **cleartext envelope header**, is refused: it would tell the relay which of an account's devices had been revoked and when, for every account it serves, on an unauthenticated channel — the metadata leak the blind-relay property exists to prevent. So the relay is told **out of band**, by `DELETE /api/v1/devices/by-vault-id/{id}` ([`../06-server/api.md`](../06-server/api.md) §Two names for one device). The route takes the *vault-side* device id because that is the only name a revoking device holds; the older `DELETE /api/v1/devices/{id}` names a ULID the relay mints at registration and never sends back through the op stream, so no vault can address a peer with it.

   The call is queued, not made. `Command::RevokeDevice` writes a row to `relay_revocation_intents` in the same transaction as the op, and `sync_driver::drain_relay_revocations` sends it once a session is up — because a revocation has to work with no network at all, which is the situation a user is in the moment they notice a device is gone. A refusal keeps the row and retries on the next session, logging `sync.device.revoke_not_relayed`, which says outright that the relay is still accepting the device. See [`../10-cross-cutting/log-events.md`](../10-cross-cutting/log-events.md).

   **Two conditions bound what this buys.** The relay only refuses a *device-bound* request: with no `X-Sunrise-Device-Sig` and `[auth] require_device_sig` false — which is the default — no device is resolved and no revocation check runs, so a revoked device that stops signing keeps uploading. And a device that registered before clients sent `vault_device_id` has nothing on its relay row to match; the `DELETE` answers `404`, logged as `sync.device.revoke_unknown_to_relay`, and that device goes on being accepted.
4. The revoked device, when next online, sees the revocation in its inbox; the UI explains the situation and the local DB is wiped on first launch.

A revoked device that never reconnects retains whatever plaintext it had at the moment of revocation. We are honest about this in the UI.

**What converges, and what does not.** Every replica applies every op it can decrypt, whatever its sender's revocation state and whatever order the `device_revoke` and the op arrive in. There is no refusal record anywhere in the tree and `Engine::is_revoked` has no caller in the apply path, so delivery order is not an input to the materialized state: two replicas holding the same op set hold the same task table. Two consequences follow and are worth stating outright, because they are what makes the rest of this section safe.

* **A cut correction is lossless.** Nothing was refused under the old cut, so a revocation re-issued from a healthy device changes what is sealed *next* and destroys nothing already applied. That is what lets the register move in both directions — the LWW rule above — instead of being an irreversible `MIN`.
* **The write bound belongs at the relay, and peer-side enforcement comes after it.** Refusing at apply time cannot be made convergent without a projection rebuild this engine does not have, and it is what would reintroduce order-dependent divergence. Step 3 above is that write bound; [#82](https://github.com/justin13888/Sunrise/issues/82) is defence in depth on top of it, not a substitute for it. Note what the relay bound does *not* reach: a device on any other transport, a relay the account does not use, and the two conditions named in step 3.

So the guarantee a user's device may promise is: **once this replica knows about the revocation, the revoked device reads nothing written afterwards.** Not that it stops writing, and not that what it wrote before the revocation reached everyone is rolled back. The decision, its rejected alternatives and what would reopen it are [ADR-0034](../11-adr/0034-revocation-bounds-reads-not-writes.md).

## What rotation does not do

- It does not retroactively un-leak content. A revoked device keeps whatever plaintext it had on disk.
- It does not erase the *fact* of past activity from server-side metadata (timestamps, op counts).
- It does not affect historical ops the rotating side already emitted under prior keys; the op log is by design replayable to converge state.
