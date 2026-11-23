---
status: accepted
---

# Sharing with Others

A user shares a **Stream** (and all its descendant entities) with one or more other identities. The Stream is the unit; there is no per-Task ACL.

## Implementation status: the viewer grant, its op, its re-seal, its relay route and its link are built; the CLI shares, accepts, lists, revokes and declines; a recipient can verify and key a shared Stream from the Stream itself, and no client applies or syncs one

The "accept invite" and "view shared stream as editor" rows are MUST in the [parity matrix](../07-clients/parity-matrix.md), not built, and ranked on the roadmap ([`../roadmap.md`](../roadmap.md)) as [#133](https://github.com/justin13888/Sunrise/issues/133), which tracks the whole of sharing.

**Built:** the cryptographic half of a **viewer** grant, in `crates/sunrise-crypto/src/share_grant.rs` with a frozen vector (`sunrise-crypto-test-vectors`, `share_grant`):

* the `ShareGrantPayload` codec below, canonical CBOR, refusing an unknown, repeated, missing or mis-sized field;
* the HPKE Base seal of the Stream key, name and color to the recipient's `ID_D_pub`, and its open;
* the granting identity's `identity_sig`, over the grant and the `ID_D_pub` it is sealed to, and its verification;
* the recipient's `expires_at` check (§Share expiration);
* the re-seal a Stream-key rotation emits: a grant of the new epoch to every recipient but the revoked and the expired (§Revoking access, step 2).

The only role it grants is `viewer`, the answer recorded for #133: a read-only grant per Stream, with expiry, revocable by key rotation ([ADR-0020](../11-adr/0020-v1-must-demotions.md) §Addendum). A grant whose role is anything else is refused rather than read as a viewer.

**Built** ([#1318](https://github.com/justin13888/Sunrise/issues/1318), `crates/sunrise-core/src/engine/share_grant.rs`): the grant as an op, at `DOC_SCHEMA_V = 32`.

* `Engine::share_stream` seals the Stream's live epoch, name and color to a `Person`'s identity, signed by the account identity in force, and logs it as a `ShareGrant` control op on the vault-meta stream ([`data-encryption-format.md`](./data-encryption-format.md) §Control op payloads). Only a device holding `ID_S_priv` can share. It returns the encoded grant, which is what reaches the recipient.
* Applying the op stores nothing beside it. `Engine::stream_shares` folds the op log into one share per recipient: the latest grant, by envelope stamp, that names a Person, decodes as a viewer grant, verifies under an identity on the account's chain for the op's `recipient_id_d_pub`, and comes from a sender whose reads are not bounded. The signature covers the key the grant is sealed to, so a device that re-files a grant the account signed beside its own key files one that verifies under nobody, and no re-seal ever carries that key forward. The bound on the sender holds whoever signed the grant, the head included: an evicted device may hold a retired identity's `ID_S_priv`, and can replay an older grant of the head's under a later stamp. A share whose only grants a now-bounded device filed lapses until the owner shares it again.
* Every Stream-key rotation re-seals its new epoch to the Stream's shares through `reseal_share_grants`, beside the account's own key envelopes (§Revoking access, step 2). A re-seal's op carries `reseal: true`.

**Built** ([#1325](https://github.com/justin13888/Sunrise/issues/1325), the same module): ending a share, and re-sealing an epoch a device without `ID_S_priv` minted, at `DOC_SCHEMA_V = 33`.

* `Engine::revoke_share` logs a `ShareRevoke` control op and rotates the Stream key in the same transaction (§Revoking access). `Engine::record_share_decline` logs the account's record of a recipient's decline as a `ShareDecline` control op, with no rotation (§Granting access, step 7). Both refuse, writing nothing, a recipient with no share in force. Any account device whose reads are not bounded may end a share; neither op carries an identity signature, because only the account's own devices write the vault-meta stream, and ending a share gives nobody anything.
* The fold orders each recipient's believed grants and ends by envelope stamp. The latest fresh grant or end is the cut. Where the cut is an end, the share has ended, and a re-seal stamped after it does not revive it: a sibling that rotated before it saw the revocation re-sealed to a share it did not know had ended. Otherwise the share is the latest grant stamped at or after the cut. Sharing again after an end is a fresh grant, which takes the cut. `Engine::ended_stream_shares` lists the ended shares and whether each was revoked or declined. A rotation hands the ended recipients to `reseal_share_grants` as its revoked list.
* A rotation by a device without `ID_S_priv` cannot sign a re-seal. It logs a `ShareResealRequest` control op instead, `{ "ShareResealRequest": { "stream_id", "epoch", "key_id", "recipients": [ { "identity_id" } ] } }`, naming the shares its own fold holds in force: the shares its re-seal would have reached. A device holding `ID_S_priv` re-seals each requested epoch whose key it holds to each share the request names whose epoch is below it, less any share its own fold has ended (`Engine::backfill_share_grants`). It does so on whichever of the three it needs arrives last: the key (from an envelope or a key bundle), the share's grant, or the request. So an epoch a paired device minted reaches the Stream's recipients once a holder sees it.
* The holder re-seals only to the shares the request names, never to one its own fold alone holds in force. A revocation logs its end and rotates in one transaction, so a holder can learn the new epoch, from a key bundle or with the end op parked or lost, before it applies the end; sealing the epoch to every share its fold held would hand the new key to the recipient the rotation was for, and no later op takes a sealed key back. A share the minter had not seen when it rotated, such as one shared concurrently, receives that epoch only at the next rotation by a holder.
* A rotation by a holder re-seals its own epoch and logs no request, so no other holder re-seals it. A backfill writes one `ShareGrant` per named share per holder that sees the request before it sees another holder's re-seal, and only for an epoch a device without `ID_S_priv` minted.

A build at `DOC_SCHEMA_V` 32 parks the three new ops, so its own rotation still re-seals to a share they ended.

**Built** ([#1320](https://github.com/justin13888/Sunrise/issues/1320), `crates/sunrise-client-core/src/links.rs`): the `sunrise://share/<token>` link that carries a grant to its recipient (step 4). `ShareLink` builds and parses the token, and `DeepLink::Share` is the link ([`../07-clients/interaction-patterns.md`](../07-clients/interaction-patterns.md) §`sunrise://share/<token>`).

**Built** ([#1333](https://github.com/justin13888/Sunrise/issues/1333)): the client half of steps 1, 4 and 5, on the CLI.

* `sunrise_relay_client::grants` calls the four relay grant routes: `create_viewer_grant`, `claim_viewer_grant`, `list_viewer_grants` and `withdraw_viewer_grant`, each signed with the device binding. A claim the relay will not honour, for whatever reason, is `GrantError::NotHeld`.
* `Core::share_stream` takes the recipient's `ID_S_pub` and `ID_D_pub` and returns the encoded grant. The Person the op names is derived from the recipient's identity, `per_` over its 16 identity-id bytes, so sharing with one identity twice names one Person.
* `Core::open_share_grant` is step 5's check: it opens a grant with this account's `ID_D_priv`, so only on the device that created the account, and refuses one to another identity, one the link's `granter_id_s_pub` did not sign over this account's `ID_D_pub`, and one past its expiry. It writes nothing and drops the key it opened.
* Step 1 by key rather than by handle: the recipient's `sunrise share key` prints base64url of its `ID_S_pub || ID_D_pub`, which the owner passes to `sunrise share <stream> <share-key> [--days <n>]`. That writes the grant, registers the relay grant under a fresh claim token with the same expiry, prints the link, and pushes the `share_grant` op to the relay before it exits; where the push does not finish, it says the link stands and `sunrise sync` publishes the op. `sunrise accept <link>` opens the grant, shows the Stream's name and the granter's identity id, and only then claims the relay grant. A claimed relay grant on another Stream than the link's is withdrawn again before `accept` refuses it, and where the relay will not withdraw it, `accept` says the account still holds it. Each side compares the other's identity id out of band. The identity fingerprint of [`identity-and-device-keys.md`](./identity-and-device-keys.md) §Public-key publication covers `ID_D_pub` as well, which the link does not carry, so it is not what either side shows.

**Built** ([#1334](https://github.com/justin13888/Sunrise/issues/1334), `crates/sunrise-core/src/core/share.rs`): ending a share and listing a Stream's shares at the core's seam. `Core::revoke_share` runs `Engine::revoke_share` (the `ShareRevoke` op and the rotation, returning the minted epoch), `Core::record_share_decline` runs `Engine::record_share_decline`, and `Core::stream_shares` and `Core::ended_stream_shares` list the shares in force and those ended, as revoked or declined.

**Built** ([#1335](https://github.com/justin13888/Sunrise/issues/1335), migration 0056): what a share leaves on a device that no op carries, and the CLI's list, revoke and decline.

* `Core::accept_share` is step 6 up to the subscription: it opens the grant as `open_share_grant` does and holds `stream_key_<epoch>`, wrapped under this account's `vault_root`, in `held_share_keys`, keyed by the granter's identity id, the owner's Stream id and the epoch, beside the claimed relay grant's id, the sealed name and color, and the expiry. The table is not `stream_keys`: that is the account's own key set, every Stream id in it is rotated and its writable epochs published in the key bundle, and its cache is keyed by `(stream_id, epoch)` alone, so an owner that named one of the recipient's own Stream ids would otherwise put a key it knows among the keys the recipient seals its own ops under. Nothing that rotates, publishes or seals reads `held_share_keys`. `Core::held_shares` lists what is held, and `Core::drop_held_share` drops it.
* The owner's device records the id of each relay viewer grant it registers for a share in `share_relay_grants` (`Core::record_share_relay_grant`), and forgets it once the relay lets it go. The record is the registering device's alone: no op carries it, and another device that revokes the share rotates the key, which is the guarantee, with no grant to withdraw.
* `sunrise share` records the registered grant's id; `sunrise accept` holds the key after it claims. `sunrise share list <stream>` lists the shares in force and those ended. `sunrise share revoke <stream> <identity>` revokes and rotates, then withdraws every relay grant the device recorded for the share (step 3), saying so where it recorded none; a relay that refuses or cannot be reached leaves the share revoked, and a rerun withdraws what is left. `sunrise share declined <stream> <identity>` is the owner's record of a decline (step 7). The recipient's `sunrise share decline <stream>` withdraws the relay grant it claimed and only then drops the held key; it writes no op, and tells the person to let the owner know out of band.

**Built** ([#1336](https://github.com/justin13888/Sunrise/issues/1336), [ADR-0076](../11-adr/0076-a-shared-stream-carries-its-writer-certs-and-re-sealed-grants.md), `DOC_SCHEMA_V = 34`): what a recipient needs on the owner's channel to read it.

* The owner's devices log `StreamWriterCert` control ops in the shared Stream's own log (`crates/sunrise-core/src/engine/shared_stream.rs`): each device's cert beside the account's identity chain record, per `(Stream, epoch, device)`. A write into a Stream with a share in force logs its writer's cert under the write's epoch first; sharing logs, under the live epoch, the cert of every device with an op in the Stream; a device that applies a share of a Stream it wrote into logs its own.
* Every re-seal `ShareGrant` is copied into the Stream as a `StreamShareGrant`, sealed under every key of the share's previous epoch, after the emitter's cert under it. The account's own replicas store nothing from either op.
* `Core::read_shared_envelope` reads one envelope as the recipient, through a `SharedStreamReader` built from the link's `granter_id_s_pub` (`crates/sunrise-core/src/core/shared_read.rs`). It believes a writer cert whose chain holds the granter's identity, verifies every other op under a believed cert or reports it waiting, holds the epoch a grant copy to this account seals beside the others under the same granter, and reads nothing once the share has expired on this account's clock.

**Not built:**

* A re-seal while no device holding `ID_S_priv` is online. Until one learns the epoch, a paired device's rotation reaches no share recipient, and says so (`core.share.reseal_skipped`).
* The recipient's side of a decline: the signed message that carries it to the owner, and the recipient discarding post-cutoff ops on applying a revocation.
* The `persons` table `0013_baseline.sql` creates is still read and written by nothing: a share names its `Person` by ref, derived from the recipient's identity, and no row is written for it.
* Applying and syncing a shared Stream ([#1337](https://github.com/justin13888/Sunrise/issues/1337)): the recipient's replica (ADR-0076 §3) and its storage, persisting the granter's `ID_S_pub` and what the reader believes, materializing what it reads, the sync driver's subscription through the grant, snapshot and blob reads with `?grant=`, and stopping on `RELAY_GRANT_REVOKED`. The relay half is built ([ADR-0074](../11-adr/0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md), the threat model for all of sharing): an owner registers a grant on a Stream under a claim token, one other account claims it, and that account's sessions read the owner's channel and can write nothing into it. The same grant reads the owner's pinned snapshot and blobs, and a publish wakes the accounts holding one ([#1322](https://github.com/justin13888/Sunrise/issues/1322)). The CLI registers and claims one, and holds the Stream key an accepted grant seals; no client subscribes through the grant or applies the owner's ops, so an accepted Stream appears in no list.
* The Apple route for the `sunrise://share/<token>` link and its accept sheet; the Apple parser refuses the link ([#1338](https://github.com/justin13888/Sunrise/issues/1338)).
* Revoking, declining and listing shares from a client other than the CLI, and holding an accepted share's key anywhere but the CLI (above).
* The `editor` role. It is decided ([ADR-0075](../11-adr/0075-an-editor-writes-a-shared-streams-contents-and-every-replica-is-the-boundary.md), [#1321](https://github.com/justin13888/Sunrise/issues/1321)) and none of it is built. Every build refuses an `editor` grant until all of that ADR's §8 is built (§Roles).

ADR-0024 decision 4 — independently random per-`(stream_id, epoch)` keys distributed by HPKE — is what makes selective sharing expressible at all, and it is built.

## Roles

| Role | Read | Propose ops | Notes |
|---|---|---|---|
| `viewer` | yes | no (ops emitted are dropped client-side; sync layer rejects them) | Read-only |
| `editor` | yes | entity ops in the shared Stream only | Decided, not built ([ADR-0075](../11-adr/0075-an-editor-writes-a-shared-streams-contents-and-every-replica-is-the-boundary.md)). Ops carry the editor's `device_id`, chain to its identity, and merge by per-field ops like the owner's ([ADR-0044](../11-adr/0044-per-field-ops.md)) |

There is no `admin` and no `commenter` role. Granting and revoking is a privilege of the Stream's **owner identity** only.

**What an editor may write** (ADR-0075 §1–§2): entity ops in the shared Stream's own log, under an epoch its grant sealed, on the entities below the Stream (its Tasks, Blocks, Notes and Attachments). An editor never writes:

* a control op of any family, in any stream;
* the Stream entity itself, meaning its name, color, pause, archive, deletion or sharing;
* any other stream;
* an op that moves an entity out of the shared Stream or names an entity outside it.

An editor is never a device of the owner's account.

**Who enforces it** (ADR-0075 §3): every replica that applies the Stream, on its own. That means the owner's devices, each viewer and each editor. The relay does not enforce it, and neither does the owner's vault-meta log, which recipients cannot read. An editor's op applies only when all four hold:

1. Its signing device's `DeviceCert` was signed by an identity whose `identity_id_from_pub` is an editor grant's `recipient_identity_id`.
2. The owner identity has attested that grant inside the shared Stream.
3. It is under an epoch the editor was sealed and inside any revocation cut.
4. Its content stays inside the editor's authority.

An op that fails any of them, though its signature verifies, is parked ([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §4), not dropped and not a sync failure: it counts as received and nothing of it is materialized. Whether it applies is a function of the Stream's retained log, never of delivery order. An attestation, a cut or a writer cert landing in the Stream retries every op parked for want of one, and a cut that lands after ops past it were applied re-derives the Stream without them, applying whichever half of a fork the cut names (ADR-0075 §3, §4, §9). An editor's authority ends at a frontier cut its `share_revoke` carries, never at a time compared with the editor's own HLC (ADR-0075 §4).

**What stays enforced until then.** `share_grant.rs` decodes `editor` as `ShareGrantError::UnsupportedRole`, and nothing seals one. The ADR's §8 lists what must all be built before that changes, behind a structural feature.

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
                             ;     "sunrise.share_grant.v1" || canonical_cbor(fields_1_through_6)
                             ;     || recipient_id_d_pub)
   }
   ```
   `recipient_id_d_pub` is the 32-byte `ID_D_pub` `hpke_ct` is sealed to. It is under the signature and not on the wire: HPKE Base shows nobody but the recipient which key a ciphertext was sealed to, and the owner's account re-seals every later epoch to the key its grant names, so an unsigned key would let any device that writes the account's log redirect every later epoch to itself. The recipient verifies with its own `ID_D_pub`.
   `stream_metadata` is `{1: name (tstr), 2: color (tstr)}`, the Stream's name and its color's wire spelling (so the recipient sees a meaningful entry before a full sync). Each is at most 1024 bytes, and an encoded grant at most 8 KiB.

   The op that carries it is the account's own record, sealed on the vault-meta stream like every control op: `{ "ShareGrant": { "person", "recipient_id_d_pub", "grant" } }`, where `grant` is the payload above in its canonical encoding, `person` the `per_` ref of the Person shared with, and `recipient_id_d_pub` the key every later re-seal is sealed to, which the grant itself does not carry but its signature covers ([`data-encryption-format.md`](./data-encryption-format.md) §Control op payloads). The recipient never reads this op; it receives the encoded grant in step 4, and each later epoch's re-seal as a copy logged in the shared Stream itself, a `StreamShareGrant` sealed under the epoch it replaces ([ADR-0076](../11-adr/0076-a-shared-stream-carries-its-writer-certs-and-re-sealed-grants.md) §2). The owner's devices log their own `DeviceCert`s there too, as `StreamWriterCert`s, so the recipient can verify the owner's ops (§1 of the same record).

   **Why the granter is in the AAD.** HPKE Base authenticates no sender, and `identity_sig` alone does not stop a third identity re-signing the owner's `hpke_ct` as its own grant: the recipient would then hold the owner's Stream key under the impostor's name. `granter_identity_id` is `identity_id_from_pub` of the `ID_S_pub` the recipient verifies `identity_sig` under, so a ciphertext opens only beside the signature of the identity that sealed it. It is not a field: the recipient derives it from the key it already checked.
4. **Owner publishes the `share_grant` op and registers a relay viewer grant** ([ADR-0074](../11-adr/0074-a-viewer-reads-a-shared-stream-through-a-relay-grant.md)): `POST /api/v1/sync/grants` on the Stream, under a claim token of 32 random bytes and the grant's `expires_at`. The recipient receives the claim token beside the sealed grant, in one `sunrise://share/<token>` link: base64url, unpadded, of `0x01 || claim_token (32) || granter_id_s_pub (32) || grant`, where `granter_id_s_pub` is the key `identity_sig` verifies under and what step 5 checks against the owner's fingerprint. It claims the token from its own account, and from then on reads the owner's channel by naming the grant in its subscription. The relay learns which account reads the Stream; it never learns the recipient's identity, and never holds the key.
5. **Recipient receives** the envelope on next sync. UI shows "Stream X shared with you by Y. Accept?" The recipient's client verifies `identity_sig` under the `granter_id_s_pub` the link carries (step 4), and the recipient checks that key's fingerprint against the owner's, out of band, as the owner checked theirs in step 2. No `ID_S_pub` is published anywhere the relay could look it up.
6. **On accept:** recipient's device opens the HPKE ciphertext using its `ID_D_priv`, stores `stream_key_<epoch>` locally (wrapped under its own `vault_root`), subscribes to the Stream on the relay, decrypts ops, renders.
7. **On decline:** the recipient's decline reaches the owner out of band, as the grant reached the recipient (not built). The owner's device records it as a `ShareDecline` control op on the vault-meta stream, `{ "ShareDecline": { "stream_id", "recipient_identity_id", "declined_at_ms" } }`, which ends the share as a revocation does, with no rotation of its own; the owner's UI shows the result (`Engine::ended_stream_shares`).

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
   Built as the control op `{ "ShareRevoke": { "stream_id", "recipient_identity_id", "effective_at_ms" } }` on the vault-meta stream (`Engine::revoke_share`), which emits it and step 2 in one transaction.
2. Perform a Stream key rotation (see [`key-rotation.md`](./key-rotation.md)). The rotation re-wraps the new epoch for all sibling devices and remaining peers, **excluding** the revoked recipient. A peer whose grant has expired at the rotating device's clock is excluded too: its grant has ended, and a grant of a new epoch would extend it. Each remaining peer's grant of the new epoch carries its `expires_at` unchanged. `reseal_share_grants` in `sunrise-crypto` is this step's peer half, and every rotation calls it: `emit_key_envelopes`, which each rotation distributes its epoch through, re-seals the epoch to the Stream's shares as `ShareGrant` ops, sealed under the same vault-meta epoch as the key envelopes. The revoked list it passes is every recipient whose share a `ShareRevoke` or `ShareDecline` ended. A rotation by a device without `ID_S_priv` re-seals nothing and logs a `ShareResealRequest` naming the shares still in force; a device holding `ID_S_priv` that learns the epoch re-seals it to those.
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

A Note in a shared Stream may contain `{kind: "ref", ref: entity-ref}` pointing to a Task or Note in a Stream the recipient does not have. The **owner is the only origin of cross-stream references in shared content**. That does not rest on an editor's UI. An editor's op that names an entity outside the shared Stream is outside its authority, and every replica parks it unapplied ([ADR-0075](../11-adr/0075-an-editor-writes-a-shared-streams-contents-and-every-replica-is-the-boundary.md) §2–§3). Cross-stream refs in shared content therefore always come from the owner.

On op emission, the **owner's device** runs egress scrubbing per recipient cohort as part of constructing the op:

1. Walk the Note's outbound payload before encryption.
2. For every `{kind: "ref", ref}` whose `ref` is in a Stream this cohort does not share, replace with `{kind: "redacted", reason: "private_ref", placeholder_text: "—"}` ([`../02-domain/notes.md`](../02-domain/notes.md) §In-app references defines the shape). The original ref is preserved in the owner's local copy of the op (unscrubbed); the scrubbed form is what gets encrypted for this cohort.
3. If multiple recipients have heterogeneous access sets, the owner's device emits one envelope per cohort under the same Stream key. The design uses a uniform "all share-grants on a Stream see the same content" model, so this is an edge case for cross-Stream refs only.

A scrubbed envelope is detectable to the **owner** (they retain the original). Recipients cannot tell whether their envelope was scrubbed; this is by design (no leak of the existence of private references).

There is no editor→owner re-scrubbing path. A cross-stream ref in an editor's op never applies anywhere, so there is nothing for the owner to re-scrub on inbound editor ops.

## Cross-relay sharing

Not implemented, and the single answer — the owner's relay is authoritative, there is
no federation, and the credential question is open — lives in
[`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md)
§Cross-server delivery.

## Edits by editors

Decided by [ADR-0075](../11-adr/0075-an-editor-writes-a-shared-streams-contents-and-every-replica-is-the-boundary.md), not built. When an editor modifies a shared Stream:

1. Their device produces entity ops signed by *their* device key and encrypted under the Stream key of an epoch their grant sealed.
2. Ops are published into the owner's channel on the owner's relay, through a write route open only to an account holding a relay grant registered with the role `editor` (ADR-0075 §6). The relay's check is hygiene, not the boundary.
3. Ops carry the editor's `device_id`. Its `DeviceCert` chain travels in the shared Stream, so every replica can verify it without reading the editor's vault-meta log (ADR-0075 §8 item 1). Authorship is preserved on every op.
4. Every replica checks the op against the editor's authority before applying it (§Roles).

There is no "merge request" model. An editor's op that passes §Roles' checks applies on arrival and merges by per-field ops ([ADR-0044](../11-adr/0044-per-field-ops.md)), exactly as the owner's do. A future "review mode" toggle is not designed or built.

Revoking an editor also cuts its writes: the `share_revoke` op carries the last `(seq, op_hash)` the owner accepted from each of the editor's devices. Every replica applies that chain and nothing past or beside it, and one that had already applied an op past the cut takes it back out when the cut lands (ADR-0075 §4). An honest op still in flight when the cut is made is lost, an op past the cut that a replica already showed disappears from it, and the revoke UI says both.

## Privacy implications

Every party — owner, recipient, relay — sees:

- The fact that two identities share something (visible to the relay).
- Op counts and timestamps in the shared Stream (visible to the relay).
- The granting identity's public-key bundle (the recipient verifies it against an OOB fingerprint).

The design does not hide the sharing graph from the relay. Reducing this leakage is tracked in `11-adr/` as future work (oblivious access patterns).
