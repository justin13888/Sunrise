---
status: draft
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

- The viewing client emitting an ephemeral "viewing" beacon every 30s while the Stream view is open.
- The server fanning out to other subscribers.
- These beacons are not stored in the op log; they are a transient pub/sub.

## What the server cannot infer from presence

- *Which* tasks the user is reading (the server doesn't know stream contents and a "viewing this stream" beacon doesn't enumerate tasks).
- *What* the user is editing.

## Disabled mode

A user can run an entirely "presence-off" device. Other devices show it as "offline" but sync still works (sync delivery does not depend on presence).
