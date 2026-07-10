---
status: accepted
---

# Integrations — Overview

External integrations connect Sunrise to systems we *don't* build. Integrations run **on-device**: tokens live in the encrypted vault; the server never holds third-party credentials.

| Integration | Direction | Spec |
|---|---|---|
| Google Calendar | Bidirectional | [`google-calendar.md`](./google-calendar.md) |
| iCalendar (.ics) | Import / export | [`icalendar.md`](./icalendar.md) |

v1 ships only these two. CalDAV, outbound webhooks, and inbound email-to-Sunrise are explicit non-goals — see [`../00-product/non-goals.md`](../00-product/non-goals.md).

## Design principles

1. **On-device.** Integration credentials and run-loop live in the user's vault and run on their devices. The server is not a credentialed proxy.
2. **Per-Stream opt-in.** Integrations are turned on per Stream, not globally. A user can sync `Work` to Google Calendar and not `Personal`.
3. **No silent surprise edits.** Importing a calendar event creates a *read-only* Block. Mutations to external systems require explicit "push from this Stream" toggle.
4. **Token rotation** handled per the third party's API; tokens stored encrypted at rest in the vault.
5. **Failures are visible.** A failing integration shows an in-app banner, not a silent log line.

## Multi-device coordination

Multiple devices running the same integration would call the API multiple times. We elect a **primary integration runner** per integration using the same rule as the compactor election (see [`../04-storage/compaction.md`](../04-storage/compaction.md)): the smallest `device_id` among devices with a heartbeat in the last 24 h. Election re-runs per-integration per-day. Other devices defer via a periodic heartbeat in a control op.

## Token storage

Tokens stored as fields in the integration config inside the Stream entity (CRDT-synced, encrypted), reachable only after vault unlock. They are never in plaintext in logs or in transit beyond the third-party's TLS endpoint.

### External revocation detection

On any third-party API call returning `401` *after* a recent successful refresh, the integration sets the CRDT field `needs_reauth = true`, surfaces an in-app banner, zeroizes the old tokens, and stops scheduling sync runs. The user clicks "Reauthenticate" and walks through the standard OAuth re-consent flow.

## Disabling

Disabling an integration shows a modal containing a checkbox **"Also delete entities imported from <provider>"**, unchecked by default:

- Flushes any in-flight sync.
- Marks imported entities as orphaned (read-only with a "source disconnected" banner) — the default behavior when the box is unchecked.
- If the box is checked, the disable op carries `delete_imports: true`; all clients delete the matching entities on observation.

## Building new integrations

The integration interface is a Rust trait inside the core:

```rust
trait Integration: Send + Sync {
    fn id(&self) -> IntegrationKind;
    async fn sync(&self, ctx: &IntegrationCtx) -> Result<()>;
    async fn handle_event(&self, evt: DomainEvent) -> Result<()>;
    async fn revoke(&self) -> Result<()>;
}

pub struct IntegrationCtx<'a> {
    pub stream_id:  StreamId,
    pub device_id:  DeviceId,
    pub keys:       &'a StreamKeyAccess,    // encrypt/decrypt for this stream
    pub config:     &'a IntegrationConfig,  // { tokens, settings }
    pub error_sink: &'a dyn ErrorSink,      // banner alerts to UI
    pub log:        &'a tracing::Span,
    pub clock:      &'a dyn Clock,
}
```

`Clock` and `ErrorSink` are concrete traits — passing them as `&dyn` enables deterministic testing (a fake clock and a captured error stream).

New integrations are added by implementing this trait and registering with the core's integration registry. The UI layer adds a settings panel.
