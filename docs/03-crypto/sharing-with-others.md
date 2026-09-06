---
status: accepted
---

# Sharing with Others

A user shares a **Stream** (and all its descendant entities) with one or more other identities. The Stream is the unit; there is no per-Task ACL.

## Implementation status: documented only, and demoted from v1

**Nothing in this document is implemented, and it is no longer in the v1 MUST set.** [ADR-0020](../11-adr/0020-v1-must-demotions.md) deferred the "accept invite" and "view shared stream as editor" parity rows for exactly this reason; [ADR-0024](../11-adr/0024-key-hierarchy.md) explains the cryptographic blocker underneath it.

* `share_grant`, `share_revoke` and `share_decline` appear in no Rust or Swift file. They are not among the 21 `InnerOp` variants in `crates/sunrise-core/src/inner_op.rs`.
* `ShareGrantPayload` field 6 is an HPKE single-shot seal. `hpke = "0.13"` is declared in `[workspace.dependencies]` and **no member crate depends on it**; it is not in `Cargo.lock`.
* `crates/sunrise-domain/src/person.rs` defines `struct Person` and `0013_baseline.sql` creates a `persons` table that nothing reads or writes.
* The deeper blocker is the key hierarchy, not the missing ops. Every Stream key today derives from one account-wide vault root, so there is no unit smaller than "the whole vault" to grant. ADR-0024 decision 4 — independently random per-`(stream_id, epoch)` keys distributed by HPKE `key_envelope` ops — is what makes selective sharing expressible at all.

The primitives this spec composes *are* real and frozen: Ed25519 identity signatures, X25519, XChaCha20-Poly1305, `wrap_stream_key`/`unwrap_stream_key`, the `OpEnvelope` codec and `DeviceCert`, all in `sunrise-crypto` with frozen vectors. The sharing-specific layer above them is not.

## Roles

| Role | Read | Propose ops | Notes |
|---|---|---|---|
| `viewer` | yes | no (ops emitted are dropped client-side; sync layer rejects them) | Read-only |
| `editor` | yes | yes | Ops carry the editor's device_id and identity ID; merged by entity LWW like any other op |

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
       3: uint               ; effective_at_ms
   }
   ```
2. Perform a Stream key rotation (see [`key-rotation.md`](./key-rotation.md)). The rotation re-wraps the new epoch for all sibling devices and remaining peers, **excluding** the revoked recipient.
3. Nothing is asked of the relay. A revoked recipient stops receiving *readable* content because the epoch has rotated and no envelope is sealed to it under the new key; there is no relay-side grant check and no revocation error frame. The relay keeps forwarding whatever it is given, so a revoked recipient may still receive post-cutoff ciphertext it cannot open, and discards it locally on applying the `share_revoke`.

The revoked recipient retains historical decryption ability for ops that were created under the previous epoch (the same property this document's target state assumes for revoked sibling devices — note that *that* is also target state: revocation as implemented stops nothing, see [`key-rotation.md`](./key-rotation.md) §Revocation). UI states this explicitly: "Y will no longer receive new updates. Y still has the copy of the data they had at revocation time." Already-decrypted local copies persist; revocation is **not** a guarantee of forgetting, only of stopping new data flow.

## Share expiration

`expires_at` (the optional field on `ShareGrantPayload`) is enforced **client-side** by recipients: a recipient with `now >= expires_at` MUST stop applying ops from that grant. Already-applied ops remain in the local vault (same as revoke).

There is no server-side expiry enforcement, and the guarantee does not need one: a hostile relay that keeps forwarding post-expiry ops changes nothing, because the recipient checks `expires_at` before applying. What the client-side-only gate does not buy is bandwidth — an expired recipient may still be sent ops it will discard.

## What is shared

When sharing a Stream, the recipient gets:

- The Stream entity.
- All Tasks, Routines, Blocks, Notes, Attachments transitively reachable from the Stream.
- Person references **only if** those Persons are explicitly opted-in by the owner per-Person (default: pseudonymous handle).
- **Not** the owner's other Streams, even those mentioned in Notes — references are scrubbed at egress (below).

## Egress scrubbing

A Note in a shared Stream may contain `{kind: "ref", ref: entity-ref}` pointing to a Task or Note in a Stream the recipient does not have. The **owner is the only origin of cross-stream references in shared content**: editors only see entity ids for entities inside the shared Stream, so an editor's UI cannot construct a ref to an entity in one of the owner's private Streams. Cross-stream refs in shared content therefore always come from the owner.

On op emission, the **owner's device** runs egress scrubbing per recipient cohort as part of constructing the op:

1. Walk the Note's outbound payload before encryption.
2. For every `{kind: "ref", ref}` whose `ref` is in a Stream this cohort does not share, replace with `{kind: "redacted", reason: "private_ref", placeholder_text: "—"}` ([`../02-domain/notes.md`](../02-domain/notes.md) §In-app references defines the shape). The original ref is preserved in the owner's local copy of the op (unscrubbed); the scrubbed form is what gets encrypted for this cohort.
3. If multiple recipients have heterogeneous access sets, the owner's device emits one envelope per cohort under the same Stream key. v1 ships with a uniform "all share-grants on a Stream see the same content" model, so this is an edge case for cross-Stream refs only.

A scrubbed envelope is detectable to the **owner** (they retain the original). Recipients cannot tell whether their envelope was scrubbed; this is by design (no leak of the existence of private references).

There is no editor→owner re-scrubbing path: editors cannot author cross-stream refs in the first place, so there is nothing for the owner to re-scrub on inbound editor ops.

## Cross-relay sharing

Not in v1, and the single answer — the owner's relay is authoritative, there is
no federation, and the credential question is open — lives in
[`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)
§Cross-server delivery.

## Edits by editors

When an editor modifies a shared Stream:

1. Their device produces ops signed by *their* device key, encrypted under the Stream key for the current epoch.
2. Ops are published to the granter's relay (or, in the cross-relay case above, the relay both parties agree to use).
3. Ops carry the editor's `device_id` (whose `DeviceCert` is in the editor's vault-meta log, which is fetched on first share). Authorship is preserved on every op.

There is no "merge request" model: an editor's op applies on arrival and merges by entity-level LWW ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)), which means two editors changing the same Note concurrently keep one version, not a union of both. A future "review mode" toggle is tracked as v2.

## Privacy implications

Every party — owner, recipient, relay — sees:

- The fact that two identities share something (visible to the relay).
- Op counts and timestamps in the shared Stream (visible to the relay).
- The granting identity's public-key bundle (the recipient verifies it against an OOB fingerprint).

We do not hide the sharing graph from the relay in v1. Reducing this leakage is tracked in `11-adr/` as future work (oblivious access patterns).
