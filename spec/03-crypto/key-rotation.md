---
status: accepted
---

# Key Rotation

Three key types rotate, each with a different cost and cascade. Throughout this spec, "the rotating device" is the device the user initiated rotation from; it MUST be a paired, currently-authorized device.

## Device key rotation (cheap)

**Triggers.**

- User-initiated voluntary rotation (e.g. before international travel).
- Automatic rotation on a schedule (default off; opt-in; cadence values 30 / 90 / 365 days).

**Procedure.**

1. The rotating device generates new `D_S'`, `D_D'` keypairs.
2. It signs a fresh `DeviceCert'` for itself using `ID_S_priv` (still held by this device).
3. It emits a `device_cert` op (control envelope, signed by the **new** `D_S_priv'`).
4. It re-wraps each Stream key currently stored in `stream_keys` for this device under the new `D_D_pub'` (this is local-only; no peer involvement).
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
   Both signatures are required so receivers can verify the rotation came from someone holding the old key, and that the new key is a willing successor.
3. It re-issues a fresh `DeviceCert` for **every other still-authorized device** under the new `ID_S_priv'`. (Other devices' own `D_S` and `D_D` are unchanged; only their cert is replaced.) These `device_cert` ops are emitted by the rotating device on behalf of the others. Each peer device, upon seeing both the `identity_transition` and its updated cert, treats itself as still-authorized.
4. It uploads a fresh recovery blob using the new identity keys (the recovery code itself MAY be rotated at the same time; it is the user's choice).
5. It emits a fresh `share_grant` to **every shared peer** for every Stream (the peer's identity DH `ID_D_pub` is unchanged; only the granting identity has new keys, so the share signature changes).
6. It re-publishes `{identity_id, new ID_S_pub, new ID_D_pub}` to the server's identity registry, signed by the new `ID_S_priv'`. The server replaces the public bundle and retains the previous one for `effective_at + 7 days` so peers in flight can still verify recently-emitted ops.

**Stream keys are NOT rotated as a consequence of identity rotation by itself.** Stream keys rotate only when a device or peer is revoked. If the user's reason to rotate identity is "I think someone has my identity key," that someone could only impersonate me going forward (forge ops); they could not read content unless they also held a Stream key. If the user wants both, they perform identity rotation followed by Stream key rotation per Stream, and the UI offers a single "rotate everything" affordance that does it.

Cost on a typical user (10 devices, 30 Streams, 5 sharing peers): O(devices + streams + peers) = ~45 control ops; wall time on a 4G connection a few seconds.

## Revocation

Revocation = removing a device from the identity. Triggered from any other still-authorized device.

**Procedure.**

1. The trusted device emits a `device_revoke` op for the target device, signed by the trusted device.
2. The trusted device performs a Stream key rotation **for every Stream the revoked device had access to** (all of them, in v1). This produces fresh `key_envelope` ops for sibling devices and `share_grant` ops for peers under the new epoch — but pointedly NOT for the revoked device.
3. The server, on seeing the `device_revoke` op (it is a control envelope, the server can read the metadata: `revoked_device_id`), starts rejecting future ops signed by the revoked device.
4. The revoked device, when next online, sees the revocation in its inbox; the UI explains the situation and the local DB is wiped on first launch.

A revoked device that never reconnects retains whatever plaintext it had at the moment of revocation. We are honest about this in the UI.

## What rotation does not do

- It does not retroactively un-leak content. A revoked device keeps whatever plaintext it had on disk.
- It does not erase the *fact* of past activity from server-side metadata (timestamps, op counts).
- It does not affect historical ops the rotating side already emitted under prior keys; CRDT history is by design replayable to converge state.
