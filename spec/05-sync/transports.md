---
status: accepted
---

# Transports

The wire protocol runs over exactly two transports in v1. Both terminate TLS at the server.

## T-1: WebSocket (default)

- Bidirectional, low-latency, runs everywhere TLS does.
- Default for desktop, mobile, web, TUI.
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

Same protocol messages, transported as a series of HTTP requests. Used as automatic fallback when WebSocket fails repeatedly; UI may surface "your network is restricting Sunrise; sync is slower."

## Push wakeup

Push is *not a transport*. Push is an out-of-band wakeup: server sends a content-less push telling a device "you have ops; wake up and connect." See [`../06-server/push-notifications.md`](../06-server/push-notifications.md).

## Choice rules

A client tries T-1 first; on N consecutive failures it falls back to T-2. The choice is automatic; the user does not need to know.

## Explicit non-goals (v1)

- **LAN / mDNS direct sync.** Devices on the same network always relay through the server. Self-hosters who want LAN-only operation run the server on the LAN.
- **USB / BLE pairing.** All pairing is QR-over-server (see [`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md)).
- **Peer-to-peer.** No NAT-traversal, no direct device-to-device sockets. The architecture (per-Stream encryption + relay-agnostic protocol) is compatible with future P2P, but v1 does not ship it.
