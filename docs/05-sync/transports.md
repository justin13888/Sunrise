---
status: accepted
---

# Transports

**v1's transport is an SSE stream downstream and typed `POST` operations
upstream** ([ADR-0023](../11-adr/0023-sse-sync-transport.md), which supersedes
[ADR-0005](../11-adr/0005-sync-transport.md)). That is no longer a decision
waiting on an implementation: it is what runs. The client is `SseTransport`
(`crates/sunrise-sync/src/sse.rs`), used by `sunrise-cli`, the Apple apps
through `sunrise-core-bindings`, and the `sunrise-e2e` suite; the server is
`crates/sunrise-server/src/api/sync.rs` over `crates/sunrise-server/src/sync_session.rs`.

The WebSocket is gone rather than being replaced. `crates/sunrise-server/src/ws.rs`
was deleted, and neither `axum` nor `tokio-tungstenite` resolves in `Cargo.lock`
any more. Earlier revisions of this page described the WebSocket as "what runs
today"; it does not, and this revision is the correction.

This page previously said "v1 ships **WebSocket only**" and deferred HTTP
fallbacks to v2, which contradicted ADR-0005's own "primary WebSocket, fallback
HTTP/2 long-poll" on the record for the whole of v1. ADR-0023 settled it in a
third direction and retired both statements.

## The decided transport (ADR-0023)

| Sync concern | Surface |
|---|---|
| Session establishment, version + capability negotiation | `POST /sync/session` → session id, negotiated versions, agreed capability bits |
| Server → client fan-out | `GET /sync/events`, `text/event-stream`, `itemSchema`-typed per OpenAPI 3.2 |
| Client → server ops | `POST /sync/ops` → typed `Ack` response |
| Liveness | SSE comment heartbeats |
| Credential renewal | `POST /sync/session/refresh` |
| Cursor replay and `SYNC_CURSOR_GAP` | SSE `Last-Event-ID` |

The forcing constraint is [ADR-0021](../11-adr/0021-kynos-openapi-server.md):
`kynos` excludes bidirectional streaming on principle, and an event stream is a
single response body, so SSE is the only streaming shape that lands inside the
authoritative OpenAPI document. Cursor resumption is a rename rather than new
machinery — the relay already keeps a durable, monotonically-ordered
`relay_frames.id` per channel and already reports a typed gap when a cursor
falls below `relay_evicted`.

Negotiation semantics do not move. `Hello::negotiate` in
`crates/sunrise-wire-protocol/src/negotiation.rs` keeps its rules, its error
mapping and its frozen fixtures; only the frame that carries it changes. The
`Transport` trait in `sunrise-sync` also keeps its purpose — it is a
three-method byte-frame pipe, which is where a future P2P or WebTransport path
re-enters.

## The routes, as built

Five, all inside the authoritative OpenAPI document because `kynos` generates it
from the handlers themselves:

| Route | Purpose |
|---|---|
| `POST /api/v1/sync/session` | open a session; negotiate versions and capabilities |
| `POST /api/v1/sync/subscribe` | declare the streams this session wants |
| `GET /api/v1/sync/events` | the `text/event-stream` fan-out |
| `POST /api/v1/sync/ops` | upstream ops, answered with a typed `Ack` |
| `POST /api/v1/sync/session/refresh` | credential renewal |

`Hello::negotiate` in `crates/sunrise-wire-protocol/src/negotiation.rs` keeps its
rules, its error mapping and its frozen fixtures unchanged; only the frame that
carries it moved. The `Transport` trait in `sunrise-sync` also keeps its purpose
— a three-method byte-frame pipe, which is where a future P2P or WebTransport
path would re-enter.

**Keepalive is implemented, and it is a comment rather than a frame.** The
stream emits a `:sunrise` comment every `KEEP_ALIVE_SECS` = 15 s
(`crates/sunrise-server/src/api/sync.rs:64-69`, `:570-574`). The `0x0C Ping` /
`0x0D Pong` pair still exists in the message catalog and both sides still answer
one, but neither sends one unprompted: a comment does the same job — stopping an
intermediary from reaping an idle connection — with no frame type and nothing
for the client to answer. `SseTransport` drops incoming comments before they
reach the driver (`sse.rs:223-244`).

Earlier revisions of this page said keepalive was not implemented and that a
dead peer holding a valid token would hold a session until the token expired.
The migration closed that, exactly as the page predicted it would.

**Per-platform notes:**

| Platform | Library / API |
|---|---|
| Rust core (macOS, iOS, CLI) | `hyper` + `hyper-rustls`, framed by `SseTransport` |
| Web | Browser `EventSource` — deferred with the client itself ([ADR-0012](../11-adr/0012-web-wasm-deferred.md)) |
| Android | Deferred; no client exists |

## Reconnect on failure

