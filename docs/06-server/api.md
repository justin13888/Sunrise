---
status: accepted
---

# Server API

Two surfaces: the **sync protocol** (an SSE stream downstream and typed `POST`s upstream, per [ADR-0023](../11-adr/0023-sse-sync-transport.md); payloads spec'd in [`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md)) and a small **REST API** for account lifecycle and blobs.

> **This document is no longer authoritative about shapes.**
> [`schemas/generated/openapi.v1.json`](../../schemas/generated/openapi.v1.json) is, per
> [ADR-0021](../11-adr/0021-kynos-openapi-server.md). It is generated from the
> handlers, committed, and checked against them by
> `the_committed_description_is_current` — so where this page and the
> description disagree, the description is right and this page is stale.
>
> What is kept here is what a schema cannot carry: why an operation exists,
> which failures are retryable, what order the two-phase commit runs in, and
> the byte layouts the wire depends on.
>
> **Implementation status.** Every operation below is served, by
> `crates/sunrise-server/src/api/`. `routes/`, the hand-written `axum` routing
> earlier revisions described, is gone; so is the `/sync` WebSocket, replaced by
> the SSE surface [ADR-0023](../11-adr/0023-sse-sync-transport.md) specifies.
> Routes this document specifies that **no handler serves** are still marked
> **NOT IMPLEMENTED** in place.

## REST endpoints

All endpoints are HTTPS. Authenticated requests carry a standard OIDC access token in `Authorization: Bearer <jwt>` plus an `X-Sunrise-Device: <dev_id>` header (see [`auth.md`](./auth.md)). Sign-up, login, password reset, MFA, and email change are all handled by the configured OIDC issuer — they have no Sunrise REST endpoints.

### Device binding (per-request)

The `X-Sunrise-Device` header carries the `device_id` (Crockford base32 of 16 bytes) on every authenticated request. Every authenticated request is also accompanied by an `X-Sunrise-Device-Sig` header containing an Ed25519 detached signature over the method, the target, the `Date` header, and a hash of the request body's **canonical form**. The server validates that:

1. `device_id` exists in the account's device set and is not revoked.
2. `X-Sunrise-Device-Sig` verifies under the device's signing key.
3. The OIDC token's `https://sunrise.app/device_id` claim (URI-namespaced per RFC 7519 §4.2; issued by the IdP from the client's `claims` parameter) matches `X-Sunrise-Device` (defense in depth).

The mode is `header_sig_v2` ([ADR-0022](../11-adr/0022-device-signature-canonical-json.md)),
and its byte layout is specified in [`auth.md`](./auth.md) §Device binding
rather than left to an implementation. Two live qualifications:

- The binding is **optional by default**. `[auth] require_device_sig` defaults
  to `false`, so a request with no `X-Sunrise-Device` is accepted with no
  device resolved. A binding that *is* present is always verified in full,
  whatever the flag says — a signature that fails verification is never an
  ignored header. `POST /accounts` and `POST /devices` take a bootstrap
  exemption: a device cannot sign before it exists.
- Verification happens **after** the body is parsed, because what is signed is
  the request's canonical form rather than the octets that carried it. A
  malformed body therefore fails as a `400` before it can fail as a `401`, and
  that ordering is observable.

