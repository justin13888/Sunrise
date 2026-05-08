---
status: draft
---

# Deployment Topologies

Three topologies are supported. A user can move between them without data loss.

## T1: Managed cloud (default)

```
[Devices] ── TLS ─▶ [Sunrise Cloud relay + blob store + push gw]
                           │
                           ├── object storage (S3-compatible)
                           ├── Postgres (metadata)
                           └── push providers (APNs, FCM, Web Push)
```

- Sunrise operates the relay. Free tier and paid tiers (see [`../06-server/billing.md`](../06-server/billing.md)).
- E2EE applies; Sunrise cannot read content.
- Recommended for users who don't want to operate infrastructure.

## T2: Self-hosted

```
[User devices] ── TLS ─▶ [Single binary: relay + embedded blob store]
                                │
                                └── local disk OR S3-compatible
```

- One Go/Rust binary, optional Postgres, optional S3.
- Auth: basic auth, OIDC, or "single user, paired devices only" mode.
- Push: optional; if absent, devices poll. (See [`../06-server/push-notifications.md`](../06-server/push-notifications.md).)
- Use cases: privacy-conscious users; teams of ≤3; researchers.

## T3: LAN-only / no server (limited)

```
[Device A]  ── mDNS/BLE/USB ─▶  [Device B]
```

- No long-running server. Devices discover each other on a local network and exchange ops directly.
- Useful for: travel without internet, paired offline use, initial setup before any cloud account.
- Limitations: shared documents with a remote identity are not possible; reminders to a not-currently-paired device are not delivered until they meet again.
- Implementation: same wire protocol as T1/T2, but transport is `mdns+ws` or `usb-tether+http` instead of `wss`.

## Topology migration

| From → To | How |
|---|---|
| T3 → T1 | User signs up; first sync uploads queued ops. |
| T1 → T2 | User runs self-host binary, points clients at it via "Sync server URL" setting; clients re-pair to the new host but keep their identity and data. |
| T1 → T3 | User stops paying / disconnects. Existing devices remain functional and can sync over LAN. New devices cannot pair without an out-of-band channel. |
| T2 → T1 | Reverse of T1 → T2. |

The wire protocol is the same in all topologies. Only the transport URL and the auth mode change.

## Federation note

There is no federation between Sunrise servers in v1. A managed-cloud user and a self-hosted user can still share *data* by adding each other's identity (the sharing protocol uses the *user's* public identity, not the server). The relay path for shared ops is whichever server hosts the *shared document* (see [`../05-sync/shared-documents.md`](../05-sync/shared-documents.md)). Cross-server delivery is not part of v1.
