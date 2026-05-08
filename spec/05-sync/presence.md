---
status: accepted
---

# Presence

Lightweight indicators of which of the user's other devices are online and which shared peers are co-viewing.

## Scope (small on purpose)

- Show the user "Phone is online; Laptop is online; TUI was last seen 12 minutes ago."
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
