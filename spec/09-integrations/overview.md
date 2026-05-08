---
status: accepted
---

# Integrations — Overview

External integrations connect Sunrise to systems we *don't* build. Integrations run **on-device**: tokens live in the encrypted vault; the server never holds third-party credentials.

| Integration | Direction | Spec |
|---|---|---|
| Google Calendar | Bidirectional | [`google-calendar.md`](./google-calendar.md) |
| CalDAV | Bidirectional | [`caldav.md`](./caldav.md) |
| iCalendar (.ics) | Import / export | [`icalendar.md`](./icalendar.md) |
| Webhooks (outbound) | Outbound triggered by automation rules | [`webhooks.md`](./webhooks.md) |
| Email-to-Sunrise | Inbound (deferred) | [`email.md`](./email.md) |

## Design principles

1. **On-device.** Integration credentials and run-loop live in the user's vault and run on their devices. The server is not a credentialed proxy.
2. **Per-Stream opt-in.** Integrations are turned on per Stream, not globally. A user can sync `Work` to Google Calendar and not `Personal`.
3. **No silent surprise edits.** Importing a calendar event creates a *read-only* Block. Mutations to external systems require explicit "push from this Stream" toggle.
4. **Token rotation** handled per the third party's API; tokens stored encrypted at rest in the vault.
5. **Failures are visible.** A failing integration shows an in-app banner, not a silent log line.

## Multi-device coordination

Multiple devices running the same integration would call the API multiple times. We elect a **primary integration runner** per integration:

- The most-recently-active device that has the integration's token decrypted.
- Other devices defer to it via a periodic heartbeat in a control op.
- On the runner's prolonged absence (> 24h), another device takes over.

This avoids API rate-limit duplication.

## Token storage

Tokens stored as fields in the integration config inside the Stream entity (CRDT-synced, encrypted), reachable only after vault unlock. They are never in plaintext in logs or in transit beyond the third-party's TLS endpoint.

## Disabling

Disabling an integration:

- Flushes any in-flight sync.
- Marks imported entities as orphaned (read-only with a "source disconnected" banner).
- Optionally deletes imported entities (user choice).

## Building new integrations

The integration interface is a Rust trait inside the core:

```rust
trait Integration: Send + Sync {
    fn id(&self) -> IntegrationKind;
    async fn sync(&self, ctx: &IntegrationCtx) -> Result<()>;
    async fn handle_event(&self, evt: DomainEvent) -> Result<()>;
    async fn revoke(&self) -> Result<()>;
}
```

New integrations are added by implementing this trait and registering with the core's integration registry. The UI layer adds a settings panel.
