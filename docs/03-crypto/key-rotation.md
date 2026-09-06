---
status: accepted
---

# Key Rotation

Three key types rotate, each with a different cost and cascade. Throughout this spec, "the rotating device" is the device the user initiated rotation from; it MUST be a paired, currently-authorized device.

## Implementation status: Stream-key rotation and revocation are built; identity rotation is not

[ADR-0024](../11-adr/0024-key-hierarchy.md) landed the hierarchy these procedures assume: Stream keys are 32 random bytes per `(stream_id, epoch)`, wrapped under the vault root in `stream_keys`, and distributed by HPKE `key_envelope` ops.

**Built:**

* **§Stream key rotation**, in full. `Keychain::mint_epoch` draws a fresh key, `Engine::emit_key_envelopes` seals it to every sibling device's `D_D_pub` and to the identity's `ID_D_pub` (revocation is not filtered here — see §Revocation), and past epochs are retained: `stream_keys` is keyed `(stream_id, epoch, key_id)` and a decrypt tries every key at `(stream_id, epoch)`, so two devices minting one epoch concurrently both keep theirs. `Command::RotateStreamKey` is the narrow entry point.
* **§Revocation is recorded and converged, and enforced nowhere.** `Command::RevokeDevice` emits a `device_revoke` op and rotates every stream in the rotation set — the vault-meta stream and the Inbox included — so the machinery of §Revocation steps 1 and 2 exists. What does **not** exist is any consequence of being revoked.

  Two enforcement claims were built here and both were removed, because each was false and each cost more than it bought:

  * *A revoked device receives no new epoch key.* False. `emit_key_envelopes` seals every epoch to each device **and to the account identity**, and pairing hands every device `ID_D_priv` — so a revoked device opens the identity copy of every new epoch and reads straight through the rotation. Excluding it from the device recipients withheld nothing. That is [#76](https://github.com/justin13888/Sunrise/issues/76).
  * *A revoked device's writes are refused by its peers.* Removed. A refused op leaves the refusing replica's sync cursor for that device frozen while the relay, which knows nothing of the revocation, goes on accepting its uploads; within retention the eviction gap fires on every subscribe and latches `Degraded`, so an ordinary revocation would put every device in the account into a permanent, unclearable data-loss warning. Withholding its Stream key instead is worse still: `defer_op` returns before the op log, so every op behind it in that stream parks forever on a key that will never arrive. Bounding writes needs the relay to stop accepting them — [#82](https://github.com/justin13888/Sunrise/issues/82), behind [#80](https://github.com/justin13888/Sunrise/issues/80).

  What the register does provide is agreement. The cut is the **HLC of the `device_revoke` op itself**; there is no `effective_at` field for an emitter to choose or for anyone to bound. Concurrent revocations resolve as an LWW register on that op's own `(hlc, device_id)` — the rule [ADR-0014](../11-adr/0014-entity-level-lww-merge.md) already resolves every other concurrent write in this engine with — so every replica reaches the same cut whatever order it saw them in, a cut that landed wrong is correctable by revoking again, and a device cannot move its own cut. Converging the *effect* of a revocation rather than only its record is [#78](https://github.com/justin13888/Sunrise/issues/78).
* **§Slow peers and out-of-order epochs**, as written. There is no per-epoch barrier: an op whose key has not arrived is parked in `deferred_ops` and retried after every absorbed key, so arrival order across epochs changes nothing.

**Not built, and named here rather than discovered later:**

* **Identity rotation.** There is no `identity_transition` op. This is not a detail: pairing hands every device `ID_D_priv` (see [`pairing-and-onboarding.md`](./pairing-and-onboarding.md)) and every epoch is sealed to the identity as well as to each device, so **a revoked device can still open the identity's copy of every new epoch**. Excluding a revoked device from the *device* recipients therefore withholds nothing, which is why this slice does not try — see §Revocation. It also still holds `ID_S_priv`, so it can issue itself a fresh valid cert under a new device id. Closing both needs step 1 of §Identity rotation.
* **§Device key rotation.** No `device_rotate`; a device's `D_S` / `D_D` are minted once at open and never replaced.
* **`share_grant` / `share_revoke`.** Sharing is unbuilt, so every "and shared peers" clause below describes nothing.
* **§Revocation steps 3 and 4.** The relay does not read `device_revoke` and does not refuse a revoked device's uploads. Nothing wipes the revoked device's local database.

  **There is no client-side substitute, which is why this PR stops here.** A revoked device keeps its relay credentials, so it goes on uploading and the relay goes on offering those ops to every peer. A peer cannot durably decide otherwise: refusing without advancing its cursor stalls that stream against relay retention, and advancing makes a decision permanent that a corrected cut must be able to undo. The fix belongs at the relay — `Command::RevokeDevice` calling `DELETE /api/v1/devices/{id}`, which already exists and already refuses a revoked device's *signed* uploads and sync sessions where device-signature binding is in force. That is not built: `sunrise-core` has no HTTP client, the relay API client lives in a crate only the CLI depends on, and `Engine::apply` is synchronous and inside a transaction — so it needs a durable intent the sync driver drains, with its own failure semantics for a revocation emitted while the relay is unreachable. Tracked as [#82](https://github.com/justin13888/Sunrise/issues/82) behind [#80](https://github.com/justin13888/Sunrise/issues/80).

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
3. The server stops accepting ops signed by the revoked device. **It cannot learn this from the op.** `device_revoke` is an inner op sealed under the vault-meta Stream key, and the relay holds no Stream keys — so `revoked_device_id` is ciphertext to it, not metadata it can read. For this step to work, one of two things has to change: the server is told **out of band** (`DELETE /api/v1/devices/{id}`, which already exists and already refuses a revoked device's signed uploads and sync sessions), or `revoked_device_id` is promoted into the **cleartext envelope header**, which would tell the relay which of an account's devices had been revoked and when, for every account it serves — the metadata leak the blind-relay property exists to prevent. Tracked as [#80](https://github.com/justin13888/Sunrise/issues/80); this document records the constraint rather than settling the choice.
4. The revoked device, when next online, sees the revocation in its inbox; the UI explains the situation and the local DB is wiped on first launch.

A revoked device that never reconnects retains whatever plaintext it had at the moment of revocation. We are honest about this in the UI.

## What rotation does not do

- It does not retroactively un-leak content. A revoked device keeps whatever plaintext it had on disk.
- It does not erase the *fact* of past activity from server-side metadata (timestamps, op counts).
- It does not affect historical ops the rotating side already emitted under prior keys; the op log is by design replayable to converge state.