Clients probe via `GET /api/v1/meta`, which returns `device_binding_mode`
(`"header_sig_v2"`) and `device_binding_required` (the flag's value). v1
signatures are not accepted: nothing was deployed under the old construction,
so there is no window to support both, and supporting both would mean retaining
the raw-body access ADR-0022 exists to remove.

Implemented in `sunrise-http-sig` — shared, so the relay and the generated
client cannot disagree about it — and reached through `api/signed.rs`, whose
`Signed<T>` extractor parses and verifies in one step.

### Account

| Method | Path | Body | Returns | Status |
|---|---|---|---|---|
| POST | `/api/v1/accounts` | `AccountCreateRequest` = `{ email, identity_signing_pub, identity_dh_pub, recovery_blob, terms_at_ms }` | `201` + `AccountInfo` | implemented |
| GET | `/api/v1/accounts/me` | — | `AccountInfo` | implemented |
| PUT | `/api/v1/accounts/me/recovery_blob` | `{ recovery_blob }` | 204 | **NOT IMPLEMENTED** |
| GET | `/api/v1/accounts/me/recovery_blob` | — | `{ recovery_blob }` (opaque ciphertext; useless without the offline recovery code) | **NOT IMPLEMENTED** |
| POST | `/api/v1/accounts/me/delete/initiate` | — | 202; issues a single-use confirmation token (32 bytes Crockford base32, 52 chars, TTL 15 minutes) which the OIDC issuer relays to the user's verified email | **NOT IMPLEMENTED** |
| DELETE | `/api/v1/accounts/me` | `{ confirm_phrase }` | 202 (deletion within 30 days) | **NOT IMPLEMENTED** |

Both request and response types live in `sunrise-onboarding` (`account.rs`) and
are shared with the client:

```rust
AccountCreateRequest { email, identity_signing_pub, identity_dh_pub, recovery_blob, terms_at_ms }
AccountInfo          { identity_id, email, tier, device_count, created_at_ms }
```

`AccountInfo.tier` is always the string `"free"`: `resolve_account` sets it at
provisioning (`crates/sunrise-server/src/store.rs:268`) and nothing updates it or
reads it for a decision. It is retained for wire compatibility, not because it
means anything — there are no plan tiers in v1
([ADR-0027](../11-adr/0027-v1-self-host-first.md)).

`AccountInfo` carries a **device count**, not a `[DeviceMeta]` array, and names
the account `identity_id`; `GET /api/v1/devices` is where device metadata comes
from. `POST /accounts` neither chooses nor returns a new account id: the account
is already provisioned by the bearer's `(iss, sub)` (see [`auth.md`](./auth.md)
§per-request-auth), and the call attaches identity material to it. It is
idempotent by construction — `Store::set_identity` writes each field under
`COALESCE`, so a retry cannot swap the identity key.

**The recovery flow this document and [`auth.md`](./auth.md) describe is
unreachable.** `accounts.recovery_blob` is written by `POST /api/v1/accounts`
and is read back by nothing: no `SELECT` names the column, and `row_to_account`
does not project it. Neither the `PUT` nor the `GET` above exists, so a blob
that goes in cannot come out and a fresh device has no way to fetch the
ciphertext it must decrypt.
[ADR-0024](../11-adr/0024-key-hierarchy.md) depends on this being fixed: its
recovery path opens identity-sealed `key_envelope` ops with `ID_D_priv`, which
lives in exactly this blob.

The `recovery_blob` is stored opaquely. The server does not validate its internal format or version. The 10 MiB cap and the "signed by an active device" precondition below describe the unbuilt `PUT` route: on the live `POST /accounts` path the blob rides the bootstrap exemption, so it is bounded only by `[server] max_body_bytes` and needs no device signature. Recovery blobs follow the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 — the server stores opaque bytes and does not introspect.

#### Account errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `VALIDATION_*` | Malformed body, missing field. | No (fix request). |
| 401 | `AUTH_TOKEN_INVALID` | OIDC token signature/issuer/audience invalid. | No. |
| 401 | `AUTH_TOKEN_EXPIRED` | Token `exp` past. | After OIDC refresh. |
| 403 | `AUTH_SIGNUP_DISABLED` | First-login attempt for unknown account when `auth.allow_signup = false`. | No. |
| 403 | `ACCOUNT_DELETE_PHRASE_INVALID` | `confirm_phrase` token consumed, expired, or never issued. | After re-running `/initiate`. |
| 413 | `VALIDATION_PAYLOAD_TOO_LARGE` | `recovery_blob` body > 10 MiB. Body: `{ "code":"VALIDATION_PAYLOAD_TOO_LARGE", "max_bytes": 10485760 }`. | No (shrink). |

