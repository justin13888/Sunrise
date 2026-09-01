---
status: proposed
---

# Presence

> **Status: proposed. Not scheduled for v1.**
> [ADR-0027](../11-adr/0027-v1-self-host-first.md) clause 3 places presence
> after v1, and fixes the condition for its return: an ADR that states the
> behavioural-metadata leak outright and amends
> [`../06-server/overview.md`](../06-server/overview.md) §Non-responsibilities
> in the same change. This document is the design of record for that work, not
> a description of anything that ships.
>
> **What exists in the tree:** two message-kind discriminators and nothing
> behind them (below).
>
> **Why it is not v1:** two blockers, one mechanical and one architectural.
>
> *Mechanical — there is no frame to carry it.*
> `PresenceBeacon` (`0x0A`) and `PresenceUpdate` (`0x0B`) exist as message-kind
> discriminators in `crates/sunrise-wire-protocol/src/messages.rs` and that is
> all: there is no payload type for either, no server handler, and no client
> emitter. Since [ADR-0023](../11-adr/0023-sse-sync-transport.md) there is not
> even a frame to send one on — the socket's catch-all inbound dispatch is gone
> with the socket, and the five sync operations
> (`crates/sunrise-server/src/api/sync.rs`) are typed one per purpose, so a
> beacon has no route that would accept it. It is now unreachable rather than
> silently dropped, which is a smaller gap than it sounds: neither state has a
> handler behind it. Capability bit 36 `CLI_PRESENCE_BEACONS` is defined only in
> [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md)`:173`
> — the registry is prose; no constant for it exists in `crates/`, so it is not
> in `REQUIRED_CLIENT_BITS` because there is no bit to be in it. There is no
> presence channel, no ACL check on one, and no "last activity" tracking.
>
> *Architectural — it would be the relay's first look at user data.*
> **Presence is specified as UNENCRYPTED, and that is a deliberate choice, not
> an oversight** — see the Implementation section: device ids and timestamps are
> treated as metadata the relay sees anyway. It is also the only user data in
> the design that the relay would read in the clear. Sunrise's whole posture is
> that the server never sees content; presence would make it see *behaviour* —
> who is online, when, and which stream cohort someone is looking at, at 30 s
> resolution, retained for as long as the relay chooses. If presence ships, that
> is designed leakage the ADR admitting it MUST state outright, and
> [`../06-server/overview.md`](../06-server/overview.md)'s
> non-responsibilities list MUST be amended to match. Do not treat this page as
> having settled that.
>
> **What holds regardless:** nothing in this file constrains v1 code. The
> *posture* statement does bind: until an ADR says otherwise, the relay reads no
> user data in the clear, and presence is the design that would change it.

Lightweight indicators of which of the user's other devices are online and which shared peers are co-viewing.

## Scope (small on purpose)

- Show the user "Phone is online; Laptop is online; Desktop was last seen 12 minutes ago."
- For shared streams with editor peers, show "Alice is viewing this stream now."
- *Not* show typing indicators, cursor positions, or fine-grained co-presence in v1.

## Implementation

- The server tracks per-device "last activity" time (when it last sent or received bytes).
- The client subscribes to `presence(account_id)` and `presence(stream_id)` channels.
- The server publishes presence as opaque blobs (encrypted under per-account or per-stream keys) — actually, we keep it simple: device IDs and timestamps are *metadata* the server sees anyway, so presence is unencrypted *server-knowledge* that the client uses.
- Privacy: a user can disable presence broadcast for a given device; that device appears offline to others.

## Stream presence

For shared streams, "is co-viewing" is reported by:

- The viewing client emitting an ephemeral "viewing" beacon every 30 s while the Stream view is open.
- The server fanning out to other subscribers.
- These beacons are not stored in the op log; they are a transient pub/sub.

### Channel ACL

Presence channels are subscribed by `(account_id, stream_id)`:

- A device subscribes to presence on a Stream only if its identity is the owner OR a current grant recipient (state ∈ `{accepted}`).
- The relay enforces this on subscribe.
- Subscription is dropped on revoke.

Presence beacons leak that **someone with access** is viewing; they do not leak which person beyond beacons being scoped to identity-id-hash (`BLAKE3(idn, 4)` rendered as 8 lowercase hex chars). Each cohort member can map hash → person via their local people directory.

### Beacon interval

- Default: every 30 s while the Stream view is foreground.
- Configurable: `[presence] beacon_interval_s = 30`, range 10–120.
- A device that goes background sends one final "leaving" beacon; absence after 90 s is treated as offline.

## What the server cannot infer from presence

- *Which* tasks the user is reading (the server doesn't know stream contents and a "viewing this stream" beacon doesn't enumerate tasks).
- *What* the user is editing.

## Disabled mode

A user can run an entirely "presence-off" device. Other devices show it as "offline" but sync still works (sync delivery does not depend on presence).
