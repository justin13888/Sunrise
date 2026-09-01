---
status: accepted
---

# Trust and Server Role

The single most-asked question about an E2EE app is: *"if the server can't read anything, why does it exist?"* This spec answers that.

## What the server provides

1. **A reliable mailbox.** Devices may be offline for weeks. The server holds encrypted ops until the next device check-in.
2. **Push fanout.** Wakes a device to pull when an op is queued for it.
3. **Encrypted blob storage.** Attachments larger than the op-log payload limit are stored as encrypted blobs; the server holds them but cannot read them.
4. **Authentication for sync.** Verifies an OIDC access token (issued by a separate IdP) plus a registered device ID. See [`../06-server/auth.md`](../06-server/auth.md).
5. **Rate limiting & abuse prevention.** Per-account quotas to keep the system viable.
6. **Coordination for sharing.** Invites, key-exchange envelopes between identities (the keys themselves are wrapped end-to-end).
7. **Account/billing surface.** Email + payment for managed cloud users. Self-hosted servers can omit billing.

## What the server explicitly does *not* do

- It does not see plaintext content. Ever.
- It does not see derived data (search indexes, summaries, embeddings) — those are computed on devices.
- It does not enforce business rules on content. It treats ops as opaque, signed, ordered ciphertext.
- It does not evaluate a role, a grant, a revocation or an expiry. Every such check is a signature check performed by a *receiving client* against a record in that client's own vault. Where a spec says "the server also checks", it is wrong; the relay cannot reach the grant, the cert or the payload. (The one revocation the relay does act on is its own `devices.revoked` flag, which gates authentication — account metadata it already holds, not a content decision.)
- It does not generate notifications based on content. Push payloads are wake-ups only — content is fetched and decrypted on device.
- It does not run integrations on the user's behalf with their cleartext credentials. Integration tokens for third-party APIs (Google Calendar, etc.) live in the encrypted vault and run *on device*, with the server never holding them. (Exception: optional server-side cron for stable-rotated integrations is **out of scope for v1**.)

## What the server *can* see (metadata)

This is honest disclosure to users, not a defect. The list is the relay's actual schema (`crates/sunrise-server/src/store.rs`), not a summary of it:

- **Account email** — `accounts.email`, plaintext, plus the OIDC subject that identifies the user to the issuer: `accounts.oidc_iss` and `accounts.oidc_sub`.
- **Per device:** its id, its **public keys** (`device_pub_s`, `device_pub_d`), its **nickname** — a free-form human-readable device name — its **platform**, its reported **app version**, and created/last-seen timestamps. The `device_cert` is stored too, as opaque `TEXT` the server never parses or verifies.
- **Push tokens per device**, in plaintext (`push_tokens`).
- **Per frame of ciphertext:** its size and arrival timestamp, plus the routing ids the relay parses out of each envelope header — `(stream_id, device_id, seq)` — which is what makes op counts and per-device sizes derivable.
- **IP and approximate geo per request** (kept ≤14 days).
- **Sharing graph:** identity X has shared *something* with identity Y, including counts of ops in shared documents. (Not yet reachable — sharing is unimplemented; see [`../03-crypto/sharing-with-others.md`](../03-crypto/sharing-with-others.md).)

`nickname`, `platform` and `app_version` were absent from earlier revisions of this list. A nickname is the most user-legible item on it — it names a machine the way its owner does — so it is disclosed explicitly rather than folded into "device IDs".

Reducing what's in this list is a design pressure. Specifically, mixnet-style request hiding and oblivious access patterns are tracked in [`../11-adr/`](../11-adr/) as future considerations.

## Why the boundary holds structurally

The rule "the server never sees plaintext" is enforced by the type system, not by discipline:

- **`sunrise-server` has no `sunrise-crypto` dependency.** Its `Cargo.toml` does not list it, so no server code path can reach a key type, an AEAD, or the envelope opener — a relay that wanted to decrypt could not call the code to do it without a manifest change a reviewer would see.
- **The only envelope type it can reach is `EnvelopeHeader`** (`crates/sunrise-cbor/src/envelope_header.rs`), which carries `{stream_id, device_id, seq}` and **no payload field**. Its own doc comment states the intent: "Intentionally not a subset-of-`OpEnvelope` struct: there is no payload, no signature and no nonce here, because a consumer of this type is one that must not have them." Frames are stored verbatim as `relay_frames.bytes` and forwarded unopened.

Two limits on that, stated rather than left to be discovered:

- **The relay's own SQLite database is not encrypted.** The client vault is SQLCipher-keyed by `BLAKE3.derive_key("sunrise.sqlcipher_key.v1", vault_root)`; the server calls plain `Connection::open` with no `PRAGMA key`. Everything in the metadata list above sits in a file an operator or a backup can read directly. What that file does *not* contain is anything openable — the frames in it are ciphertext the server has no key for.
- **The blob store content-addresses over ciphertext.** `blob_id` is `blb_` plus the first 16 bytes of a BLAKE3 hash of the *uploaded bytes*, which `finalize` recomputes from disk rather than trusting the client's claim (`crates/sunrise-server/src/api/blobs.rs`), so the server can tell that two uploads are byte-identical. That is deliberate and harmless in practice: each attachment gets a fresh random per-blob key, so two identical plaintexts encrypt to different ciphertext and do not collide. The server learns "these two uploads are the same ciphertext", never "these two attachments are the same file".

## Self-hosted vs managed distinction

The Sunrise server has **two deployment profiles**:

| Profile | Auth | Billing | Push | Geo |
|---|---|---|---|---|
| Managed | Sunrise-operated OIDC issuer | Stripe | APNs/FCM via shared cert | Global |
| Self-hosted | Operator-chosen OIDC issuer (Keycloak, Authelia, Auth0, …) | Off | Optional, operator's own certs | Wherever the operator runs |

A user on a self-hosted server may still federate with managed users for sharing. The sharing protocol is operator-agnostic.

### Cross-server delivery

This is the single answer; other specs point here rather than restating it.

**Cross-server delivery is not in v1.** Sharing itself is deferred
([ADR-0020](../11-adr/0020-v1-must-demotions.md) §(a),
[ADR-0027](../11-adr/0027-v1-self-host-first.md) clause 5), so there is no
cross-server case to answer yet.

When sharing lands, the rule is that **the owner's relay is authoritative**:
`ShareGrantPayload` carries `relay_url`, the recipient's client adds an outbound
connection to it alongside its own relay connection, and there is no federation
between independent relays. If the owner's relay is unreachable the shared Stream
is unavailable; there is no peer-to-peer fallback.

What the credentials for that outbound connection are is **not decided**, and
cannot be until the grant model exists. Earlier revisions specified a
`relay_token` short-lived bearer minted at grant time; nothing implements it, no
route accepts it, and designing an authentication token before the thing it
authorizes is the wrong order. It is removed rather than left standing as a
contract.

## Rationale

The alternative to having a server is pure peer-to-peer. We considered it and rejected it for v1 because:

- Mobile devices cannot be reliable peers (background limits, NAT, battery).
- Sharing across networks needs a rendezvous; that rendezvous is a server even if we call it something else.
- Self-hosting a tiny relay is operationally simpler than running a P2P NAT-traversal stack.

v1 has no P2P or LAN-direct transport. See [`../05-sync/transports.md`](../05-sync/transports.md).