`ACCOUNT_DELETE_PHRASE_INVALID` and `VALIDATION_PAYLOAD_TOO_LARGE` are **not
implemented**: neither constant exists in `error.rs`'s `codes` module, and the
routes that would emit them do not exist. The codes `codes` actually defines are
`AUTH_TOKEN_INVALID`, `AUTH_TOKEN_EXPIRED`, `AUTH_SIGNUP_DISABLED`,
`AUTH_DEVICE_SIG_INVALID`, `AUTH_DEVICE_NOT_OWNER`, `DEVICE_NOT_FOUND`,
`VALIDATION_INVALID`, `BLOB_HASH_MISMATCH`, `BLOB_CHUNK_MISSING`,
`BLOB_NOT_FOUND`, `RELAY_STORAGE_UNAVAILABLE` and `FATAL_INTERNAL`. An
oversized body is rejected by kynos's own `middleware::limits::BodySize`, mounted
at `[server] max_body_bytes` (default 2 MiB), which answers `413` — and, unlike
the `tower-http` layer it replaces, contributes that response to every operation
it covers in the OpenAPI description, so the API no longer rejects payloads it
claims to accept. The limit is still well below the 10 MiB cap this section
assumes.

### Identity discovery (for sharing) — NOT IMPLEMENTED

Neither route below is mounted; there is no `api/identities.rs`. Sharing
depends on discovery, and [`../11-adr/0024-key-hierarchy.md`](../11-adr/0024-key-hierarchy.md)
makes the point in the other direction: under the derived-key model there is no
unit smaller than "everything" to grant, so discovery would have had nothing to
serve even if it existed.

| Method | Path | Body | Returns | Status |
|---|---|---|---|---|
| GET | `/api/v1/identities?handle=<h>` | — | `{ identity_id, identity_pub_s, identity_pub_d, fingerprint_qr }` | **NOT IMPLEMENTED** |
| GET | `/api/v1/identities/<idn_…>` | — | as above | **NOT IMPLEMENTED** |

The user's `identity_pub_*` is signed-by-self; a hostile server substituting keys is detectable via OOB fingerprint check. (ADR-0024 replaces self-signature with identity-signed device certs, which changes what this route would have to return.)

