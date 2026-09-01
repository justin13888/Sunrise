---
status: accepted
---

# Key Rotation

Three key types rotate, each with a different cost and cascade. Throughout this spec, "the rotating device" is the device the user initiated rotation from; it MUST be a paired, currently-authorized device.

## Implementation status: none of this is built

**Every procedure in this document is a target, not a description.** [ADR-0024](../11-adr/0024-key-hierarchy.md) is the governing decision and explains why: rotation is not implementable on the key hierarchy the tree actually has.

The evidence, so this is checkable rather than asserted:

* **No rotation entry points exist.** There is no `device_rotate`, `device_revoke`, `identity_transition`, re-wrap, or epoch bump anywhere in `crates/`.
* **No control ops exist.** The op kinds this spec depends on — `key_envelope`, `device_cert`, `device_revoke`, `share_grant`, `share_revoke`, `snapshot`, `checkpoint`, `identity_transition` — have no implementation. All 21 variants of `InnerOp` in `crates/sunrise-core/src/inner_op.rs` are domain CRUD (task, stream, context, routine, block, attachment, focus, review).
* **Epochs do not move.** `crates/sunrise-core/src/keychain.rs` pins `pub const EPOCH: u32 = 1` and derives every Stream key as `BLAKE3.derive_key("sunrise.stream_key.v1", vault_root || stream_id || u32_be(epoch))`. Bumping that constant re-derives a key anyone holding the vault root can also compute — it rotates ciphertext, not the secret. ADR-0024 §Alternatives rejects exactly that as "rotation that looks like rotation and is not".
* **Stream-key rotation would be vault-root rotation.** Because every Stream key derives from the one account-wide root, there is no per-Stream unit to rotate. Rotating one rotates all.
* **Revocation is not expressible.** A paired device holds the vault root, and the root *is* the whole key schedule. Nothing a still-authorized device emits can take that back.
* **HPKE, which steps 2 and 3 of Stream-key rotation require, has no consumer.** `hpke = "0.13"` sits in `[workspace.dependencies]` and no member `Cargo.toml` references it.

ADR-0024 makes Stream keys independently random per `(stream_id, epoch)`, wrapped under the vault root, distributed by `key_envelope` ops and read from the `stream_keys` table — which is what the procedures below assume. Until that slice lands, treat this document as the specification it is.

## Device key rotation (cheap)

**Triggers.**

- User-initiated voluntary rotation (e.g. before international travel).
- Automatic rotation on a schedule (default off; opt-in; cadence values 30 / 90 / 365 days).

**Procedure.**

1. The rotating device generates new `D_S'`, `D_D'` keypairs.
2. It signs a fresh `DeviceCert'` for itself using `ID_S_priv` (still held by this device).
3. It emits a `device_cert` op (control envelope, signed by the **new** `D_S_priv'`).
4. It re-wraps each Stream key currently stored in `stream_keys` for this device under the new `D_D_pub'` (this is local-only; no peer involvement). The `stream_keys` table is created by [ADR-0024](../11-adr/0024-key-hierarchy.md) and is the shape this step assumes; per the banner above, nothing writes or re-wraps it yet.
5. It emits a `device_revoke` op for the **old** key with `effective_at = now + 24h` (the overlap window).

The relay continues to accept signed ops from the old device key until `effective_at`. After that, ops signed by the old key are rejected.

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
- The relay accepts ops from any epoch known to it; epoch is part of the envelope and is not validated against the "current" epoch.
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
2. The trusted device performs a Stream key rotation **for every Stream the revoked device had access to** (all of them, in v1). This produces fresh `key_envelope` ops for sibling devices and, once per-Stream grants exist, `share_grant` ops for peers under the new epoch — but pointedly NOT for the revoked device. Per-Stream grants are not built and sharing is post-v1 ([ADR-0027](../11-adr/0027-v1-self-host-first.md)), so the `share_grant` half has no subject today.
3. Two independent things then happen, and the spec has previously conflated them.
   * **At the relay,** revocation arrives through the account API, never through the op. `DELETE /api/v1/devices/{device_id}` sets `revoked = 1` on the relay's own `devices` row (`crates/sunrise-server/src/store.rs:431`); `Store::active_device` filters on it (`store.rs:400-411`), which refuses every subsequent signed request (`crates/sunrise-server/src/api/signed.rs:166`) and ends a live SSE session with `AUTH_DEVICE_REVOKED` (`crates/sunrise-server/src/api/sync.rs:746-768`). That is relay metadata the relay already holds; it is not a content check.
   * **In the vault,** revocation is enforced by receiving clients. A device applying a remote op verifies the signing `device_id` against the `device_cert` set derived from its own vault-meta log; an op signed by a device with an applied `device_revoke` whose `effective_at` has passed is dropped and counted, never applied. The relay cannot make this check — it does not open envelopes and there is no `device_revoke` op kind in the tree (§Implementation status) — and it forwards such an op unchanged. This is the half that holds against a hostile relay, and it is the half that matters.
4. The revoked device, when next online, sees the revocation in its inbox; the UI explains the situation and the local DB is wiped on first launch.

A revoked device that never reconnects retains whatever plaintext it had at the moment of revocation. We are honest about this in the UI.

## What rotation does not do

- It does not retroactively un-leak content. A revoked device keeps whatever plaintext it had on disk.
- It does not erase the *fact* of past activity from server-side metadata (timestamps, op counts).
- It does not affect historical ops the rotating side already emitted under prior keys; the op log is by design replayable to converge state.
