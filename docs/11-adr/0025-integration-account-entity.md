# 0025 — Integration credentials are a synced entity, not a Stream field

**Status:** accepted

**Depends on** [ADR-0024](./0024-key-hierarchy.md).
**Amends** [`docs/09-integrations/overview.md`](../09-integrations/overview.md),
[`google-calendar.md`](../09-integrations/google-calendar.md) and
[`docs/02-domain/time-blocks.md`](../02-domain/time-blocks.md).
**Bumps** `DOC_SCHEMA_V`.

## Context

"Calendars should have state that syncs across clients, so you log in once, with
secrets passed end-to-end and never leaking to the server." None of the pieces
for that exist, and the documents disagree about whether they ever did.

* **There is no `Calendar` or `IntegrationAccount` entity.** `EntityKind` has
  twelve variants and none of them is one. The only calendar nouns in the domain
  are `Block` — a real, synced entity — and `gcal::CalendarEntry`, a deserializer
  for a Google API response that is never persisted.
* **`gcal.rs` has zero consumers.** 850 lines of PKCE exchange, refresh with the
  durable-refresh-token rule, and change detection that suppresses phantom
  deletes — all tested with injected transport, and reachable from nothing.
  `IntegrationProvider` has no implementor; not even the live iCal path uses it.
* **`gcal::Credentials` has no store.** It is a plain in-memory struct. No OAuth
  token is persisted anywhere, synced or local, because no flow is wired.
* **The documents contradict each other.** `09-integrations/overview.md` says
  tokens are "stored as fields in the integration config inside the Stream entity
  (synced and encrypted like any other entity field)". `02-domain/streams.md`
  says "**There is no `integrations` map**, and no `IntegrationKey` /
  `IntegrationConfig` type. Earlier revisions declared one; nothing has ever
  serialized it." The field was deleted; the sentence depending on it was not.
* **`google-calendar.md` puts the relay in the token path**, specifying that the
  authorization-code flow is "mediated server-side, so the client never sees
  `client_secret`", with the managed server holding the secret. That contradicts
  `overview.md`'s "The server is not a credentialed proxy" and "Tokens never
  leave the device", and it is the design error that prompted this work. It is
  inert only because nothing implements it.
* **`Block` cannot hold what a calendar event carries.** It has no `rrule`,
  `location`, `notes`, `source` or `external_id`. `time-blocks.md` lists them as
  "specified but not modelled", and the consequence is concrete: iCal import
  parses `RRULE`, `DESCRIPTION` and `LOCATION`, then reports them at the domain
  boundary because there is nowhere to put them. **A recurring event imports as a
  single occurrence.**

## Decision

**A new synced entity, `IntegrationAccount`, holds integration state and
credentials. This ADR adds the model and no network fetching of any kind.**

* **Entity, not a Stream field.** Credentials are account-scoped, not
  project-scoped: one Google account backs many Streams. Hanging them off
  `Stream` was what produced a map nothing ever serialized, and it gives the
  wrong granularity for revocation — rotating one Stream's key should not orphan
  an unrelated calendar authorization.