### Devices

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/devices` | `{ device_pub_s, device_pub_d?, device_cert?, vault_device_id?, nickname, platform, app_version? }` | `201` + `{ device_id }` |
| DELETE | `/api/v1/devices/<dev_id>` | — | 204 (revocation; only callable by another paired device) |
| DELETE | `/api/v1/devices/by-vault-id/<vault_dev_id>` | — | 204 (the same revocation, addressed by the id the vault knows the device by) |
| GET | `/api/v1/devices` | — | `[DeviceMeta]` |
| POST | `/api/v1/devices/push-tokens` | `PushRegistration` = `{ device_id, platform, token }` | `200` + `{ "registered": true }` |

The push-token route takes the device id **in the body**, not in the path: it is
`POST /api/v1/devices/push-tokens`, and `PushRegistration.device_id` (26
Crockford base-32 characters; the alias `device_id_hex` is still parsed) is
validated against the caller's own account by `Store::active_device` before the
token is stored. A device id the caller does not own — or one it owns but has
revoked — is a `403 AUTH_DEVICE_NOT_OWNER`, so ownership and revocation are
settled in one lookup. `platform` is the `PushPlatform` enum (`apns` / `fcm` /
`webpush`), not a free-form `provider` string.

`POST /api/v1/devices` validates `device_pub_s` as a parseable Ed25519 key,
`nickname` as 1..=64 bytes, `platform` against the six-value list in
`DeviceMeta`, and `vault_device_id` — when present — as 26 Crockford base-32
characters; each failure is a `400 VALIDATION_INVALID`. It rides the bootstrap
exemption, because the device it registers cannot have signed the request.

#### Two names for one device, and why the second exists

`device_id` is a ULID the relay mints at registration and returns to the device
that registered. Nothing carries it any further: a vault knows its **own**
relay id and no peer's, because a peer's arrives at the relay and never travels
back through the op stream.

What a vault holds for a peer is the peer's 16-byte **vault** device id — the id
a `device_revoke` op names, the id in every op envelope's cleartext routing
header. So a revocation, expressed by the only name the revoking device has,
had no route to send it to until `DELETE /api/v1/devices/by-vault-id/<id>`
existed ([#80](https://github.com/justin13888/Sunrise/issues/80)); the
device-id route was unreachable in principle rather than merely uncalled.

`vault_device_id` is optional on registration and nullable on `DeviceMeta`,
because it is additive to a shipped route. A device that registered without it
**cannot be revoked at the relay at all** — there is nothing on its row to
match, and the `by-vault-id` route answers `404` while that device goes on
authenticating. Clients treat that `404` as the warning it is rather than as
success.

What this tells the relay that it did not already know is a **binding**, not a
new identifier. The vault device id is already cleartext in every op envelope
and already stored in `relay_frame_heads`; the relay could already observe, at
upload time, that a signed request from device row *Y* carried envelopes headed
*X*. This makes that join durable and explicit rather than derivable, which is
the honest cost of making a revocation expressible, and it is confined to
material the relay handles already. See
[`../01-architecture/trust-and-server-role.md`](../01-architecture/trust-and-server-role.md).

#### Revocation is a soft delete

Revocation does **not** delete a row, and there is no Postgres. `Store::revoke_device`
runs one SQLite transaction that sets `revoked = 1, revoked_at_ms = <now>` on
the matching row and deletes that device's `push_tokens` entries. The device row
survives on purpose: `DeviceMeta.revoked` and `revoked_at_ms` are part of the
`GET /api/v1/devices` contract, so a listing has to be able to report a device
as revoked rather than as absent.

Three consequences are observable and tested:

- **A device cannot revoke itself, by either of its names.** `api/devices.rs`
  refuses a `DELETE` whose target is the caller's own bound device with
  `403 AUTH_DEVICE_NOT_OWNER`, and reads the same rule off `vault_device_id` on
  the `by-vault-id` route — a second name for one row is otherwise a way round
  the first rule.
  A device that can revoke itself is a device a thief can use to erase the
  evidence; signing out locally is a key wipe, not a server call.
- **The `by-vault-id` route revokes *every* active row carrying that vault id**,
  not one. A device re-registering is a second row rather than an error (it is
  what lets a client run the bootstrap on every start), and every one of those
  rows is the device the caller means; revoking one and leaving the rest would
  leave the device authenticating under a row its owner cannot name.
- **Revoking twice is a `404 DEVICE_NOT_FOUND`**, because the `UPDATE` is
  guarded by `revoked = 0` and a zero-row update maps to `StoreError::NotFound`.
  Revoking another account's device lands in the same place — the account id is
  in the `WHERE` clause, not in a comparison afterwards.
- **The effect is immediate for REST.** One connection behind a mutex means the
  update is visible to the very next request; `active_device` filters revoked
  rows, so the device stops authenticating at once. There is no 5 s
  connection-affinity window and no cache to expire — an earlier draft of this
  document asked clients to tolerate one, and nothing in the code produces it.
  For the `/sync` session, see [`auth.md`](./auth.md) §device-binding.

#### `DeviceMeta` schema

```cddl
DeviceMeta = {
  device_id:       tstr,        ; Crockford base32 of 16 bytes; the relay's own ULID
  vault_device_id: tstr / null, ; Crockford base32 of 16 bytes; the id the vault knows it by
  nickname:        tstr,        ; user-set; ≤ 64 bytes; default = platform default ("MacBook Air", etc.)
  platform:       "ios" / "android" / "macos" / "windows" / "linux" / "web",
  app_version:    tstr / null,  ; e.g. "1.4.2"; client-reported, absent by default
  created_at_ms:   uint,
  last_seen_at_ms: uint,
  revoked:         bool,
  revoked_at_ms:   uint / null,
}
```

#### Device errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `VALIDATION_*` | Bad CBOR, wrong field type, invalid `device_pub_*`. | No. |
| 401 | `AUTH_TOKEN_INVALID` / `AUTH_TOKEN_EXPIRED` | See Account errors. | See Account errors. |
| 403 | `AUTH_DEVICE_NOT_OWNER` | Caller is not a paired device of the account; the `DELETE` target is the caller itself; or a push registration names a device the account does not actively own. | No. |
| 401 | `AUTH_DEVICE_SIG_INVALID` | The caller **is** an active device of this account and its `header_sig_v2` binding still did not check out: an unverifiable `X-Sunrise-Device-Sig`, no `Date` header alongside it, or a `Date` outside ±300 s. Also returned, before any device lookup, when `require_device_sig` is set and the caller did not present a **complete** binding — `X-Sunrise-Device` and `X-Sunrise-Device-Sig` are read together, so either one missing takes this path, not only both — which `GET /meta`'s `device_binding_required` already advertises. | No — re-sign with a correct clock. Never refresh the bearer; it was not the problem. |
| 401 | `AUTH_TOKEN_INVALID` | `X-Sunrise-Device` names a device that is **not** an active row on this account, or the token's own `device_id` claim disagrees with it. Deliberately the same answer a bad bearer gets: a finer code here would tell an unauthenticated caller which devices an account has. | No. |
| 404 | `DEVICE_NOT_FOUND` | `<dev_id>` does not match any **active** device on this account — including a device already revoked. For `by-vault-id`, also a device that registered before it sent a `vault_device_id`, which is **still accepted by this relay**: the `404` is not a statement that the device is refused. | No. |

### Blobs

Two-phase commit. The blob id is not minted at `init` — it cannot be, because
it is the **content address** of bytes the server has not seen yet.

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/blobs/init` | `{ stream_id, chunk_count, size_bytes }` | `{ upload_id, chunk_urls: […] }` |
| PUT | `/api/v1/blobs/<upload_id>/<i>` | raw ciphertext chunk | 204 |
| POST | `/api/v1/blobs/finalize` | `{ upload_id, content_hash, chunk_hashes }` | `{ blob_id, size_bytes, chunk_count }` |
| GET | `/api/v1/blobs/<blob_id>` | — | the concatenated ciphertext, `application/octet-stream` |

