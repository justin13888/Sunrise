---
status: accepted
---

# Sharing with Others

A user shares a **Stream** (and all its descendant entities) with one or more other identities. The Stream is the unit; there is no per-Task ACL.

## Roles

| Role | Read | Propose ops | Notes |
|---|---|---|---|
| `viewer` | yes | no (ops emitted are dropped client-side; sync layer rejects them) | Read-only |
| `editor` | yes | yes | Ops carry the editor's device_id and identity ID; CRDT merges directly |

There is no `admin` and no `commenter` in v1. Granting and revoking is a privilege of the Stream's **owner identity** only.

## Granting access

1. **Owner finds recipient identity.** Either by handle (server lookup of `idn_…` by user-handle/email) or by an identity public-key fingerprint shared OOB.
2. **Owner verifies fingerprint.** UI strongly encourages OOB verification (QR scan, voice, in-person) for non-trivial shares. The `verified_at` annotation is stored on the Person record and is shown in the share confirmation.
3. **Owner emits a `share_grant` control op:**
   ```cddl
   ShareGrantPayload = {
       1: bstr .size 16,     ; stream_id
       2: uint,              ; epoch (current Stream-key epoch)
       3: bstr .size 16,     ; recipient_identity_id_bytes
       4: tstr,              ; role: "viewer" / "editor"
       5: uint?,             ; expires_at (ms since epoch; optional)
       6: bstr,              ; hpke_ct: HPKE single-shot to recipient ID_D_pub
                             ;   info = "sunrise.hpke.share_grant.v1" || stream_id || u32_be(epoch) || recipient_identity_id
                             ;   plaintext = canonical_cbor({1: stream_key (32 B), 2: stream_metadata})
       7: bstr .size 64      ; identity_sig: Ed25519_sign(ID_S_priv,
                             ;     "sunrise.share_grant.v1" || canonical_cbor(fields_1_through_6))
   }
   ```
   `stream_metadata` carries the Stream's name and color (so the recipient sees a meaningful entry before a full sync).
4. **Owner publishes the `share_grant` op** to the relay. The relay routes it to the recipient's account.
5. **Recipient receives** the envelope on next sync. UI shows "Stream X shared with you by Y. Accept?" The recipient's client verifies `identity_sig` against the granting identity's published `ID_S_pub`.
6. **On accept:** recipient's device opens the HPKE ciphertext using its `ID_D_priv`, stores `stream_key_<epoch>` locally (wrapped under its own `vault_root`), subscribes to the Stream on the relay, decrypts ops, renders.
7. **On decline:** recipient emits a `share_decline` control op (signed control envelope to the granting identity); the owner's UI shows the result.

## Revoking access

Owner triggers revoke. This is implemented as:

1. Emit a `share_revoke` control op:
   ```cddl
   ShareRevokePayload = {
       1: bstr .size 16,     ; stream_id
       2: bstr .size 16,     ; recipient_identity_id_bytes
       3: uint               ; effective_at
   }
   ```
2. Perform a Stream key rotation (see [`key-rotation.md`](./key-rotation.md)). The rotation re-wraps the new epoch for all sibling devices and remaining peers, **excluding** the revoked recipient.
3. The relay, on seeing `share_revoke`, stops forwarding the Stream's ops to the revoked recipient's devices.

The revoked recipient retains historical decryption ability for ops that were created under the previous epoch (this is the same property as for revoked sibling devices). UI states this explicitly: "Y will no longer receive new updates. Y still has the copy of the data they had at revocation time."

## What is shared

When sharing a Stream, the recipient gets:

- The Stream entity.
- All Tasks, Routines, Blocks, Notes, Attachments transitively reachable from the Stream.
- Person references **only if** those Persons are explicitly opted-in by the owner per-Person (default: pseudonymous handle).
- **Not** the owner's other Streams, even those mentioned in Notes — references are scrubbed at egress (below).

## Egress scrubbing

A Note in a shared Stream may contain `{kind: "ref", target: EntityRef}` pointing to a Task or Note in a Stream the recipient does not have. On op emission, the **owner's device** runs egress scrubbing as part of constructing the op:

1. Walk the Note CRDT's outbound payload before encryption.
2. For every `{kind: "ref", target}` whose `target` is in a Stream the current recipient set does not all share, replace with `{kind: "redacted", placeholder: "—"}`. The original ref is preserved in the owner's local copy of the op (unscrubbed); the scrubbed form is what gets encrypted under the shared Stream's key.
3. If multiple recipients have heterogeneous access sets, the owner's device emits one envelope per recipient cohort under the same Stream key (this is unusual; v1 ships with a uniform "all share-grants on a Stream see the same content" model, so this is an edge case for cross-Stream refs only).

Editors do not run egress scrubbing — an editor by definition has access to the Stream's content but no special privilege over other Streams; their Notes naturally contain refs only to entities they themselves can see. If an editor (somehow) authored a ref to a Stream they do not own, the **owner's device** re-scrubs on receipt before forwarding to other recipients. The CRDT layer treats the scrubbed form as the canonical merged value.

## Cross-relay sharing

Both parties' devices SHOULD reach the same relay for v1. If they reach different relays:

- **v1 path:** the granting client uploads the `share_grant` op to its own relay; the recipient must connect to that relay (with credentials provided OOB by the granter, e.g. a self-host operator's invite token) to fetch ongoing Stream ops. There is no automatic federation between relays in v1.
- **Future:** a federated forwarding handshake between cooperating relays. Tracked in `11-adr/` as a v2 candidate; not part of v1.

## Edits by editors

When an editor modifies a shared Stream:

1. Their device produces ops signed by *their* device key, encrypted under the Stream key for the current epoch.
2. Ops are published to the granter's relay (or, in the cross-relay case above, the relay both parties agree to use).
3. Ops carry the editor's `device_id` (whose `DeviceCert` is in the editor's vault-meta log, which is fetched on first share). Authorship is preserved on every op.

There is no "merge request" model. The CRDT merges directly. A future "review mode" toggle is tracked as v2.

## Privacy implications

Every party — owner, recipient, relay — sees:

- The fact that two identities share something (visible to the relay).
- Op counts and timestamps in the shared Stream (visible to the relay).
- The granting identity's public-key bundle (the recipient verifies it against an OOB fingerprint).

We do not hide the sharing graph from the relay in v1. Reducing this leakage is tracked in `11-adr/` as future work (oblivious access patterns).
