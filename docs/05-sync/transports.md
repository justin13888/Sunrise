---
status: accepted
---

# Transports

v1 ships **WebSocket only**. The wire protocol terminates TLS at the server.

## WebSocket

- Bidirectional, low-latency, runs everywhere TLS does.
- Used by every client surface: macOS, CLI, and the mobile/web clients when built.
- Ping every 30 s; close after 90 s without traffic and reconnect.

**Per-platform notes:**

| Platform | Library / API |
|---|---|
| Rust core (macOS, CLI) | `tokio-tungstenite` |
| iOS UI layer | `URLSessionWebSocketTask` if needed for background; otherwise core handles |
| Android UI layer | `OkHttp` WebSocket if needed; otherwise core handles |
| Web | Browser WebSocket |

## Reconnect on failure

On connection failure, exponential backoff with jitter: start at **500 ms**, cap at **60 s**, jitter ±20%. "Connection failure" includes TLS handshake, WebSocket upgrade, or `HelloAck` failing within 30 s. The push wakeup signal (below) and OS network-change events trigger an immediate retry attempt.

HTTP-based fallback transports (long-poll, short-poll) are **deferred to v2** for hostile-network scenarios. v1 declines to ship them rather than carry the protocol-surface complexity.

## Push wakeup

Push is *not a transport*. Push is an out-of-band wakeup: the server sends a content-less push telling a device "you have ops; wake up and connect." See [`../06-server/push-notifications.md`](../06-server/push-notifications.md). Push is **best-effort**: the server retries delivery up to 3 times with 30 s spacing; if all fail, the next foreground or background-fetch (~15 min) catches up. The client SHOULD NOT depend on push for correctness, only for latency.

## Explicit non-goals (v1)

- **HTTP fallback transports.** Deferred to v2 (above). v1 expects working WebSocket connectivity.
- **LAN / mDNS direct sync.** Devices on the same network always relay through the server. Rationale: partition-tolerance complexity, peer-discovery security, key distribution to peers without a central relay, and NAT traversal all add risk for marginal user benefit. Self-hosters who want LAN-only operation run the server on the LAN.
- **USB / BLE pairing.** All pairing is QR-over-server (see [`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md)).
- **Peer-to-peer.** No NAT-traversal, no direct device-to-device sockets. The architecture (per-Stream encryption + relay-agnostic protocol) is compatible with future P2P, but v1 does not ship it.
