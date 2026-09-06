---
status: accepted
---

# Integrations — Overview

External integrations connect Sunrise to systems we *don't* build. Integrations run **on-device**: tokens live in the encrypted vault; the server never holds third-party credentials.

| Integration | Direction | v1 status | Spec |
|---|---|---|---|
| Google Calendar | Bidirectional *(target)* | **deferred** — the provider is implemented and tested as **read-only import**, with no consumer, no cursor storage and no UI ([ADR-0020](../11-adr/0020-v1-must-demotions.md), [#4](https://github.com/justin13888/Sunrise/issues/4)) | [`google-calendar.md`](./google-calendar.md) |
| iCalendar (.ics) | Import / export | **live on both shipping clients**, a narrow subset — `sunrise ical import` / `export`, and the macOS File menu over the seam's `import_ical` / `export_ical` | [`icalendar.md`](./icalendar.md) |

**`IntegrationProvider` has no implementor.** Neither integration implements
the trait `crates/sunrise-integrations/src/lib.rs` declares — not the deferred
GCal path, and not the live iCal one, which is driven directly by
`ical_vault::{import, export}` from the CLI and the UniFFI seam. The crate's own
module docs say "each integration runs through the [`IntegrationProvider`]
trait so the core can drive runs uniformly"; nothing does. Treat it as intended
shape, not as a seam anything runs through.

v1 ships only these two. CalDAV, outbound webhooks, and inbound email-to-Sunrise are explicit non-goals — see [`../00-product/non-goals.md`](../00-product/non-goals.md).

## Design principles

1. **On-device.** Integration credentials and run-loop live in the user's vault and run on their devices. The server is not a credentialed proxy.
2. **Per-Stream opt-in.** Integrations are turned on per Stream, not globally. A user can sync `Work` to Google Calendar and not `Personal`.
3. **No silent surprise edits.** Importing a calendar event creates a *read-only* Block. Mutations to external systems require explicit "push from this Stream" toggle.
4. **Token rotation** handled per the third party's API; tokens stored encrypted at rest in the vault.
5. **Failures are visible.** A failing integration shows an in-app banner, not a silent log line.

## Multi-device coordination

> **Not implemented, and deliberately deferred.** There is no election, no
> control op carrying a heartbeat, and no runner to elect between:
> [ADR-0025](../11-adr/0025-integration-account-entity.md) explicitly excludes
> "the primary-runner election that `overview.md` specifies for deciding which
> device does the fetching", on the grounds that a runner election with nothing
> to elect over is premature. The section below is the target.

Multiple devices running the same integration would call the API multiple times. We elect a **primary integration runner** per integration using the same rule as the compactor election (see [`../04-storage/compaction.md`](../04-storage/compaction.md)): the smallest `device_id` among devices with a heartbeat in the last 24 h. Election re-runs per-integration per-day. Other devices defer via a periodic heartbeat in a control op.

## Token storage

**Credentials live in an `IntegrationAccount` entity, not in a Stream field.**
[ADR-0025](../11-adr/0025-integration-account-entity.md) is the decision, and it
replaces the claim an earlier revision of this section made — that tokens are
"stored as fields in the integration config inside the Stream entity". That
field does not exist: the `integrations` map was deleted, nothing ever
serialized it, and [`../02-domain/streams.md`](../02-domain/streams.md) says so.

Three rules follow, and each is load-bearing:

1. **Account-scoped, not Stream-scoped.** One Google authorization backs many
   Streams, so hanging it off `Stream` gives the wrong granularity for
   revocation — rotating one Stream's key would orphan an unrelated calendar
   authorization.
2. **Only the durable half is synced.** The refresh token and the account's
   identity go in the entity; **the short-lived access token stays
   device-local and is never written to an op.** That is not only a secrecy
   argument — it is a write-amplification one. Ops are full-state under
   entity-level LWW ([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)), so
   syncing a value that changes every hour would rewrite the whole entity every
   hour, on every device, forever. `gcal.rs`'s `apply_refresh` already
   implements the matching rule that an omitted `refresh_token` must not erase
   the durable one.
3. **The secret rides as an ordinary entity field**, which is sufficient only
   because ops are sealed under per-Stream keys *and* because
   [ADR-0024](../11-adr/0024-key-hierarchy.md) makes epoch rotation real —
   keys are random per `(stream, epoch)` and wrapped, rather than derived from
   the vault root, so a key can be rotated at all. Under the derived-key model
   every device that ever knew the root could derive every key forever, which
   is why ADR-0025 depends on ADR-0024 rather than shipping beside it.

   **A revoked device can still read credentials written after its
   revocation.** Rotation is expressible now; it is not yet enforced against a
   revoked device, because every device holds `ID_D_priv` and every epoch is
   also sealed to the identity — see
   [`../03-crypto/key-rotation.md`](../03-crypto/key-rotation.md) §Revocation.
   Any integration whose threat model needs "revoking the laptop cuts off its
   access to the connected account" is waiting on
   [#76](https://github.com/justin13888/Sunrise/issues/76), not on this ADR.

"Log in once" is what the entity buys: authorizing on one device writes the
durable credential into the vault, and every paired device receives it through
normal sync with no second authorization. The relay sees ciphertext, as it does
for every other op. Credentials are reachable only after vault unlock, and are
never in plaintext in logs or in transit beyond the third party's TLS endpoint.

### External revocation detection

On any third-party API call returning `401` *after* a recent successful refresh, the integration sets the field `needs_reauth = true`, surfaces an in-app banner, zeroizes the old tokens, and stops scheduling sync runs. The user clicks "Reauthenticate" and walks through the standard OAuth re-consent flow.

## Disabling

Disabling an integration shows a modal containing a checkbox **"Also delete entities imported from <provider>"**, unchecked by default:

- Flushes any in-flight sync.
- Marks imported entities as orphaned (read-only with a "source disconnected" banner) — the default behavior when the box is unchecked.
- If the box is checked, the disable op carries `delete_imports: true`; all clients delete the matching entities on observation.

## Building new integrations

The integration interface is a Rust trait, and it lives in
`crates/sunrise-integrations/src/lib.rs` rather than in the core. What is
declared today is narrower than earlier revisions of this section described:

```rust
#[async_trait]
pub trait IntegrationProvider: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> IntegrationKind;
    async fn run(&self) -> Result<RunSummary, IntegrationError>;
}
```

There is no `IntegrationCtx`, no `IntegrationConfig` and no registry: an earlier
revision specified a context struct carrying `{ tokens, settings }` off the
Stream, and that is the same deleted field §Token storage corrects. A provider
that needs credentials reads them from the `IntegrationAccount` entity; a
provider that needs a transport takes one by injection, which is what `gcal.rs`
already does and what keeps it testable with no network.

New integrations implement this trait. The UI layer adds a settings panel.
