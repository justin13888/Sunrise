---
status: draft
---

# Transports

The same wire protocol can run over multiple transports. Each transport has different properties.

## T-1: WebSocket (default)

- Bidirectional, low-latency, runs everywhere TLS does.
- The default for desktop, mobile, web, TUI, server-server-not-applicable-here.
- Ping every 30s; close after 90s without traffic and reconnect.

**Per-platform notes:**

| Platform | Library / API |
|---|---|
| Rust core (desktop, TUI) | `tokio-tungstenite` |
| iOS UI layer | `URLSessionWebSocketTask` if needed for background; otherwise core handles |
| Android UI layer | `OkHttp` WebSocket if needed; otherwise core handles |
| Web | Browser WebSocket |

## T-2: HTTP/2 long-poll fallback

For environments where WebSocket is blocked (some corporate networks):

- Client `POST /sync/poll` with cursor; server responds with up-to-N batched ops or 200 empty after a server-side timeout.
- Client `POST /sync/push` with a single OpBatch.

Same protocol messages, transported as a series of HTTP requests.

Used as automatic fallback when WebSocket fails repeatedly; UI may surface "your network is restricting Sunrise; sync is slower."

## T-3: LAN / mDNS direct sync

For the no-server T3 deployment topology, or as an opportunistic *supplement* to T-1:

- Devices advertise via mDNS (`_sunrise._tcp.local.`).
- Same wire protocol over a TLS connection authenticated by device certs (no need for server auth).
- Used to:
  - Bootstrap pairing without internet.
  - Catch up faster when two devices on the same LAN are far behind their mutual cloud cursor.
  - Operate offline (T3 topology).

**Trust:** LAN peers must still be paired devices of the same identity, OR carry a valid share grant. We don't trust unknown LAN peers.

## T-4: USB tether

For initial pairing in extreme-paranoia mode.

- iOS: USB-attached Mac runs Sunrise; devices exchange via local lo-back through `usbmuxd`.
- Android: ADB reverse tunnel.
- Same wire protocol.

Optional and rarely used.

## T-5: BLE (mobile-to-mobile pairing)

For pairing two phones with no shared internet (rare). Uses BLE GATT for the handshake and LAN/mobile data for op transfer.

## P2P (deferred to v2)

Always-on peer-to-peer between devices is tracked but not built. Requires NAT traversal infra. The architecture (per-Stream encryption + relay-agnostic protocol) is compatible with future P2P; the work is operational, not architectural.

## Push wakeup

Push is *not a transport*. Push is an out-of-band wakeup: server sends a content-less push telling a device "you have ops; wake up and connect." See [`../06-server/push-notifications.md`](../06-server/push-notifications.md).

## Choice rules

A client tries transports in order:

1. T-1 (WebSocket).
2. T-2 (HTTP/2) if (1) failed N times.
3. T-3 (LAN) opportunistically when on Wi-Fi and mDNS finds a peer.

The choice is automatic; the user does not need to know.
