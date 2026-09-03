---
status: accepted
---

# Transports

**v1's transport is an SSE stream downstream and typed `POST` operations
upstream** ([ADR-0023](../11-adr/0023-sse-sync-transport.md), which supersedes
[ADR-0005](../11-adr/0005-sync-transport.md)). The WebSocket described below is
the **implementation being replaced**, not the settled design. It is what runs
today — `crates/sunrise-server/src/ws.rs` on the server, `WsTransport` under
`crates/sunrise-core/src/sync_driver.rs` on the client — and it is documented
here because it is what a reader will find in the tree.

This page previously said "v1 ships **WebSocket only**" and deferred HTTP
fallbacks to v2, which contradicted ADR-0005's own "primary WebSocket, fallback
HTTP/2 long-poll" on the record for the whole of v1. ADR-0023 settles it in a
third direction and retires both statements.

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

## WebSocket (current implementation, being replaced)

- Bidirectional, low-latency, runs everywhere TLS does.
- Used by every client surface that syncs today: macOS, CLI, and the e2e suite.
- Dialed by `tokio-tungstenite`, which leaves the workspace with the WebSocket.

**Keepalive is not implemented.** A `0x0C Ping` / `0x0D Pong` pair exists in the
message catalog and both sides *answer* one, but **neither side ever sends one
unprompted**: there is no 30 s ping timer and no 90 s idle close anywhere in
`ws.rs` or the sync driver. The only deadline on a server-side session is
token expiry, whose `select!` arm ends an idle session with
`AUTH_TOKEN_EXPIRED` when its bearer ages out — that is a credential check, not
a liveness check. A dead peer holding a valid token therefore keeps a session
until the token expires. Under ADR-0023 liveness becomes SSE comment
heartbeats, so this gap is closed by the migration rather than by adding a
timer to a transport being removed.

**Per-platform notes** (as built / as intended):

| Platform | Library / API |
|---|---|
| Rust core (macOS, CLI) | `tokio-tungstenite` |
| iOS UI layer | `URLSessionWebSocketTask` if needed for background; otherwise core handles |
| Android UI layer | `OkHttp` WebSocket if needed; otherwise core handles |
| Web | Browser WebSocket |

## Reconnect on failure

On connection failure, exponential backoff with jitter: start at **500 ms**, cap at **60 s**, jitter ±20%. "Connection failure" includes TLS handshake, WebSocket upgrade, or `HelloAck` failing within 30 s. The push wakeup signal (below) and OS network-change events trigger an immediate retry attempt.

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
`crates/sunrise-server/src/routes/`, and capability bit 4 `SRV_RELAY_PAIR`
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
