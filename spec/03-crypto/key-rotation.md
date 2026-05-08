---
status: draft
---

# Key Rotation

Three keys can rotate: device, stream, identity. Each has a different cost.

## Device key rotation (cheap)

**Triggers.**
- User rotates a device's key voluntarily (e.g. before international travel).
- Periodic auto-rotation (default off; opt-in for high-paranoia users).

**Procedure.**
1. Device generates new `D_S` and `D_D`.
2. Issues a new `DeviceCert` signed by the still-valid identity key on this device.
3. Publishes a control op announcing the new keys; tag points old key as superseded.
4. Old per-device wrapped stream keys are re-wrapped under the new device key (handled by the same device, no peer involvement).

The relay continues to accept signed ops from the old device key for a short overlap window (default 24h) so in-flight ops aren't lost.

## Stream key rotation (medium)

**Triggers.**
- A device with access to the Stream is revoked.
- A shared peer's access is revoked.
- User-initiated periodic rotation.

**Procedure.**
1. Device with current access generates new `StreamKey'`.
2. Re-wraps `StreamKey'` for every still-authorized device and shared peer in the form of a **key-envelope** op published in the Stream's op log.
3. From a chosen rotation point (op N+1), all *new* ops in this Stream are encrypted under `StreamKey'`.
4. The old `StreamKey` is retained on still-authorized devices to decrypt historical ops.
5. Revoked parties cannot decrypt ops from N+1 onward.

A rotation does **not** re-encrypt historical ops. Historical ops are still readable by anyone who held the old key when those ops were created — that's an inherent limit of E2EE; we don't lie about it. If forward secrecy of historical content is needed, the user must export, delete, and re-create.

## Identity rotation (expensive)

**Triggers.**
- Suspected recovery code compromise.
- Suspected identity-key extraction (rare; requires escape from OS keystore on the user's device).

**Procedure.**
1. User initiates from a trusted device.
2. Device generates new `ID_S'`, `ID_D'`.
3. Publishes an "identity rotation" control op signed by *both* the old and new identity keys (a "transition certificate").
4. Re-issues `DeviceCert` for every still-authorized device under the new identity key.
5. Rotates every Stream key (cascading from the identity change).
6. Notifies all sharing peers; their devices verify the transition cert and re-bind the Person ↔ identity link.
7. Old identity is retired in the op log; future ops signed by the old identity are rejected.

Cost: O(devices + streams + peers) re-key operations. Approximate wall time on a typical user's data: a few seconds. Approximate sync impact: a moderate batch of control ops. UX should be a single-click "rotate identity" with a progress indicator.

## Revocation

Revocation = "remove this device from the identity." It is a **device** rotation triggered from any other still-authorized device:

1. Trusted device publishes a revocation op for the target device.
2. Trusted device rotates every Stream key (so future ops aren't readable by the revoked device's old keys).
3. Other devices apply the rotation and continue.
4. The server stops accepting ops signed by the revoked device.
5. The revoked device, when next online, sees the revocation in its inbox; UI tells the user, and the device wipes its local data.

## What rotation does *not* do

- It does not retroactively un-leak content. If a revoked device was holding plaintext on disk, that plaintext is in their hands.
- It does not magically forget the *fact* of past activity from sync metadata. The server still has timestamps and counts.
