# 0005 — WebSocket as default sync transport

**Status:** superseded by [0023](./0023-sse-sync-transport.md)

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

## Superseded

[ADR-0023](./0023-sse-sync-transport.md) replaces the WebSocket with an SSE
stream downstream and typed POST operations upstream, so that the sync surface
is described by the same OpenAPI 3.2 document as the rest of the relay
([ADR-0021](./0021-kynos-openapi-server.md)).

Two things above were already untrue before that decision, and are recorded here
so the history reads correctly:

- **The HTTP/2 long-poll fallback was withdrawn, not implemented.**
  [`docs/05-sync/transports.md`](../05-sync/transports.md) narrowed v1 to
  "WebSocket only" and deferred HTTP fallbacks to v2. This ADR was never amended
  to match, so the two documents contradicted each other on the record.
- **The QUIC / HTTP-3 row's reasoning has aged.** It was rejected for uneven
  browser support and warming Rust libraries. ADR-0023 re-examines WebTransport
  on current evidence and still declines it, for an entirely different reason:
  it is not describable in OpenAPI, so it would leave sync outside the document.

What holds unchanged is the last consequence: the wire protocol is
transport-agnostic, and `sunrise-sync`'s `Transport` trait is where a future
WebTransport or P2P path re-enters.
