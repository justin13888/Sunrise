---
status: accepted
---

# Deployment Topologies

Two topologies are designed. **Only T2 is built** ([ADR-0027](../11-adr/0027-v1-self-host-first.md)); T1 is retained as a future target, not built yet and ranked on the roadmap ([`../roadmap.md`](../roadmap.md)). A user can move between them without data loss.

## T1: Managed cloud — not built

```
[Devices] ── TLS ─▶ [Sunrise Cloud relay + blob store + push gw]
                           │
                           ├── object storage (S3-compatible)
                           ├── Postgres (metadata)
                           └── push providers (APNs, FCM, Web Push)
```

- Sunrise operates the relay. **Not built** ([ADR-0027](../11-adr/0027-v1-self-host-first.md)): the self-host single binary is the only server shape today, and there are no plan tiers.
- E2EE applies; Sunrise cannot read content.
- Recommended for users who don't want to operate infrastructure.

## T2: Self-hosted

```
[User devices] ── TLS ─▶ [Single binary: relay + embedded blob store]
                                │
                                └── local disk
```

- One Rust binary over local disk: one SQLite file for metadata and blobs on the local filesystem ([`../06-server/overview.md`](../06-server/overview.md)). Postgres for multi-node metadata ([#481](https://github.com/justin13888/Sunrise/issues/481)) and an S3-compatible blob store ([#482](https://github.com/justin13888/Sunrise/issues/482)) are **proposed, not built**.
- Auth: operator brings any OIDC issuer (Keycloak, Authelia, Dex, Auth0, Google Workspace, etc.). See [`../06-server/auth.md`](../06-server/auth.md).
- Push: optional; if absent, devices poll. (See [`../06-server/push-notifications.md`](../06-server/push-notifications.md).)
- Use cases: privacy-conscious users; teams of ≤3; researchers.

## Topology migration

| From → To | How |
|---|---|
| T1 → T2 | User runs self-host binary, points clients at it via "Sync server URL" setting; clients re-pair to the new host but keep their identity and data. |
| T2 → T1 | Reverse of T1 → T2. |

The wire protocol is the same in both topologies. Only the server URL and OIDC issuer change.

> **No-server / LAN-only mode is not supported.** Sync always goes through a server (managed or self-hosted). Pairing goes through the relay's rendezvous or, for a device with no relay, a file exchange (`sunrise pair offer`, `request`, `issue`, `accept`; see [`../05-sync/transports.md`](../05-sync/transports.md)); that exchange adds no way to sync without a server. Self-hosters who want LAN-only operation run the server on the LAN.

## Federation note

There is no federation between Sunrise servers, and cross-server delivery is not implemented — nor is the sharing it would carry. The one answer, including what happens when sharing does land, is in [`trust-and-server-role.md`](./trust-and-server-role.md) §Cross-server delivery.
