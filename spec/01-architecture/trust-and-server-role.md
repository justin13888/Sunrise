---
status: accepted
---

# Trust and Server Role

The single most-asked question about an E2EE app is: *"if the server can't read anything, why does it exist?"* This spec answers that.

## What the server provides

1. **A reliable mailbox.** Devices may be offline for weeks. The server holds encrypted ops until the next device check-in.
2. **Push fanout.** Wakes a device to pull when an op is queued for it.
3. **Encrypted blob storage.** Attachments larger than the op-log payload limit are stored as encrypted blobs; the server holds them but cannot read them.
4. **Authentication for sync.** Verifies that a request claiming to be from device D is actually signed by D's key.
5. **Rate limiting & abuse prevention.** Per-account quotas to keep the system viable.
6. **Coordination for sharing.** Invites, key-exchange envelopes between identities (the keys themselves are wrapped end-to-end).
7. **Account/billing surface.** Email + payment for managed cloud users. Self-hosted servers can omit billing.

## What the server explicitly does *not* do

- It does not see plaintext content. Ever.
- It does not see derived data (search indexes, summaries, embeddings) — those are computed on devices.
- It does not enforce business rules on content. It treats ops as opaque, signed, ordered ciphertext.
- It does not generate notifications based on content. Push payloads are wake-ups only — content is fetched and decrypted on device.
- It does not run integrations on the user's behalf with their cleartext credentials. Integration tokens for third-party APIs (Google Calendar, etc.) live in the encrypted vault and run *on device*, with the server never holding them. (Exception: optional server-side cron for stable-rotated integrations is **out of scope for v1**.)

## What the server *can* see (metadata)

This is honest disclosure to users, not a defect:

- **Account email** (for login).
- **Device IDs** (random per-install) and last-seen timestamps.
- **Op counts and sizes per device.**
- **IP and approximate geo per request** (kept ≤14 days).
- **Push token mapping per device.**
- **Sharing graph:** identity X has shared *something* with identity Y, including counts of ops in shared documents.

Reducing what's in this list is a design pressure. Specifically, mixnet-style request hiding and oblivious access patterns are tracked in [`../11-adr/`](../11-adr/) as future considerations.

## Self-hosted vs managed distinction

The Sunrise server has **two deployment profiles**:

| Profile | Auth | Billing | Push | Geo |
|---|---|---|---|---|
| Managed | Sunrise-issued accounts | Stripe | APNs/FCM via shared cert | Global |
| Self-hosted | Operator-chosen (basic, OIDC, none) | Off | Optional, operator's own certs | Wherever the operator runs |

A user on a self-hosted server may still federate with managed users for sharing. The sharing protocol is operator-agnostic.

## Rationale

The alternative to having a server is pure peer-to-peer. We considered it and rejected it for v1 because:

- Mobile devices cannot be reliable peers (background limits, NAT, battery).
- Sharing across networks needs a rendezvous; that rendezvous is a server even if we call it something else.
- Self-hosting a tiny relay is operationally simpler than running a P2P NAT-traversal stack.

A future P2P transport may exist as a *supplement* (LAN sync, when both devices are on the same network), not a replacement. See [`../05-sync/transports.md`](../05-sync/transports.md).