On connection failure the client backs off exponentially with jitter: initial
**100 ms**, doubling, capped at **30 s**, jitter ±20%, and **`max_retries = 5`**
(`crates/sunrise-sync/src/backoff.rs:31-63`). The schedule is therefore
*exhaustible*, which is the part a reader most needs and which earlier revisions
of this page did not mention at all: five jittered delays of 100, 200, 400, 800
and 1600 ms, after which `next_delay` returns `None`. Exhausting it does not give
up — the session loop resets the policy, sleeps a flat un-jittered 30 s and
starts the sequence again, so a client that cannot reach its relay retries
forever in bursts of five. `Backoff` takes its jitter as an injected `[0, 1]`
value rather than sampling ambient randomness — the determinism gate in CI
forbids the latter. "Connection failure" includes TLS handshake, session open, or
`HelloAck` failing within 30 s.

[`offline-queue.md`](./offline-queue.md) §Backoff owns these numbers, and this
page defers to it rather than keeping a second copy: it also covers the
per-batch retransmit timer that shares the same policy and why the 30 s cap
inside `next_delay` is unreachable on it. An earlier revision here published a
different pair — start 500 ms, cap 60 s — that matched no code, and
[`wire-protocol.md`](./wire-protocol.md) §Error model already records that pair
as the drift this consolidation exists to prevent.

**Not built: the immediate-retry triggers.** Earlier revisions said the push
wakeup signal (below) and OS network-change events trigger an immediate retry.
Neither seam exists — nothing in the workspace observes network reachability,
and no client consumes a push wakeup — so reconnection is driven entirely by the
backoff timer above, as [`offline-queue.md`](./offline-queue.md) §Network changes
states from the other side. The design is still the one to build; it is not a
description of the client.

## Push wakeup

Push is *not a transport*. Push is an out-of-band wakeup: the server sends a content-less push telling a device "you have ops; wake up and connect." See [`../06-server/push-notifications.md`](../06-server/push-notifications.md). Push is **best-effort**: the server retries delivery up to 3 times with 30 s spacing; if all fail, the next foreground or background-fetch (~15 min) catches up. The client SHOULD NOT depend on push for correctness, only for latency.

## Explicit non-goals (v1)

- **HTTP long-poll / short-poll fallbacks.** ADR-0005's original fallback, withdrawn here and formally rejected by ADR-0023: higher latency and more overhead than SSE with no compensating advantage. v1 expects working connectivity to the relay.
- **WebTransport over HTTP/3.** The better *transport* on the technical merits — connection migration, no TCP head-of-line blocking, native binary framing — and rejected for the same reason as the WebSocket: `kynos` excludes it, so sync would still sit outside the OpenAPI document. It also mandates real TLS, which would remove the plaintext loopback path the CLI and e2e suite use. See ADR-0023's alternatives table.
- **LAN / mDNS direct sync.** Devices on the same network always relay through the server. Rationale: partition-tolerance complexity, peer-discovery security, key distribution to peers without a central relay, and NAT traversal all add risk for marginal user benefit. Self-hosters who want LAN-only operation run the server on the LAN.
- **USB / BLE pairing.**
- **Peer-to-peer.** No NAT-traversal, no direct device-to-device sockets. The architecture (per-Stream encryption + relay-agnostic protocol) is compatible with future P2P, but v1 does not ship it.

## Pairing does not run over the relay

Earlier revisions of this page said all pairing is QR-over-server. **It is not,
and the relay has no part in pairing at all.** There is no pairing route in
`crates/sunrise-server/src/api/`, and capability bit 4 `SRV_RELAY_PAIR`
("server forwards Noise-XX pairing transport") is defined in
`crates/sunrise-wire-protocol/src/capability.rs` and **never advertised** — the
server's `Hello` response sets only `REQUIRED_CLIENT_BITS |
REQUIRED_SERVER_BITS | SrvTokenRefresh`.

What ships instead is a **manual two-file pairing-payload exchange**, driven by
the CLI in `crates/sunrise-cli/src/livesync.rs`: `SUNRISE_EXPORT_PAIRING_FILE`
writes this device's `PairingPayload` on startup and `SUNRISE_PAIRING_FILE`
reads one and hands it to `Core::open`. It is read *before* the vault opens,
because since [ADR-0024](../11-adr/0024-key-hierarchy.md) the identity a vault
belongs to is decided when the vault is created; a payload offered afterwards
has nothing left to join. Both steps are best-effort — an unwritable or missing
file is logged (`ui.pair.payload_exported`, `ui.pair.payload_adopted`), not
fatal. The QR, Noise-XX handshake and SAS
machinery in `crates/sunrise-pairing/` (`qr.rs`, `handshake.rs`, `sas.rs`) is
built and tested but has no relay transport under it, so nothing routes a
handshake between two devices yet. See
[`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md)
for the designed flow.
