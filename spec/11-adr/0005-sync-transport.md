# 0005 — WebSocket as default sync transport

**Status:** accepted

## Context

Devices must exchange ops promptly when both online. Mobile must work behind cellular NAT. Browsers can't open arbitrary TCP. Some networks block long-lived connections.

## Decision

- **Primary:** WebSocket over TLS (`wss://`).
- **Fallback:** HTTP/2 long-poll for restricted networks.
- **LAN supplement:** mDNS-discovered direct WS for opportunistic same-network sync.
- **Future:** P2P (NAT-traversal, libp2p-style) is explicitly deferred.

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
- LAN sync is a real feature, not a research project, and is straightforward over WS.
- Future P2P is structurally compatible — the wire protocol is transport-agnostic.
