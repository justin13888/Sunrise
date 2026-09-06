---
status: accepted
---

# Server — Overview

The Sunrise server is a thin, untrusted-for-content relay. It is an open-source single binary plus optional dependencies.

> **Implementation status.** What `crates/sunrise-server` builds today is the
> self-host single-binary shape and nothing else: `rusqlite` against one SQLite
> file (`store.rs`) and blobs on the local filesystem (`sunrise_storage::BlobStore`).
> There is **no** Postgres, S3, Redis, pub/sub, or push delivery anywhere in
> `crates/` — no `sqlx`, `aws-sdk-s3`, `apns2`, `fcm`, or `web-push` appears in
> any `Cargo.toml`. The managed and scale-out sections below are design targets;
> each unbuilt piece is marked. Two accepted ADRs moved this document's ground
> and both have landed:
> [ADR-0021](../11-adr/0021-kynos-openapi-server.md) replaced the hand-written
> `axum` routing with `kynos` and a generated, authoritative OpenAPI 3.2
> document, and [ADR-0023](../11-adr/0023-sse-sync-transport.md) replaced the
> `/sync` WebSocket with SSE downstream plus typed `POST` upstream. Neither
> `axum` nor `tokio-tungstenite` resolves in `Cargo.lock` any more.

## Responsibilities (recap)

| Responsibility | Spec |
|---|---|
| Authenticate sync connections | [`auth.md`](./auth.md) |
| Relay encrypted ops to peer devices and shared identities | [`relay-and-blob-storage.md`](./relay-and-blob-storage.md) |
| Store encrypted op log + blobs durably | [`relay-and-blob-storage.md`](./relay-and-blob-storage.md) |
| Wake devices via push when ops arrive while they're offline — **not implemented** | [`push-notifications.md`](./push-notifications.md) |
| Handle account lifecycle — create only; **recovery-blob fetch and account deletion have no route** | [`api.md`](./api.md), [`auth.md`](./auth.md) |
| Enforce fixed operator limits — request body size, blob chunk/count/size ceilings, relay-log retention | [`api.md`](./api.md), [`relay-and-blob-storage.md`](./relay-and-blob-storage.md) |

## Non-responsibilities

- Read content. The relay does parse each op envelope's **cleartext routing header** (`stream_id`, `device_id`, `seq`) so it can filter a replay against a subscriber's cursors, and it stores the envelope bytes durably. Neither is a step toward reading content: the ciphertext field is never touched, and the bytes it stores are the bytes it already relays. See [`relay-and-blob-storage.md`](./relay-and-blob-storage.md).
- Run business logic on content.
- Generate human-readable notifications.
- Run third-party integrations (Google Calendar OAuth flows happen on-device; the integration runs there).
- Federation between servers (out of scope for v1).

## Process model

A single server binary (`sunrise-server`) listens on one socket (`[server] listen`,
default `127.0.0.1:8443`), serving:

- The sync surface under `/api/v1/sync/` — `session`, `subscribe`, `ops` and
  `session/refresh` as typed `POST`s, and `events` as the `text/event-stream`
  fan-out ([ADR-0023](../11-adr/0023-sse-sync-transport.md)). The binary
  terminates no TLS of its own; a `[tls]` config block is rejected by the
  parser, so `https://` means a reverse proxy in front.
- The rest of `/api/v1/` — account lifecycle, devices, blobs, push-token
  registration, `meta`, `health`.
- `/metrics` — Prometheus text exposition, mounted only when the listener is
  loopback; a public bind serves `404` there and logs
  `srv.start.metrics_withheld`.

There is no admin HTTP surface: nothing under `/api/v1/admin/` is routed.

Stateless except for:

- One SQLite database (`rusqlite`), holding accounts, devices, push tokens
  **and** the durable relay op log. There is no Postgres.
- A local-filesystem blob root (`ServerConfig::blob_root`). There is no object
  store.
- No Redis, and no presence/session cache of any kind.

## Languages / frameworks

| Layer | Choice | Built |
|---|---|---|
| Server core | Rust, `kynos` over `hyper` ([ADR-0021](../11-adr/0021-kynos-openapi-server.md)); the sync stream is `kynos::response::stream::sse`, and the routing, the security schemes and the OpenAPI 3.2 document all come off the handlers | yes |
| DB access | `rusqlite` (workspace feature set `bundled-sqlcipher`, `blob`, `trace`) | yes |
| Object storage | `sunrise_storage::BlobStore` — chunk files on the local filesystem | yes |
| Push | `push::PushProvider` trait plus `push::LoggingProvider`, which increments a counter and returns `Ok(())` | trait only |
| HTTPS client (JWKS/discovery) | `hyper` + `hyper-rustls` | yes |
| Containerization | OCI image, distroless base | no |

The relay links **no** `sunrise-crypto`. Its only view into an op is
`sunrise_cbor::decode_envelope_header`, a type with no payload field — the
non-responsibility above is enforced by the dependency graph, not by discipline.

Why Rust: shares the wire-format crate with the client core (one source of truth for the protocol) and is operationally cheap.

## Stateless scale-out (design target — not implemented)

Nothing in this section is built. There is no pub/sub layer, no second
instance, and no external store to share. It records the intended shape.

The server scales horizontally. Each instance:

- Holds the open event streams.
- Reads/writes Postgres for op metadata.
- Reads/writes S3 for op envelopes (and blobs).
- Publishes "new op" notifications to a shared topic (Redis pub/sub or NATS) so other instances holding the receiving device's connection can fan out.

Single-instance self-host omits the pub/sub layer.

## Self-host single binary

For T2 deployments, a "single binary" mode bundles SQLite and disk-backed blob storage. **This is the only mode that exists**: `sunrise-server` has no external dependencies and runs as `./sunrise-server -c sunrise.toml` on a small VM.

## Operational requirements

- TLS termination handled by the binary or a reverse proxy.
- Backup: as built, one `tar` of the data dir — the SQLite file is the whole
  server state, op log included. A Postgres dump plus an S3 snapshot is the
  scaled shape, and there is no scaled deployment.
- Monitoring: `/metrics` Prometheus endpoint with content-free metrics. The
  metrics and any admin surface MUST be reachable on loopback only, or behind
  operator authentication; a public bind MUST NOT serve them. `build_router`
  enforces this by **mounting `/metrics` only when the listener is loopback**,
  and logging `srv.start.metrics_withheld` when it is not. An operator who
  wants the endpoint remotely puts a reverse proxy in front of the loopback
  bind. It was previously merged into the public router with no auth layer and
  no bind check, which published the counter set on any public deployment.
- No DB migrations require downtime — additive SQL only.

## Versioning

v1 is a single wire-protocol version: the server speaks v1, clients speak v1, no negotiation. A future major version coordinates with a client release ≥30 days prior and migrates everything atomically; clients that miss the deadline see a clear "please update" message.

Persisted server-side structures (recovery blobs at rest; snapshot blobs the server relays opaquely) follow the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3. The server does not introspect these payloads; it stores opaque bytes.

## Deferred to v2

- **Managed cloud, plan tiers and billing** ([ADR-0027](../11-adr/0027-v1-self-host-first.md)). v1 ships one server shape and has no tiers.
- Cross-server federation.
- Foreground-service "always-on" sync on Android — moot for v1, which ships no Android client at all ([ADR-0027](../11-adr/0027-v1-self-host-first.md) clause 4).
