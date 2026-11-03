# 0023 — Sync moves to SSE downstream and typed POST upstream

**Status:** accepted

**Supersedes** [ADR-0005](./0005-sync-transport.md).
**Forced by** [ADR-0021](./0021-kynos-openapi-server.md).
**Amends** [`docs/05-sync/transports.md`](../05-sync/transports.md) and
[`docs/05-sync/wire-protocol.md`](../05-sync/wire-protocol.md).

**Amended (2026-09):** the Decision's frame-mapping table shipped without a
`Subscribe` row. The row is added there, in place, and the reason is recorded
beneath the table. Nothing else in this ADR changes. See the **Amendment** at
the end of this file.

**Amended (2026-11):** the `Transport` trait is no longer the three-method
pipe the Consequences describe, and the account and blob routes are reached
through a hand-written client rather than a generated one. Neither changes the
decision; both are recorded in the second **Amendment** at the end of this
file.

## Context

`/sync` is the only surface ops travel over. Everything else the relay serves is
account, device or blob lifecycle.

Two things forced a decision now.

**First, the record was already inconsistent.** ADR-0005 decided "Primary:
WebSocket over TLS. Fallback: HTTP/2 long-poll for restricted networks."
`docs/05-sync/transports.md` later withdrew the fallback — "v1 ships **WebSocket
only**", with HTTP-based fallbacks "deferred to v2" — and ADR-0005 was never
amended. The two documents have contradicted each other on the record since.

**Second, [ADR-0021](./0021-kynos-openapi-server.md) adopted a framework that
excludes bidirectional streaming permanently**, on a principle rather than a
roadmap: "OpenAPI describes HTTP request/response semantics. A stream that stops
being either belongs to AsyncAPI." Server-Sent Events are admitted under its
`openapi32` feature because an event stream is a single response body.

A third motivation was raised and turned out to be misdiagnosed, which is worth
recording because it nearly drove a different decision. The concern was that the
WebSocket surface is loosely typed. Its frames are in fact `Serialize`/`Deserialize`
Rust structs in `sunrise-wire-protocol`, imported by both `sunrise-server::ws` and
`sunrise-core::sync_driver` — one definition, no mirroring, stronger than most
REST. What is actually loose is five specific defects, none of them properties of
the transport:

* dispatch ends in `_ => true`, silently dropping `SnapshotReq`, `PresenceBeacon`,
  `PresenceUpdate` and `Nack`;
* `Hello`/`HelloAck` bypass the canonical codec every other payload uses;
* canonical-CBOR field ordering is maintained by hand-written comments;
* no machine-readable protocol schema exists — the CDDL in `wire-protocol.md` is
  prose nothing validates;
* `Nack` is defined and never sent.

**Changing transport fixes none of those**, and they are fixed in place
regardless of this ADR. The reason to move is the document, not the typing.

## Decision

**The WebSocket is replaced by an SSE stream downstream and typed POST
operations upstream.** OpenAPI 3.2's sequential media types and `itemSchema`
describe each event of a `text/event-stream` response, so the sync surface lands
inside the authoritative document rather than beside it.

| WebSocket frame | Replacement |
|---|---|
| `Hello` / `HelloAck` | `POST /sync/session` → session id + negotiated versions and capability bits |
| server → client fan-out | `GET /sync/events` — `text/event-stream`, `itemSchema`-typed |
| `Subscribe` | `POST /sync/subscribe` → takes effect on the next `GET /sync/events` |
| `OpBatch` up, `Ack` down | `POST /sync/ops` → typed `Ack` response |
| `Ping` / `Pong` | SSE comment heartbeats |
| `RefreshToken` / `RefreshTokenAck` | `POST /sync/session/refresh` |
| cursor replay, `SYNC_CURSOR_GAP` | SSE `Last-Event-ID` |

That last row is why this is cheaper than it looks. The relay already keeps a
durable, monotonically-ordered `relay_frames.id` per channel, already replays
from a client-supplied cursor, and already reports a typed gap rather than
silently under-delivering when a cursor falls behind `relay_evicted`.
`Last-Event-ID` is the same idea with a standard spelling, so resumption is a
rename of machinery that exists and is tested, not new machinery.

**Amended (2026-09):** the `Subscribe` row was absent when this ADR was
written and is added here to match what shipped. `POST /api/v1/sync/subscribe`
is its own operation keyed by session
(`crates/sunrise-server/src/api/sync/`; `subscribeStreams` in
`schemas/generated/openapi.v1.json`), and the module header there carries the same
mapping. The socket's `Subscribe` frame carried the same stream set and
cursors, so this is that frame's replacement rather than a new capability —
the table was incomplete, not the build.

The negotiated-session state that `Hello` established once per connection moves
into `POST /sync/session`, which returns a session id the SSE stream carries.
Version and capability negotiation itself is unchanged — `Hello::negotiate` keeps
its semantics, its error mapping and its frozen fixtures.

## Alternatives considered

| Option | Why not |
|---|---|
| **Keep the WebSocket, fix the five defects** | Genuinely tempting: it preserves duplex, costs no base64, and keeps a subsystem with eight cursor scenarios, four chaos scenarios, token-expiry and multi-tenant isolation tests passing today. It also leaves sync permanently outside the OpenAPI document and requires a second stack beside kynos for the life of the product. The five defects are fixed either way, so they are not an argument for it. |
| **WebTransport over HTTP/3** | The best *transport* of the options, and it does not solve the problem. QUIC would bring connection migration across a Wi-Fi→cellular handover, no TCP head-of-line blocking, 0-RTT resume, native binary framing, and independent streams that map neatly onto per-Stream cursors. But kynos excludes WebTransport in the same breath as WebSockets and for the same reason, so sync would still sit outside the document; adopting it means revising kynos's founding principle, not just adding a transport. It also mandates real TLS, which removes the `ws://127.0.0.1:8443/sync` plaintext loopback path the CLI and the e2e suite use, and offers no answer on UDP-blocked networks except the fallback this project has ruled out. Reconsider it if the relay ever needs mobile-handover resilience more than it needs one described surface. |
| **HTTP/2 long-poll** | ADR-0005's original fallback. Higher latency and more overhead than SSE with no compensating advantage; `transports.md` had already withdrawn it. |
| **gRPC bidirectional streaming** | Typed with codegen, but not OpenAPI-describable, and it adds protobuf beside CBOR. |

