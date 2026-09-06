---
status: accepted
---

# Deployment Topologies

Two topologies are designed. **v1 ships T2 only** ([ADR-0027](../11-adr/0027-v1-self-host-first.md)); T1 is retained as a post-v1 target. A user can move between them without data loss.

## T1: Managed cloud — post-v1

```
[Devices] ── TLS ─▶ [Sunrise Cloud relay + blob store + push gw]
                           │
                           ├── object storage (S3-compatible)
                           ├── Postgres (metadata)
                           └── push providers (APNs, FCM, Web Push)
```

- Sunrise operates the relay. **Not a v1 deliverable** ([ADR-0027](../11-adr/0027-v1-self-host-first.md)): v1 ships the self-host single binary only, and there are no plan tiers.
- E2EE applies; Sunrise cannot read content.
- Recommended for users who don't want to operate infrastructure.

## T2: Self-hosted

```
[User devices] ── TLS ─▶ [Single binary: relay + embedded blob store]
                                │
                                └── local disk OR S3-compatible
```

- One Rust binary, optional Postgres, optional S3.
- Auth: operator brings any OIDC issuer (Keycloak, Authelia, Dex, Auth0, Google Workspace, etc.). See [`../06-server/auth.md`](../06-server/auth.md).
- Push: optional; if absent, devices poll. (See [`../06-server/push-notifications.md`](../06-server/push-notifications.md).)
- Use cases: privacy-conscious users; teams of ≤3; researchers.

## Topology migration

| From → To | How |
|---|---|
| T1 → T2 | User runs self-host binary, points clients at it via "Sync server URL" setting; clients re-pair to the new host but keep their identity and data. |
| T2 → T1 | Reverse of T1 → T2. |

The wire protocol is the same in both topologies. Only the server URL and OIDC issuer change.

> **No-server / LAN-only mode is not supported in v1.** Pairing and sync always go through a server (managed or self-hosted). Self-hosters who want LAN-only operation run the server on the LAN.

## Federation note

There is no federation between Sunrise servers, and cross-server delivery is not part of v1 — nor is the sharing it would carry. The one answer, including what happens when sharing does land, is in [`trust-and-server-role.md`](./trust-and-server-role.md) §Cross-server delivery.