Every route is authenticated and device-bound like the rest of the REST API —
`api/blobs.rs` takes a signed extractor on all four — `Signed` for the two JSON bodies, `SignedBinary` for the raw chunk `PUT`, `SignedParts` for the `GET` — so the bearer, the binding and the body are verified before a handler sees the value. `init`
answers `200`, a chunk `PUT` answers `204`, and `finalize` answers `200`.
Alongside the hash checks, `init` and `finalize` enforce
`1 <= chunk_count <= 4096`, `1 <= chunk_bytes <= 1 MiB`, and a
100 MB per-attachment ceiling from
[`../02-domain/attachments.md`](../02-domain/attachments.md) §Size policy.

`chunk_hashes` is an array of `BLAKE3(ciphertext_chunk)` as 64 lowercase hex
characters, one per chunk, in order; its length — not `init`'s advisory
`chunk_count` — is what `finalize` treats as the chunk count. `content_hash`
is `BLAKE3` of the concatenation, same encoding. Finalize re-hashes what is
actually on disk and compares: the client's hashes are a claim, and this is the
check. No `plaintext_hash` exists server-side; that's a client-only construct.

`blob_id` is `blb_` followed by the first 16 bytes of `content_hash` in
lowercase hex. Identical ciphertext therefore converges on one stored copy.