* **Secrets ride as ordinary entity fields**, which is sufficient because ops are
  already sealed under per-Stream keys. What makes rotating them *possible* at
  all is [ADR-0024](./0024-key-hierarchy.md): keys become random per
  `(stream, epoch)` and wrapped, where the derived-key model let every device
  that ever knew the vault root derive every key forever. That is why this ADR
  depends on that one rather than shipping beside it.

  **Revoking a device does cut off its access to a connected account**, for a
  device admitted by pairing. Every epoch is still sealed to the account
  identity so recovery can reach it, but `ID_D_priv` no longer travels in a
  `PairingPayload`, so a revoked device can open neither its own copy — it is
  excluded from the recipient list — nor the identity's. The exception is the
  device that *created* the account, which keeps `ID_D_priv` until the recovery
  blob exists. *(This paragraph twice said the opposite of the tree: first that
  revocation cut access when it did not, then that it did not once
  [#76](https://github.com/justin13888/Sunrise/issues/76) was fixed. It is worth
  re-reading against `03-crypto/key-rotation.md` §Revocation rather than
  trusted.)*
* **Only the durable half is synced.** The refresh token and the account's
  identity go in the entity; the short-lived access token stays device-local and
  is never written to an op. This is not only a secrecy argument — it is a
  write-amplification one. Ops are full-state under entity-level LWW, so syncing
  a value that changes every hour would rewrite the whole entity every hour, on
  every device, forever. `gcal.rs`'s `apply_refresh` already implements the
  matching rule that an omitted `refresh_token` must not erase the durable one.
* **"Log in once" is what the entity buys.** Authorizing on one device writes the
  durable credential into the vault; every paired device receives it through
  normal sync and needs no second authorization. The relay sees ciphertext, as it
  does for every other op.
* **`Block` gains `rrule`, `location`, `notes`, `source` and `external_id`**, so
  a recurring event stops flattening to one occurrence and a foreign id can be
  round-tripped back out. These are additive fields, so older readers preserve
  them through the `unknown` map rather than dropping them.
* **`google-calendar.md`'s server-mediated exchange is deleted**, not softened.
  The flow is on-device PKCE with a public client, exactly as `gcal.rs` already
  implements it and as `overview.md` already claims.

Explicitly **not** in this ADR: polling, fetching, subscription URLs, an
`EventSyncer` implementation, and the primary-runner election that
`overview.md` specifies for deciding which device does the fetching. The model
lands first so that the thing that eventually fetches has somewhere to keep its
state; a runner election with nothing to elect over is premature.

## Alternatives considered

| Option | Why not |
|---|---|
| **Revive the `integrations` map on `Stream`** | What the documents still describe. Wrong scope — one authorization serves many Streams — and it was already deleted once for having no serializer. It also ties credential lifetime to Stream-key rotation, so rotating a Stream would orphan an unrelated calendar. |
| **Device-local credentials, authorize per device** | No new entity, no synced secret, and no "log in once" — which is the requirement. It also makes every new device a fresh OAuth consent. |
| **Sync the access token too** | Simpler client code, and it writes a full-state op per token refresh per device under an LWW merge model. The durable/ephemeral split costs one field and removes the churn. |
| **Server-mediated OAuth, per `google-calendar.md`** | Keeps the client from holding a `client_secret`, and puts the relay in possession of every user's calendar tokens — the precise property the whole architecture exists to avoid. A public PKCE client has no secret to protect, so the premise does not hold either. |
| **Ship the model together with fetching** | One coherent feature, and it needs a Google OAuth client ID that does not exist, so the integration half could not be tested end to end. [ADR-0020](./0020-v1-must-demotions.md) already deferred GCal from the v1 MUST set; this keeps that deferral while unblocking the schema. |

## Consequences

* A new `InnerOp` family, so the same pre-1.0 vocabulary break as
  [ADR-0024](./0024-key-hierarchy.md) applies, under the same
  [ADR-0018](./0018-storage-baseline-reset.md) reasoning.
* `DOC_SCHEMA_V` bumps for the `Block` fields and the new entity.
  `DOC_SCHEMA_FLOOR` does not move: the `Block` additions are readable by older
  builds through the `unknown` map.
* The iCal importer keeps its `(source, uid)` → Block id hash. Adding an
  `external_id` field does **not** change how dedup works — the id *is* the key,
  which is what makes re-import idempotent with no side table. `external_id`
  exists to round-trip a foreign id outward, and `time-blocks.md` must keep
  saying so or someone will wire dedup to it.
* `gcal.rs` stops being unreachable in principle, while staying unreachable in
  fact until a later PR wires a provider. That is a smaller and more honest gap
  than the current one.
* The credential field is a decryption target of real value, so it is the first
  thing in the vault whose exposure is worth more than the user's own data. It
  should be the worked example in `threat-model.md` for what an epoch rotation
  actually protects.
