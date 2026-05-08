---
status: accepted
---

# Server — Overview

The Sunrise server is a thin, untrusted-for-content relay. It is an open-source single binary plus optional dependencies.

## Responsibilities (recap)

| Responsibility | Spec |
|---|---|
| Authenticate sync connections | [`auth.md`](./auth.md) |
| Relay encrypted ops to peer devices and shared identities | [`relay-and-blob-storage.md`](./relay-and-blob-storage.md) |
| Store encrypted op log + blobs durably | [`relay-and-blob-storage.md`](./relay-and-blob-storage.md) |
| Wake devices via push when ops arrive while they're offline | [`push-notifications.md`](./push-notifications.md) |
| Handle account lifecycle (create, recover-blob fetch, delete) | [`api.md`](./api.md), [`auth.md`](./auth.md) |
| Enforce quotas and abuse protection | [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md) |
| (Managed only) Billing | [`billing.md`](./billing.md) |

## Non-responsibilities

- Read content.
- Run business logic on content.
- Generate human-readable notifications.
- Run third-party integrations (Google Calendar OAuth flows happen on-device; the integration runs there).
- Federation between servers (out of scope for v1).

## Process model

A single server binary (`sunrise-server`) listens on:

- TLS WebSocket (`wss://…/sync`) — primary sync transport.
- HTTPS REST (`https://…/api/v1/`) — account lifecycle, blob upload/download, push registration.
- Optional admin HTTP (loopback only) — health, metrics, snapshot ops.

Stateless except for:

- A relational DB (Postgres in production, SQLite for self-host single-binary).
- An object store (S3-compatible in production, local disk or fs for self-host).
- Optional Redis for short-lived presence/session caches; not required.

## Languages / frameworks

| Layer | Choice |
|---|---|
| Server core | Rust, `axum` + `tokio-tungstenite` |
| DB access | `sqlx` |
| Object storage | `aws-sdk-s3` (works against MinIO) |
| Push | `apns2`, `fcm`, `web-push` crates |
| Containerization | OCI image, distroless base |

Why Rust: shares the wire-format crate with the client core (one source of truth for the protocol) and is operationally cheap.

## Stateless scale-out

The server scales horizontally. Each instance:

- Holds active WebSocket connections.
- Reads/writes Postgres for op metadata.
- Reads/writes S3 for op envelopes (and blobs).
- Publishes "new op" notifications to a shared topic (Redis pub/sub or NATS) so other instances holding the receiving device's connection can fan out.

Single-instance self-host omits the pub/sub layer.

## Self-host single binary

For T2 deployments, a "single binary" mode bundles SQLite and disk-backed blob storage. Spec target: `sunrise-server` with no external dependencies, runnable as `./sunrise-server -c sunrise.toml` on a small VM.

## Operational requirements

- TLS termination handled by the binary or a reverse proxy.
- Backup: Postgres dump + S3 snapshot, scheduled by operator.
- Monitoring: `/metrics` Prometheus endpoint with content-free metrics.
- No DB migrations require downtime — additive SQL only.

## Versioning

v1 is a single wire-protocol version: the server speaks v1, clients speak v1, no negotiation. A future major version coordinates with a client release ≥30 days prior and migrates everything atomically; clients that miss the deadline see a clear "please update" message.

Persisted server-side structures (recovery blobs at rest; snapshot blobs the server relays opaquely) follow the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3. The server does not introspect these payloads; it stores opaque bytes.

## Deferred to v2

- **Family plan** (group quotas, multi-identity sharing under one subscription). v1 ships Free and Pro tiers only.
- Cross-server federation.
- Foreground-service "always-on" sync on Android (per-device opt-in; v1 accepts occasional Doze-induced latency).
