---
status: accepted
---

# Sharing with Others

A user shares a **Stream** (and all its descendant entities) with one or more other identities. The Stream is the unit; there is no per-Task ACL.

## Implementation status: the viewer grant, its op, its re-seal and its relay route are built; no client uses them

The "accept invite" and "view shared stream as editor" rows are MUST in the [parity matrix](../07-clients/parity-matrix.md), not built, and ranked on the roadmap ([`../roadmap.md`](../roadmap.md)) as [#133](https://github.com/justin13888/Sunrise/issues/133), which tracks the whole of sharing.

**Built:** the cryptographic half of a **viewer** grant, in `crates/sunrise-crypto/src/share_grant.rs` with a frozen vector (`sunrise-crypto-test-vectors`, `share_grant`):

* the `ShareGrantPayload` codec below, canonical CBOR, refusing an unknown, repeated, missing or mis-sized field;
* the HPKE Base seal of the Stream key, name and color to the recipient's `ID_D_pub`, and its open;
* the granting identity's `identity_sig`, and its verification;
* the recipient's `expires_at` check (§Share expiration);
* the re-seal a Stream-key rotation emits: a grant of the new epoch to every recipient but the revoked and the expired (§Revoking access, step 2).

The only role it grants is `viewer`, the answer recorded for #133: a read-only grant per Stream, with expiry, revocable by key rotation ([ADR-0020](../11-adr/0020-v1-must-demotions.md) §Addendum). A grant whose role is anything else is refused rather than read as a viewer.

**Built** ([#1318](https://github.com/justin13888/Sunrise/issues/1318), `crates/sunrise-core/src/engine/share_grant.rs`): the grant as an op, at `DOC_SCHEMA_V = 32`.

* `Engine::share_stream` seals the Stream's live epoch, name and color to a `Person`'s identity, signed by the account identity in force, and logs it as a `ShareGrant` control op on the vault-meta stream ([`data-encryption-format.md`](./data-encryption-format.md) §Control op payloads). Only a device holding `ID_S_priv` can share. It returns the encoded grant, which is what reaches the recipient.
* Applying the op stores nothing beside it. `Engine::stream_shares` folds the op log into one share per recipient: the latest grant, by envelope stamp, that names a Person, decodes as a viewer grant, verifies under an identity on the account's chain, and comes from a sender whose reads are not bounded. The bound holds whoever signed the grant, the head included: the grant's signature does not cover the op's `recipient_id_d_pub`, so an evicted device could otherwise re-file a grant the account signed under its own key and receive every later re-seal. A share whose only grants a now-bounded device filed lapses until the owner shares it again.
* Every Stream-key rotation re-seals its new epoch to the Stream's shares through `reseal_share_grants`, beside the account's own key envelopes (§Revoking access, step 2).

**Not built:**

* `share_revoke` and `share_decline` as ops, so a re-seal leaves out only the expired, and nothing revokes a share.
* A re-seal by a device without `ID_S_priv`. Such a device's rotation reaches no share recipient, and says so (`core.share.reseal_skipped`).
* The `persons` table `0013_baseline.sql` creates is still read and written by nothing: a share names its `Person` by ref, and the caller supplies the `Person` and the recipient's `ID_D_pub`.
* Any client use of the relay viewer grant. The relay half is built ([ADR-0074](../11-adr/0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md), the threat model for all of sharing): an owner registers a grant on a Stream under a claim token, one other account claims it, and that account's sessions read the owner's channel and can write nothing into it. The same grant reads the owner's pinned snapshot and blobs, and a publish wakes the accounts holding one ([#1322](https://github.com/justin13888/Sunrise/issues/1322)). No client registers, claims, subscribes or reads through one yet.
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

   The op that carries it is the account's own record, sealed on the vault-meta stream like every control op: `{ "ShareGrant": { "person", "recipient_id_d_pub", "grant" } }`, where `grant` is the payload above in its canonical encoding, `person` the `per_` ref of the Person shared with, and `recipient_id_d_pub` the key every later re-seal is sealed to, which the grant itself does not carry ([`data-encryption-format.md`](./data-encryption-format.md) §Control op payloads). The recipient never reads this op; it receives the encoded grant in step 4.

   **Why the granter is in the AAD.** HPKE Base authenticates no sender, and `identity_sig` alone does not stop a third identity re-signing the owner's `hpke_ct` as its own grant: the recipient would then hold the owner's Stream key under the impostor's name. `granter_identity_id` is `identity_id_from_pub` of the `ID_S_pub` the recipient verifies `identity_sig` under, so a ciphertext opens only beside the signature of the identity that sealed it. It is not a field: the recipient derives it from the key it already checked.
4. **Owner publishes the `share_grant` op and registers a relay viewer grant** ([ADR-0074](../11-adr/0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md)): `POST /api/v1/sync/grants` on the Stream, under a claim token of 32 random bytes and the grant's `expires_at`. The recipient receives the claim token beside the sealed grant, claims it from its own account, and from then on reads the owner's channel by naming the grant in its subscription. The relay learns which account reads the Stream; it never learns the recipient's identity, and never holds the key.
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
2. Perform a Stream key rotation (see [`key-rotation.md`](./key-rotation.md)). The rotation re-wraps the new epoch for all sibling devices and remaining peers, **excluding** the revoked recipient. A peer whose grant has expired at the rotating device's clock is excluded too: its grant has ended, and a grant of a new epoch would extend it. Each remaining peer's grant of the new epoch carries its `expires_at` unchanged. `reseal_share_grants` in `sunrise-crypto` is this step's peer half, and every rotation calls it: `emit_key_envelopes`, which each rotation distributes its epoch through, re-seals the epoch to the Stream's shares as `ShareGrant` ops, sealed under the same vault-meta epoch as the key envelopes. Until `share_revoke` exists, the revoked list it passes is empty.
3. Withdraw the relay viewer grant (`DELETE /api/v1/sync/grants/{grant_id}`, [ADR-0074](../11-adr/0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md)): the relay refuses the recipient's next subscribe and ends a stream it holds open within one re-check, with `RELAY_GRANT_REVOKED`. That saves the owner's metadata and the relay's bandwidth, and it is **not** the guarantee. A revoked recipient stops receiving *readable* content because the epoch has rotated and no envelope is sealed to it under the new key. A relay that ignores the withdrawal keeps forwarding, so a revoked recipient may still receive post-cutoff ciphertext it cannot open, and discards it locally on applying the `share_revoke`.

The revoked recipient retains historical decryption ability for ops that were created under the previous epoch (the same property this document's target state assumes for revoked sibling devices — note that *that* is also target state: revocation as implemented stops nothing, see [`key-rotation.md`](./key-rotation.md) §Revocation). UI states this explicitly: "Y will no longer receive new updates. Y still has the copy of the data they had at revocation time." Already-decrypted local copies persist; revocation is **not** a guarantee of forgetting, only of stopping new data flow.

## Share expiration

`expires_at` (the optional field on `ShareGrantPayload`) is enforced **client-side** by recipients: a recipient with `now >= expires_at` MUST stop applying ops from that grant, and a grant opened at or after its expiry refuses to open (`ShareGrantError::Expired`). Already-applied ops remain in the local vault (same as revoke).

The guarantee needs no server-side enforcement: a hostile relay that keeps forwarding post-expiry ops changes nothing, because the recipient checks `expires_at` before applying. An honest relay also stops serving the grant at the expiry the owner registered it with ([ADR-0074](../11-adr/0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md) §4), which buys bandwidth and the owner's metadata. The relay cannot read field 5, so the owner registers the same value; if the two ever disagree, the cost is bandwidth, never content.

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