## Consequences

* The whole relay surface is described by one OpenAPI 3.2 document, and `spargen`
  generates a client for the sync path as well as the lifecycle path.
* **Base64 is the real cost, and it is bounded.** SSE is UTF-8, so CBOR envelopes
  are base64-encoded at roughly +33%. Bulk data does not traverse `/sync` —
  attachments go through the content-addressed blob 2PC — so the overhead applies
  to small op envelopes, not to payloads. The 4 MiB frame cap governs batches.
* Establishing a session costs two round trips instead of one upgrade.
* Upstream latency is unaffected in practice: the client already batches through
  a persistent outbox, so a POST per batch replaces a frame per batch.
* `sunrise-wire-protocol`'s frame header — magic, versions, `msg_kind`, flags,
  length — is subsumed by HTTP framing and SSE event types for this transport.
  The *payload* types and their canonical CBOR encoding are unchanged, so
  `Hello::negotiate`, the capability bitfield and the frozen version fixtures all
  survive. The zstd flag, implemented and never enabled at any call site, is
  retired with the header rather than carried forward unused.
* `axum` and `tokio-tungstenite` leave the workspace with the WebSocket, ending
  the two-stack migration state ADR-0021 opens.
* The `Transport` trait in `sunrise-sync` keeps its purpose. It is a three-method
  byte-frame pipe, which is why ADR-0005's claim that the wire protocol is
  transport-agnostic held; a future P2P or WebTransport path re-enters here.
  (It has since widened to fifteen methods; see the
  [2026-11 amendment](#amendment-2026-11-the-transport-widened-and-the-account-and-blob-routes-are-hand-written).)

## Amendment (2026-09): the mapping table was missing a row

The frame-mapping table in the Decision claimed a one-for-one replacement and
listed six of the socket's seven frames. `Subscribe` — which carries the
per-stream cursors and drives the cursor-gap and `CaughtUp` protocol — had no
row. The row and its reasoning are now in the table's own section, where a
reader checking the mapping will be looking.

This corrects the record, not the build: `POST /api/v1/sync/subscribe` shipped
as its own session-keyed operation, is in `schemas/generated/openapi.v1.json` as
`subscribeStreams`, and `crates/sunrise-server/src/api/sync/` has carried the
complete mapping in its module header since it was written. The implementation
was never ambiguous about it; this ADR was.

## Amendment (2026-11): the Transport widened, and the account and blob routes are hand-written

**What the original text said.** The Consequences called the `Transport` trait
"a three-method byte-frame pipe", and said `spargen` "generates a client for
the sync path as well as the lifecycle path".

**The trait has fifteen methods.** The three frame methods — `send_frame`,
`recv_frame`, `close` — are still the pipe, and are still all a frame-only
transport implements. Twelve more reach the relay's account and blob API:

* `revoke_device`;
* `blob_init`, `blob_put_chunk`, `blob_finalize`, `blob_fetch` — the blob
  two-phase commit;
* `snapshot_latest`, `snapshot_pin` (ADR-0068);
* `key_bundle_latest`, `key_bundle_pin` (ADR-0069);
* `identity_chain_latest`, `identity_chain_pin`, `recovery_blob_replace`
  (ADR-0070).

Each of the twelve defaults to `TransportError::Unsupported`, so a frame-only
transport still implements three methods and nothing else; `SseTransport`
overrides all of them.

The reason they are on the trait rather than on an HTTP client beside it is
recorded on the trait itself, in its doc comment's "The four blob operations"
section and in `revoke_device`'s
(`crates/sunrise-sync/src/transport.rs#Transport`): the driver already holds
exactly one transport, the device binding that signs every one of these
routes, and the only knowledge in the client of whether the relay is
reachable. A second client would need its own copy of the base URL, the
bearer, the signer and the reachability state, and would be one more thing to
keep in step with all of them. The blob upload is four operations rather than
one because its sequencing is where the idempotency lives, and that is a
policy `sunrise_core::blob_sync` decides and tests, not the transport.

The purpose the Consequences gave the trait holds: the wire protocol is still
carried as whole frames, and a P2P or WebTransport path still re-enters here.
What changed is that the seam also carries the account API, and a new
transport either implements it or answers `Unsupported`.

**The account and blob routes are hand-written.** `SseTransport` reaches every
route it calls — `/sync`, `/devices/by-vault-id/{id}`, `/blobs`,
`/snapshots/{stream}` and `/accounts/me` — through its own `hyper` client in
`crates/sunrise-sync/src/sse/`, not through the `spargen`-generated
`sunrise_relay_client::api::Client`. The generated client covers the
bootstrap, push-registration and recovery-read paths in `sunrise-relay-client`. So the first
Consequence holds for the document — every route above is in
`schemas/generated/openapi.v1.json` — but not, today, for the client.

Whether the account and blob methods move onto the generated client is an open
decision, tracked in #774 (Q-C-10); it changes a dependency on that path and
moves code across crates, so it is not taken here. Until it is, those routes
stay hand-written.

Neither part changes the decision. Both record its shape as built.