Blob payloads are stored as opaque bytes — the magic prefix from
[`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3
is enforced by clients, not the server.

Storage is rooted **per account**. Content addressing across accounts would be
a cross-tenant read primitive — one account naming another's blob by its hash
— so a `GET` for a blob this account has not committed is a 404 whether or not
some other account holds those bytes.

Self-host writes chunks to the local filesystem and returns relative
`chunk_urls` of the form `/api/v1/blobs/<upload_id>/<i>`, which the `PUT`
handler serves — this is the built path, and `upload_id` is `up_` followed by 16
random bytes in lowercase hex. A managed deployment would substitute presigned
URLs (upload URLs expiring 1 hour after issuance, download URLs 24 hours);
**no presigned-URL path is implemented**, and there is no object store to sign
against.

The commit order is load-bearing and is not the outbox pattern
[`relay-and-blob-storage.md`](./relay-and-blob-storage.md) describes: `finalize`
re-reads every pending chunk, checks each against the client's per-chunk BLAKE3
and the concatenation against `content_hash`, writes the chunks under the
content address, and writes the **manifest last**. A reader that finds no
manifest sees no blob, so a crash mid-commit leaves an invisible partial rather
than a short read.

**No client calls any of the four.** Outside the handlers, the only mentions in
the workspace are this document, the generator's two omit rules
(`crates/sunrise-relay-client/build.rs:42`, `:46`) and the log field catalogue.
They are the API for a client that has not landed rather than dead weight:
`Core::attach_file` seals an attachment's chunks into the *local* vault's blob
store and stops there, so until an uploader drives these routes an attachment is
readable only on the device that made it
([`../02-domain/attachments.md`](../02-domain/attachments.md) §Lazy fetch,
[#176](https://github.com/justin13888/Sunrise/issues/176)). Two of the four are
also absent from the generated relay
client — the raw-binary chunk `PUT` and the blob `GET` — because kynos and
spargen disagree about how OpenAPI 3.1 describes a raw binary body; `build.rs`
records the disagreement, so whoever writes the caller hand-writes those two and
generates `init` and `finalize`.

**Not yet implemented:** `DELETE /api/v1/blobs/<blob_id>`. Blob deletion is not
an immediate erase — [`../02-domain/attachments.md`](../02-domain/attachments.md)
§Deletion makes it a tombstone plus a device-cursor quorum and a 30-day grace
period — so it lands with the GC slice rather than as a bare unlink.

#### Blob errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `VALIDATION_INVALID` | Malformed body, out-of-range `chunk_count`, a chunk outside 1..=1 MiB, or a path segment that is not a well-formed `up_`/`blb_` id. | No (fix request). |
| 400 | `BLOB_HASH_MISMATCH` | A chunk's ciphertext BLAKE3, or the concatenation's, disagrees with the supplied hash. | No (re-upload). |
| 401 | `AUTH_TOKEN_INVALID` | Missing or unverifiable bearer. | After OIDC refresh. |
| 409 | `BLOB_CHUNK_MISSING` | `finalize` names a chunk that was never uploaded. | After uploading it. |
| 404 | `BLOB_NOT_FOUND` | No committed blob under that id **for this account**. | No. |
| 413 | `VALIDATION_PAYLOAD_TOO_LARGE` | Body over `max_body_bytes`. | No (shrink). |

### Sharing — NOT IMPLEMENTED

No `api/shares.rs` exists, no `shares` table is in `store.rs`'s schema, and
`/api/v1/shares/*` is not mounted. Sharing is blocked upstream of the API:
[ADR-0024](../11-adr/0024-key-hierarchy.md) records that under the implemented
derived-key model there is no key unit smaller than the whole vault to grant,
so the `share_envelope` this section posts has nothing to carry until that ADR
lands.

| Method | Path | Body | Returns | Status |
|---|---|---|---|---|
| POST | `/api/v1/shares` | `{ stream_id, recipient_idn, share_envelope }` | `{ share_id }` | **NOT IMPLEMENTED** |
| DELETE | `/api/v1/shares/<share_id>` | — | 204 | **NOT IMPLEMENTED** |
| GET | `/api/v1/shares/incoming` | — | `[{ share_id, granter_idn, share_envelope, created_at }]` | **NOT IMPLEMENTED** |

`POST /api/v1/shares` is idempotent on `(stream_id, recipient_idn)`:
- If a `pending` or `accepted` grant already exists, return `200 OK` with the existing `share_id`.
- If only `revoked` or `declined` grants exist, create a new grant and return `201 Created`.

The `share_envelope` is opaque to the server.

### Health / meta

| Method | Path | Body | Returns | Status |
|---|---|---|---|---|
| GET | `/api/v1/meta` | — | `MetaResponse` (below) | implemented |
| GET | `/api/v1/health` | — | `200 {"status":"ok"}` | implemented |
| GET | `/api/v1/health?deep=1` | — | 200 if every backing dependency is healthy; 503 otherwise | **NOT IMPLEMENTED** |

`MetaResponse` (`api/meta.rs`) is:

```
{ server_app_v, wire_proto_supported: [uint], crypto_suite_supported: [uint],
  doc_schema_floor, capabilities, oidc_issuer, oidc_client_id,
  device_binding_mode, device_binding_required }
```

Version fields are **arrays** of supported versions sourced from
`sunrise_cbor::version`, not the single `protocol_version` an earlier draft
named, and there is no `server_version`, `max_op_size` or `max_blob_size` field.
`/meta` is deliberately unauthenticated: a client must be able to read the
issuer before it has a token.

`GET /api/v1/health` takes no parameters and always answers `200
{"status":"ok"}` — the handler consults nothing, so it is a liveness probe and
not a readiness one. The deep check below is specified and unbuilt; a `?deep=1`
query is ignored, which means an operator wiring it as a readiness probe today
gets an unconditional `200`.

The deep readiness check would return `200 {"ok":true,"checks":{...}}` only if all of:

1. `SELECT 1` from primary DB within 2 s.
2. Object store HEAD on `_health/probe` within 5 s.
3. Local disk free ratio > 5%.

Otherwise it returns `503` with a body listing the failed checks.

`oidc_issuer` and `oidc_client_id` let an unauthenticated client bootstrap the OIDC flow without static configuration. Both are `null` in single-tenant self-host mode, where no issuer is configured.

## Errors

JSON body:

The envelope `ApiError` actually renders carries two members and no
`retry_after_seconds`:

```json
{
    "error": {
        "code": "VALIDATION_INVALID",
        "message": "…"         // safe-for-logs
    }
}
```

Codes are **stable** (clients map them to translated strings). New codes can be added; clients see unknown codes as a generic error. Messages never quote a token, a key, or a subject: a JWKS transport failure and a forged signature both render as the same opaque `401`, and a SQLite error renders as `500 FATAL_INTERNAL` with the message `"internal error"`.

### Quota responses — REMOVED FROM v1

There are none, and there is no longer a code to build them from. ADR-0027
takes per-account quotas out of v1; `AUTH_QUOTA_EXCEEDED` and
`STORAGE_QUOTA_EXCEEDED` are gone from `sunrise-error`'s `codes.toml` and their
ids (203, 300) are burned. Nothing counts storage, ops, or devices against a
plan, and the `429`/`202` pair this section used to specify — a hard cap over
110% and a soft warning inside the grace window — is described in
[`billing.md`](./billing.md) as the shape a future quota surface would take,
not as anything a client can receive.

## Rate limits — NOT IMPLEMENTED

There is no rate-limiting middleware anywhere in `crates/sunrise-server`. The
only middleware `api/mod.rs` mounts is kynos's `Cors` and `BodySize`, plus the
`RequestLog` observer in `api/observe.rs`; nothing counts requests per IP, per
account, or per token. An unauthenticated caller can hit
`/api/v1/meta`, `/api/v1/health` and `/metrics` at whatever rate the socket
allows, and the OIDC verifier's JWKS cache is the only thing bounding work on
`401`s.

The target, when it is built:

Per-IP: 60 RPM unauthenticated, 600 RPM authenticated.

Per-account limiting is **out of v1** — it presupposes per-account accounting
that does not exist ([ADR-0027](../11-adr/0027-v1-self-host-first.md) clause 2).
The design of record for it is
[`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md),
which is `proposed`.

## Why so few endpoints?

Most "API" calls in a typical app — CRUD on tasks — are *not* server API calls in Sunrise. They're local commands that produce ops, which sync as opaque envelopes over `POST /sync/ops` and back down the event stream. The REST surface is intentionally minimal.

## Endpoint availability by deployment

Only the single-binary column exists today; the other two describe deployments
that have not been built.

| Endpoint | Managed | Self-host (default) | Self-host (single-binary) |
|---|---|---|---|
| `/api/v1/accounts` (POST), `/api/v1/accounts/me` (GET) | yes | yes | **yes — built** |
| `/api/v1/accounts/me/recovery_blob`, `/api/v1/accounts/me/delete/*` | yes | yes | **no route** |
| `/api/v1/identities` (discovery) | yes | yes | **no route** |
| `/api/v1/devices`, `/api/v1/devices/<id>` | yes | yes | **yes — built** |
| `/api/v1/devices/push-tokens` | yes | optional (operator's APNs/FCM creds) | **route built; no delivery path** |
| `/api/v1/blobs/*` | yes (S3-backed) | yes (S3-backed) | **yes — local-disk-backed** |
| `/api/v1/shares/*` | yes | yes | **no route** |
| `/api/v1/meta`, `/api/v1/health` | yes | yes | **yes — built** |
| `/metrics` (router root, not under `/api/v1`) | yes | yes | **yes — built, unauthenticated** |

Clients query `/api/v1/meta`'s `capabilities` field on connect and adapt UI affordances accordingly (e.g. hide "enable push notifications" if push is unavailable). Today that field is the constant `REQUIRED_SERVER_BITS`, so it reports the required set rather than what this deployment can actually do.
