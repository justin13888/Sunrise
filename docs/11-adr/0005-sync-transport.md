# 0005 — WebSocket as default sync transport

**Status:** accepted

## Context

Devices must exchange ops promptly when both online. Mobile must work behind cellular NAT. Browsers can't open arbitrary TCP. Some networks block long-lived connections.

## Decision

- **Primary:** WebSocket over TLS (`wss://`).
- **Fallback:** HTTP/2 long-poll for restricted networks.
- **No LAN/P2P/USB/BLE in v1.** Same-network sync relays through the server like everything else; it's fast enough on typical broadband, and the alternative doubles the transport test matrix for a small audience. Future P2P remains structurally compatible (the wire protocol is transport-agnostic) but is not on the v1 path.

## Alternatives considered

| Option | Pros | Cons |
|---|---|---|
| HTTP polling only | Simplest | Higher latency; chatty; hard to hit our reminder-latency target |
| WebRTC | True P2P | NAT/TURN ops complexity; signaling server still needed; mobile background limits |
| QUIC / HTTP/3 | Modern, fast | Browser support uneven for arbitrary streams; library maturity in Rust still warming |
| **WebSocket** | Universal browser support; simple framing; fits our ops nicely | Some corporate networks block; mitigation: HTTP fallback |
| MQTT | Pub/sub semantics | Adds a broker dependency; no clear win |

## Consequences

- Server connection model is straightforward: one WS per active device per account.
- Reconnect logic is easy to test deterministically.
- Restricted networks are handled via fallback without a separate codepath for the *protocol* — only the transport adapter changes.
- Same-network sync is fast enough through a relay; we don't ship a separate LAN code path.
- Future P2P is structurally compatible — the wire protocol is transport-agnostic.
