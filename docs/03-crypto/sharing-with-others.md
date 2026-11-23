---
status: accepted
---

# Sharing with Others

A user shares a **Stream** (and all its descendant entities) with one or more other identities. The Stream is the unit; there is no per-Task ACL.

## Implementation status: the viewer grant's cryptography is built; nothing carries it

The "accept invite" and "view shared stream as editor" rows are MUST in the [parity matrix](../07-clients/parity-matrix.md), not built, and ranked on the roadmap ([`../roadmap.md`](../roadmap.md)) as [#133](https://github.com/justin13888/Sunrise/issues/133), which tracks the whole of sharing.

**Built:** the cryptographic half of a **viewer** grant, in `crates/sunrise-crypto/src/share_grant.rs` with a frozen vector (`sunrise-crypto-test-vectors`, `share_grant`):

* the `ShareGrantPayload` codec below, canonical CBOR, refusing an unknown, repeated, missing or mis-sized field;
* the HPKE Base seal of the Stream key, name and color to the recipient's `ID_D_pub`, and its open;
* the granting identity's `identity_sig`, and its verification;
* the recipient's `expires_at` check (§Share expiration);
* the re-seal a Stream-key rotation emits: a grant of the new epoch to every recipient but the revoked and the expired (§Revoking access, step 2).

The only role it grants is `viewer`, the answer recorded for #133: a read-only grant per Stream, with expiry, revocable by key rotation ([ADR-0020](../11-adr/0020-v1-must-demotions.md) §Addendum). A grant whose role is anything else is refused rather than read as a viewer.

**Not built:**

* `share_grant`, `share_revoke` and `share_decline` as ops. None is among the `InnerOp` variants in `crates/sunrise-core/src/inner_op.rs`, so nothing emits, stores or applies a grant, and no rotation calls the re-seal.
* `crates/sunrise-domain/src/person.rs` defines `struct Person` and `0013_baseline.sql` creates a `persons` table that nothing reads or writes.
* Relay routing of a Stream to a second account: the relay's channel key is `(account_id_hash, stream_id)`, so a Stream belongs to one account by construction.
* The `sunrise://share/<token>` deep link, the accept and decline UI, and the CLI surface.
* The `editor` role.

ADR-0024 decision 4 — independently random per-`(stream_id, epoch)` keys distributed by HPKE — is what makes selective sharing expressible at all, and it is built.

## Roles

| Role | Read | Propose ops | Notes |
|---|---|---|---|
| `viewer` | yes | no (ops emitted are dropped client-side; sync layer rejects them) | Read-only |
| `editor` | yes | yes | Ops carry the editor's device_id and identity ID; merged by entity LWW like any other op |

There is no `admin` and no `commenter` role. Granting and revoking is a privilege of the Stream's **owner identity** only.

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
       5: uint?,             ; expires_at (ms since epoch; absent, not null, when it never expires)
       6: bstr,              ; hpke_ct: HPKE single-shot to recipient ID_D_pub
                             ;   info = "sunrise.hpke.share_grant.v1" || stream_id || u32_be(epoch) || recipient_identity_id
                             ;   aad  = granter_identity_id || canonical_cbor(fields_1_through_5)
                             ;   plaintext = canonical_cbor({1: stream_key (32 B), 2: stream_metadata})
       7: bstr .size 64      ; identity_sig: Ed25519_sign(ID_S_priv,
                             ;     "sunrise.share_grant.v1" || canonical_cbor(fields_1_through_6))
   }
   ```
   `stream_metadata` is `{1: name (tstr), 2: color (tstr)}`, the Stream's name and its color's wire spelling (so the recipient sees a meaningful entry before a full sync). Each is at most 1024 bytes, and an encoded grant at most 8 KiB.

   **Why the granter is in the AAD.** HPKE Base authenticates no sender, and `identity_sig` alone does not stop a third identity re-signing the owner's `hpke_ct` as its own grant: the recipient would then hold the owner's Stream key under the impostor's name. `granter_identity_id` is `identity_id_from_pub` of the `ID_S_pub` the recipient verifies `identity_sig` under, so a ciphertext opens only beside the signature of the identity that sealed it. It is not a field: the recipient derives it from the key it already checked.
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
2. Perform a Stream key rotation (see [`key-rotation.md`](./key-rotation.md)). The rotation re-wraps the new epoch for all sibling devices and remaining peers, **excluding** the revoked recipient. A peer whose grant has expired at the rotating device's clock is excluded too: its grant has ended, and a grant of a new epoch would extend it. Each remaining peer's grant of the new epoch carries its `expires_at` unchanged. `reseal_share_grants` in `sunrise-crypto` is this step's peer half.
3. Nothing is asked of the relay. A revoked recipient stops receiving *readable* content because the epoch has rotated and no envelope is sealed to it under the new key; there is no relay-side grant check and no revocation error frame. The relay keeps forwarding whatever it is given, so a revoked recipient may still receive post-cutoff ciphertext it cannot open, and discards it locally on applying the `share_revoke`.

The revoked recipient retains historical decryption ability for ops that were created under the previous epoch (the same property this document's target state assumes for revoked sibling devices — note that *that* is also target state: revocation as implemented stops nothing, see [`key-rotation.md`](./key-rotation.md) §Revocation). UI states this explicitly: "Y will no longer receive new updates. Y still has the copy of the data they had at revocation time." Already-decrypted local copies persist; revocation is **not** a guarantee of forgetting, only of stopping new data flow.

## Share expiration

`expires_at` (the optional field on `ShareGrantPayload`) is enforced **client-side** by recipients: a recipient with `now >= expires_at` MUST stop applying ops from that grant, and a grant opened at or after its expiry refuses to open (`ShareGrantError::Expired`). Already-applied ops remain in the local vault (same as revoke).

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
3. If multiple recipients have heterogeneous access sets, the owner's device emits one envelope per cohort under the same Stream key. The design uses a uniform "all share-grants on a Stream see the same content" model, so this is an edge case for cross-Stream refs only.

A scrubbed envelope is detectable to the **owner** (they retain the original). Recipients cannot tell whether their envelope was scrubbed; this is by design (no leak of the existence of private references).

There is no editor→owner re-scrubbing path: editors cannot author cross-stream refs in the first place, so there is nothing for the owner to re-scrub on inbound editor ops.

## Cross-relay sharing

Not implemented, and the single answer — the owner's relay is authoritative, there is
no federation, and the credential question is open — lives in
[`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)
§Cross-server delivery.

## Edits by editors

When an editor modifies a shared Stream:

1. Their device produces ops signed by *their* device key, encrypted under the Stream key for the current epoch.
2. Ops are published to the granter's relay (or, in the cross-relay case above, the relay both parties agree to use).
3. Ops carry the editor's `device_id` (whose `DeviceCert` is in the editor's vault-meta log, which is fetched on first share). Authorship is preserved on every op.

There is no "merge request" model: an editor's op applies on arrival and merges by entity-level LWW ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)), which means two editors changing the same Note concurrently keep one version, not a union of both. A future "review mode" toggle is not designed or built.

## Privacy implications

Every party — owner, recipient, relay — sees:

- The fact that two identities share something (visible to the relay).
- Op counts and timestamps in the shared Stream (visible to the relay).
- The granting identity's public-key bundle (the recipient verifies it against an OOB fingerprint).

The design does not hide the sharing graph from the relay. Reducing this leakage is tracked in `11-adr/` as future work (oblivious access patterns).
