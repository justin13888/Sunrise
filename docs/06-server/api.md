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

- The binding is **required by default on a relay with an OIDC issuer**:
  `[auth] require_device_sig`, left unset, is on wherever an issuer is
  configured, and a request with no complete binding is refused with
  `401 AUTH_DEVICE_SIG_INVALID` before any device lookup. It is off on the
  single-tenant self-host verifier, which has no devices to tell apart and
  refuses the flag outright; there, and on a multi-tenant relay whose
  operator sets `require_device_sig = false` (which logs
  `srv.start.device_sig_optional`), a request with no `X-Sunrise-Device` is
  accepted with no device resolved. `GET /meta` publishes the policy in force
  as `device_binding_required`. A binding that *is* present is always verified in full,
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
| GET | `/api/v1/accounts/me/recovery_blob` | — | `{ recovery_blob }` (opaque ciphertext; useless without the offline recovery code) | implemented, behind an OIDC step-up |
| POST | `/api/v1/accounts/me/delete/initiate` | — | `202 { confirm_phrase, expires_at_ms }`: a single-use confirmation phrase (32 bytes Crockford base32, 52 chars, TTL 15 minutes) | implemented, behind an OIDC step-up |
| DELETE | `/api/v1/accounts/me` | `{ confirm_phrase }` | `202 { requested_at_ms, erase_after_ms }`; erased by the first maintenance pass after `[storage] account_delete_grace_days` (default 30) | implemented, behind an OIDC step-up |

Both request and response types live in `sunrise-onboarding` (`account.rs`) and
are shared with the client:

```rust
AccountCreateRequest { email, identity_signing_pub, identity_dh_pub, recovery_blob, terms_at_ms }
AccountInfo          { identity_id, email, tier, device_count, created_at_ms }
```

`AccountInfo.tier` is always the string `"free"`: `resolve_account` sets it at
provisioning (`crates/sunrise-server/src/store/accounts.rs:107`) and nothing
updates it or reads it for a decision. It is retained for wire compatibility,
not because it means anything — there are no plan tiers
([ADR-0027](../11-adr/0027-v1-self-host-first.md)).

`AccountInfo` carries a **device count**, not a `[DeviceMeta]` array, and names
the account `identity_id`; `GET /api/v1/devices` is where device metadata comes
from. `GET /api/v1/accounts/me` is implemented and, like that route, uncalled:
`bootstrap` issues the `POST` and never reads the account back, so the first
surface that shows an account is its first caller. `POST /accounts` neither chooses nor returns a new account id: the account
is already provisioned by the bearer's `(iss, sub)` (see [`auth.md`](./auth.md)
§per-request-auth), and the call attaches identity material to it. It is
idempotent by construction — `Store::set_identity` writes each field under
`COALESCE`, so a retry cannot swap the identity key. The one exception is
`recovery_blob`, which is write-once and *reports* the conflict rather than
coalescing it; see below.

**The `GET` is built; the `PUT` is not.** `Store::recovery_blob` reads the
column and `getRecoveryBlob` serves it, so a fresh device can fetch the
ciphertext it must decrypt — which is what
[ADR-0024](../11-adr/0024-key-hierarchy.md)'s recovery path depends on, since it
opens identity-sealed `key_envelope` ops with the `ID_D_priv` this blob carries.

Two things gate it.

* **An OIDC step-up.** The bearer must carry an `auth_time` within
  `[auth] recovery_max_auth_age_secs` (default 300), and the `acr`/`amr` values
  an operator listed in `recovery_acr_values` / `recovery_amr_values`, if any.
  An ordinary bearer is refused with `403 AUTH_STEP_UP_REQUIRED` — see
  [`auth.md`](./auth.md) §Recovery for why this replaces the email OTP earlier
  revisions specified, and why it is a `403` rather than a `401`. The
  single-tenant self-host verifier is exempt: it has no IdP to ask.
* **Write-once.** `POST /accounts` refuses a `recovery_blob` that differs from
  one already stored, with `409 RECOVERY_BLOB_EXISTS`. It used to accept it,
  answer `201` and discard it under the `COALESCE` below — after which a client
  had shown its user a recovery code for a blob the relay does not hold.
  Re-sending identical bytes still succeeds, so the dropped-response retry is
  unchanged. Rotation therefore has no route yet; it needs the `PUT`.

The `recovery_blob` is stored opaquely. The server does not validate its internal format or version. The 10 MiB cap and the "signed by an active device" precondition below describe the unbuilt `PUT` route: on the live `POST /accounts` path the blob rides the bootstrap exemption, so it is bounded only by `[server] max_body_bytes` and needs no device signature. Recovery blobs follow the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 — the server stores opaque bytes and does not introspect.

#### Account deletion

Two calls, both bearer + device signature and both behind the same OIDC step-up
as the recovery blob (`auth.md` §Recovery): deletion is the one act more final
than reading the blob, and a stolen session is exactly an ordinary bearer.

1. `POST /accounts/me/delete/initiate` mints a phrase, holds its BLAKE3 hash with
   a 15-minute expiry, and **returns the phrase to the caller**. Initiating
   again replaces it. Earlier revisions had the OIDC issuer relay the phrase to
   the user's verified email. No issuer offers that, and
   [`non-goals.md`](../00-product/non-goals.md) forbids this relay sending email
   itself. The email leg was never what proved who was asking. The step-up
   proves that, and it is the same resolution #56 made for the recovery blob's
   OTP. The phrase only makes the deletion a deliberate second call, so one
   retried request or client bug cannot delete an account.
2. `DELETE /accounts/me { confirm_phrase }` consumes the phrase
   (case-insensitive, single use) and marks the account. A phrase that is
   wrong, expired or already used gets `403 ACCOUNT_DELETE_PHRASE_INVALID`, and
   a wrong one leaves the live phrase usable. A repeated confirmation reports
   the first one's time and does not move the erasure.

From the mark on, the account may open no sync session, publish no op, and
init, upload or finalize no blob: each answers
`403 ACCOUNT_PENDING_DELETION`. That holds for a session opened before the mark
too. Reads and the account routes stay open. Nothing undoes the mark.

The maintenance pass erases a marked account once
`[storage] account_delete_grace_days` (default 30) have passed. In one
transaction it deletes the account row, which cascades to its devices, push
tokens, declared cursors, deletion phrase and blob tombstones. It also deletes
the account's relay frames, batches and eviction watermarks, which are keyed by
`account_h` rather than by a foreign key. After that commit it removes both blob
trees, `pending/` and `committed/`, and drops the account's channels from the
in-memory ring. An operator can erase at once with
`sunrise-server admin account delete <id> --immediately`
([`self-hosting.md`](./self-hosting.md) §Admin CLI).

#### Terms acceptance

`terms_at_ms` records when the account's holder accepted the operator's terms.
Sunrise presents no terms of its own. Sign-up belongs to the OIDC issuer
([`auth.md`](./auth.md) §Account model;
[`../00-product/non-goals.md`](../00-product/non-goals.md)), so an operator
that imposes terms presents them there, before the holder has any bearer. A
bearer is therefore the evidence of acceptance. A client with no terms surface
of its own sends the moment it publishes under that bearer. A client that shows
terms sends the moment its user accepted them. The relay keeps the first value
it receives (`COALESCE`), so a retry or a second device cannot move it.

The client supplies the value. `SunriseCore::bootstrap_account` takes it as a
parameter and never reads its own clock for it, so each client decides what
acceptance means where its product surface is. A device joining an account
that already exists should send no `POST /accounts`: it calls
`POST /api/v1/devices` alone and asserts nothing on the holder's behalf. The
Apple app's paired devices do (`register_relay_device` across the UniFFI seam),
and so does `sunrise recover` (`sunrise_relay_client::register_device`).
`sunrise bootstrap` run on a paired device still sends the `POST`, and
`COALESCE` keeps the founding device's value.

#### Account errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `VALIDATION_*` | Malformed body, missing field. | No (fix request). |
| 401 | `AUTH_TOKEN_INVALID` | OIDC token signature/issuer/audience invalid. | No. |
| 401 | `AUTH_TOKEN_EXPIRED` | Token `exp` past. | After OIDC refresh. |
| 403 | `AUTH_SIGNUP_DISABLED` | First-login attempt for unknown account when `auth.allow_signup = false`. | No. |
| 403 | `AUTH_STEP_UP_REQUIRED` | `GET /accounts/me/recovery_blob` with a bearer whose authentication is not recent or strong enough. | After a fresh OIDC authorization request (`max_age=0` / `prompt=login`), **not** after a token refresh — a refresh does not move `auth_time`. |
| 404 | `RECOVERY_BLOB_NOT_FOUND` | The account has no recovery blob. Only reachable *after* the step-up, so it is not an oracle for which accounts have one. | No. |
| 409 | `RECOVERY_BLOB_EXISTS` | `POST /accounts` offered a recovery blob differing from the stored one. | No — the stored blob stands; rotation needs the unbuilt `PUT`. |
| 403 | `ACCOUNT_DELETE_PHRASE_INVALID` | `confirm_phrase` token consumed, expired, or never issued. | After re-running `/initiate`. |
| 403 | `ACCOUNT_PENDING_DELETION` | The account's deletion is confirmed: `POST /sync/session`, `POST /sync/ops` and every blob upload step are refused. | No. |
| 413 | `VALIDATION_PAYLOAD_TOO_LARGE` | `recovery_blob` body > 10 MiB. Body: `{ "code":"VALIDATION_PAYLOAD_TOO_LARGE", "max_bytes": 10485760 }`. | No (shrink). |

`VALIDATION_PAYLOAD_TOO_LARGE` is **not implemented**: no constant exists in
`error.rs`'s `codes` module, and the route that would emit it does not exist. An
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

**One of these five has no caller in this workspace.** `GET /api/v1/devices`
is expressible by the generated relay client, and `sunrise-relay-client`'s
`bootstrap` does not issue it; nothing else reaches for it either. It is not
dead weight. The device list is what a device-management surface reads —
[#144](https://github.com/justin13888/Sunrise/issues/144) and
[#160](https://github.com/justin13888/Sunrise/issues/160) both want one, and
[#170](https://github.com/justin13888/Sunrise/issues/170) wants somewhere on the
Apple clients to keep the relay device id such a list is keyed by. Whoever adds
a caller signs it: every route here takes a signed extractor.
`POST /api/v1/devices/push-tokens` is called by
`sunrise_relay_client::register_push_token`, which the iOS app reaches through
`SunriseCore::register_push_token`
([#367](https://github.com/justin13888/Sunrise/issues/367)); it signs the
canonical body with `sunrise_http_sig::sign_with`, the pattern for a signed
route reached through the generated client, as
`SseTransport::with_device_signer` is for one reached through the sync
transport.

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
| DELETE | `/api/v1/blobs/<blob_id>` | `{ stream_id, device_id, seq }` | `202 { collect_after_ms }`; a tombstone, collected later (below) |

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

**The client is `sunrise_sync::Transport`'s four blob methods**, implemented by
`SseTransport` and driven by `sunrise_core::sync_driver` out of the
`blob_uploads` queue; `crates/sunrise-core/src/blob_sync.rs` records where the
upload is driven from, what a retry re-uses, and every bound on it. Before
[#176](https://github.com/justin13888/Sunrise/issues/176) nothing in the
workspace called any of the four, so an attachment was readable only on the
device that made it.

The client does **not** go through the generated relay client. All four are
reached with the transport's own hyper client, because two of them — the
raw-binary chunk `PUT` and the blob `GET` — are absent from the generated
surface: kynos and spargen disagree about how OpenAPI 3.1 describes a raw
binary body, and `crates/sunrise-relay-client/build.rs:42`, `:46` record the
disagreement as two omit rules. Splitting one two-phase commit across two HTTP
clients to use generated code for half of it would buy nothing and cost a
second place for the bearer, the base URL and the device binding to live.

**Deletion is a tombstone, not an unlink.**
`DELETE /api/v1/blobs/<blob_id> { stream_id, device_id, seq }` answers
`202 { collect_after_ms }`, or the fetch's `404` for a blob this account has not
committed. The body names the op that detached the blob by its cleartext
routing head: the stream it was published in, the publishing device, and its
`seq`. These are the fields the relay already reads off every op. The ciphertext
stays readable. The maintenance pass reclaims it once both conditions in
[`../02-domain/attachments.md`](../02-domain/attachments.md) §Deletion hold:

- `[storage] gc_grace_days` (default 30) have passed since the tombstone.
- Every active device of the account has acknowledged the op. A device
  acknowledges by declaring, on `POST /sync/subscribe`, a cursor for that
  stream and publishing device at or past `seq`.

The relay cannot read the `device_op_cursor` op the domain doc describes, so the
subscribe cursors stand in for it. They are the same statement ("I have applied
this device's ops up to here"), made in cleartext. A device is active when it is
unrevoked and has declared a cursor in the last 30 days. A signed subscribe
records its cursors; an unsigned one has no device to record them against. The
device that sent the `DELETE` is excused: it wrote the op. Tombstoning again
keeps the first tombstone and its clock. Finalizing the same ciphertext again
lifts the tombstone, because the attachment is back.

#### Blob errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `VALIDATION_INVALID` | Malformed body, out-of-range `chunk_count`, a chunk outside 1..=1 MiB, or a path segment that is not a well-formed `up_`/`blb_` id. | No (fix request). |
| 400 | `BLOB_HASH_MISMATCH` | A chunk's ciphertext BLAKE3, or the concatenation's, disagrees with the supplied hash. | No (re-upload). |
| 401 | `AUTH_TOKEN_INVALID` | Missing or unverifiable bearer. | After OIDC refresh. |
| 409 | `BLOB_CHUNK_MISSING` | `finalize` names a chunk that was never uploaded. | After uploading it. |
| 404 | `BLOB_NOT_FOUND` | No committed blob under that id **for this account**, on a fetch or a `DELETE`. | No. |
| 403 | `ACCOUNT_PENDING_DELETION` | `init`, a chunk `PUT` or `finalize` for an account whose deletion is confirmed. | No. |
| 413 | `VALIDATION_PAYLOAD_TOO_LARGE` | Body over `max_body_bytes`. | No (shrink). |

### Sharing — NOT IMPLEMENTED

No `api/shares.rs` exists, no `shares` table is in `store/`'s schema, and
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

### Pairing rendezvous

Where two devices meet to run the pairing handshake
([`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md)
§Relay framing for Noise), so nobody copies the six messages between screens.
Served by `crates/sunrise-server/src/api/pairing.rs`. Bearer only: the device
being added holds no device key yet, so these take the bootstrap exemption, and
a binding that is supplied is still verified.

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/pairing/send` | `{ pair_id, role, index, message }` | `200 { sent, expires_at_ms }` |
| POST | `/api/v1/pairing/receive` | `{ pair_id, role, after }` | `200 { messages, expires_at_ms }`: the other role's messages from index `after` on |
| POST | `/api/v1/pairing/abort` | `{ pair_id, role }` | `204`, whether or not a session was dropped |

`pair_id` is the QR's, 16 bytes base64url; `role` is `new_device` or
`existing_device`; `message` is one Noise message, base64url, 1 to 64 KiB
decoded. The relay never reads it.

- **Only the new device opens a session**, with its first message, and the
  session is bound to that bearer's account. That message is one pair attempt
  against the limits in §Rate limits.
- **Three messages per role**, which is what the protocol sends. A fourth drops
  the session.
- **`index` is the count of this role's earlier messages**, so a retry after a
  lost answer is acknowledged rather than buffered twice. An index that names a
  slot holding a different message, or skips past the next free slot, drops the
  session.
- **At most 4096 live sessions per relay.** Past that, opening one is `429`
  until the oldest expires.
- **300 s from the opening message**, then the session is dropped whatever its
  state. Sessions live in memory and do not survive a restart.
- A session that is not live **for the caller's account** — never opened,
  expired, aborted, overflowed, or another account's — is `404
  RELAY_PAIR_SESSION_GONE`. The answer is the same in every case, so the route
  is not an oracle for live pair ids. Start again from a new code.

Clients poll `receive`; there is no stream.

### Health / meta

| Method | Path | Body | Returns | Status |
|---|---|---|---|---|
| GET | `/api/v1/meta` | — | `MetaResponse` (below) | implemented |
| GET | `/api/v1/health` | — | `200 {"status":"ok"}` | implemented |
| GET | `/api/v1/health?deep=1` | — | `200` if every readiness check passes; `503` naming the failed ones | implemented (no disk-free check) |

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

`GET /api/v1/health` without `deep` (or with `deep=0`) always answers `200
{"status":"ok"}` — the handler consults nothing, so it is a liveness probe. It
stays `200` while the server drains: a draining process is alive.

Readiness is the same route with `?deep=1`, not a separate `/api/v1/ready`.
The query was already specified here, so a probe configuration written against
it needs no change, and one route keeps the two probes from drifting apart.
`deep` is the only query parameter on the surface; the route is unsigned, so
the signed operations' canonical target (`api/signed.rs`) is untouched. A
`deep` that is not an integer from 0 to 255 is a `400`.

`?deep=1` answers `200` only if all of these pass, and `503` otherwise:

| Check | Passes when |
|---|---|
| `accepting` | the server is not draining; it fails from the moment `SIGTERM` arrives |
| `store` | `SELECT 1` against the relay database returns within 2 s |
| `blob_root` | a probe file is written and removed under the blob root within 5 s |

Both answers carry the outcomes, and a `503` names the failures in `failed`:

```json
{"status":"ok","checks":{"accepting":true,"store":true,"blob_root":true}}
{"status":"unavailable","checks":{"accepting":false,"store":true,"blob_root":true},"failed":["accepting"]}
```

Two checks an earlier draft listed are not here. An object store `HEAD` on
`_health/probe` has nothing to probe: self-host blobs are files, and
`blob_root` is that store's check. A local disk free ratio above 5% is not
built, because reading it needs a `statvfs` the crate's `forbid(unsafe_code)`
leaves no safe dependency for yet.

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

A `429` is always `RATE_LIMITED` with a `Retry-After` header; see §Rate limits.

Codes are **stable** (clients map them to translated strings). New codes can be added; clients see unknown codes as a generic error. Messages never quote a token, a key, or a subject: a JWKS transport failure and a forged signature both render as the same opaque `401`, and a storage failure — the metadata store or the blob backend not answering — renders as a retryable `503 RELAY_STORAGE_UNAVAILABLE` naming only which store, never the backend's own message (ADR-0062 §1).

### Quota responses — REMOVED

There are none, and there is no longer a code to build them from. ADR-0027
takes per-account quotas out of scope; `AUTH_QUOTA_EXCEEDED` and
`STORAGE_QUOTA_EXCEEDED` are gone from `sunrise-error`'s `codes.toml` and their
ids (203, 300) are burned. Nothing counts storage, ops, or devices against a
plan, and the `429`/`202` pair this section used to specify — a hard cap over
110% and a soft warning inside the grace window — is described in
[`billing.md`](./billing.md) as the shape a future quota surface would take,
not as anything a client can receive.

## Rate limits

Every route is limited. `crates/sunrise-server/src/api/ratelimit/` enforces
this table; every number is a `[limits]` key in `sunrise.toml`
([`self-hosting.md`](./self-hosting.md) §Config), and the defaults below are
what a relay with no `[limits]` table runs.

### The refusal

A limited request gets **`429 Too Many Requests`**, a problem document with
`type` `https://sunrise.app/problems/rate-limited` and code **`RATE_LIMITED`**
(registry id 800, transient, retryable), and a **`Retry-After`** header in whole
seconds, never less than 1. Waiting exactly that long is admitted; retrying
sooner is refused again. The `429` and its header are in the description of
every operation, because the limiter covers every operation.

Every refusal increments `sunrise_ratelimit_rejected_total{endpoint,scope}`
([`metrics.md`](./metrics.md)); the first refusal of a run on one key logs
`srv.ratelimit.rejected` ([`log-events.md`](../10-cross-cutting/log-events.md)).

### Per client address, before anything else

Charged by the admission interceptor, the outermost layer of the router, so a
flood is refused before a body is read or a bearer verified. One bucket per
route group per client address; a bucket holds one minute's allowance and
refills continuously.

| Group | Routes | Auth | Default per address |
|---|---|---|---|
| `probe` | `GET /api/v1/health`, `GET /metrics` (loopback bind only) | none | 120 / min (`probe_per_min`) |
| `meta` | `GET /api/v1/meta` | none | 60 / min (`meta_per_min`) |
| `bootstrap` | `POST /api/v1/accounts`, `POST /api/v1/devices` | bearer | 10 / min (`bootstrap_per_min`) |
| `account` | `GET /api/v1/accounts/me`, `GET /api/v1/accounts/me/recovery_blob`, `POST /api/v1/accounts/me/delete/initiate`, `DELETE /api/v1/accounts/me`, `GET /api/v1/devices`, `DELETE /api/v1/devices/{device_id}`, `DELETE /api/v1/devices/by-vault-id/{vault_device_id}`, `POST /api/v1/devices/push-tokens` | bearer+sig | 60 / min (`account_per_min`) |
| `blob` | `POST /api/v1/blobs/init`, `PUT /api/v1/blobs/{upload_id}/{chunk_idx}`, `POST /api/v1/blobs/finalize`, `GET /api/v1/blobs/{blob_id}`, `DELETE /api/v1/blobs/{blob_id}` | bearer+sig | 600 / min (`blob_per_min`) |
| `sync` | `POST /api/v1/sync/session`, `POST /api/v1/sync/session/refresh`, `POST /api/v1/sync/subscribe`, `POST /api/v1/sync/ops`, `GET /api/v1/sync/events`; the pairing rendezvous's `POST /api/v1/pairing/send`, `POST /api/v1/pairing/receive`, `POST /api/v1/pairing/abort` | bearer+sig (+session); pairing bearer only | 600 / min (`sync_per_min`) |

**Failed authentication.** Every `401` an address earns on a `bootstrap`,
`account`, `blob` or `sync` route is counted, 20 per five minutes
(`failed_auth_per_5min`). Past that, the address's requests to those routes are
refused with `429` *before* the bearer verifier runs, a valid bearer included,
until the budget refills. `probe` and `meta` verify nothing and are exempt.

The test `every_operation_in_the_description_has_a_group` reads the generated
OpenAPI description and fails for any operation this table does not name, so a
route added later is a red build until it is given a row. At run time an
unlisted route is held to `bootstrap`, the tightest authenticated group.

**Which address.** The socket peer, unless it is listed in `[server]
trusted_proxies`; then the right-most address in `Forwarded` (or, in its
absence, `X-Forwarded-For`) that is not itself a listed proxy. With the list
empty — the default — forwarding headers are ignored, because a client can
write them. IPv6 clients are counted by `/64`, since one subscriber routinely
holds a whole `/64`; an IPv4-mapped address counts as its IPv4 address.

### Per device and per account, after authentication

Charged by the handlers, because the device is known only once its signature
has verified and the cost is a property of the request. A caller that signs with
no device — the single-tenant self-host verifier — is charged by its account,
and its refusals are labelled `scope="account"`.

| Budget | Route | Cost | Default |
|---|---|---|---|
| Op uploads | `POST /api/v1/sync/ops` | ops in the batch | 50 / s per device, bursting to 500 (`ops_per_sec`; the 10 s window [`backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md) proposed) |
| Blob upload | `PUT /api/v1/blobs/{upload_id}/{chunk_idx}` | chunk bytes | 64 MiB / min per device (`blob_upload_bytes_per_min`) |
| Blob download | `GET /api/v1/blobs/{blob_id}` | the blob's whole size, charged before the first byte | 256 MiB / min per device (`blob_download_bytes_per_min`) |
| Open uploads | `POST /api/v1/blobs/init` | one per upload, until `finalize` or an hour untouched | 16 at once per account (`open_uploads`) |
| Session creation | `POST /api/v1/sync/session` | one | 10 per 5 min per device (`sessions_per_5min`) |
| Event streams | `GET /api/v1/sync/events` | one, held until the stream ends | 4 at once per device (`streams`) |
| Pair attempts | `POST /api/v1/pairing/send`, the message that opens a session | one | 10 per rolling hour and 30 per rolling day per account, and 60 per rolling hour per client address (`sunrise_pairing::AttemptLimit`; not configurable). Off with `[limits] enabled = false` |

A request whose cost exceeds a whole bucket — a 1,000-op batch, a 100 MB blob
— is admitted from a full bucket and leaves the key in debt, so the next
request waits for the debt to refill. Refusing it would refuse it forever,
since a client cannot shrink a batch it already built.

An upload that is never finalized stops counting an hour after its last chunk,
so a client that crashed mid-upload cannot lock its account out of
attachments. An event stream releases its slot the moment its client
disconnects.

### What this does not cover

- **State is per process.** Every counter lives in the relay's memory, behind
  the `LimiterStore` trait so a shared store can replace it; several relays
  behind one balancer each enforce the table separately. See
  [#364](https://github.com/justin13888/Sunrise/issues/364).
- **No quota.** These are rates, not allowances: nothing counts storage, total
  ops or devices against a plan. That is
  [`backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md), still
  `proposed`, and [ADR-0027](../11-adr/0027-v1-self-host-first.md) defers it.
- **A flood from more than 65,536 addresses at once** fills the in-memory table;
  idle buckets are evicted first, and an address the table still cannot hold is
  admitted untracked rather than refused, so a distributed flood cannot lock
  out clients the relay has not seen yet. The reverse proxy is the place to cap
  connection counts below that.
- **CORS preflights are not limited.** With `allowed_origins` set, kynos
  answers `OPTIONS` on each covered path with an operation it synthesizes, and
  it gives that operation no interceptors, so the admission interceptor never
  runs on a preflight. A preflight reads no body, verifies nothing and touches
  no state, but an address can send as many as it likes; the reverse proxy is
  the place to cap them. No `HEAD` operation is generated either, so a `HEAD`
  is answered `405` by the router's method fallback, also outside the limiter.
- **The rate-limit response headers** (`RateLimit`, `RateLimit-Policy`) are not
  sent; `Retry-After` is the contract.

## Why so few endpoints?

Most "API" calls in a typical app — CRUD on tasks — are *not* server API calls in Sunrise. They're local commands that produce ops, which sync as opaque envelopes over `POST /sync/ops` and back down the event stream. The REST surface is intentionally minimal.

## Endpoint availability by deployment

Only the single-binary column exists today; the other two describe deployments
that have not been built.

| Endpoint | Managed | Self-host (default) | Self-host (single-binary) |
|---|---|---|---|
| `/api/v1/accounts` (POST), `/api/v1/accounts/me` (GET) | yes | yes | **yes — built** |
| `/api/v1/accounts/me/recovery_blob` (GET) | yes | yes | **yes — built, behind an OIDC step-up** |
| `/api/v1/accounts/me/delete/initiate` (POST), `/api/v1/accounts/me` (DELETE) | yes | yes | **yes — built, behind an OIDC step-up** |
| `/api/v1/accounts/me/recovery_blob` (PUT) | yes | yes | **no route** |
| `/api/v1/identities` (discovery) | yes | yes | **no route** |
| `/api/v1/devices`, `/api/v1/devices/<id>` | yes | yes | **yes — built** |
| `/api/v1/devices/push-tokens` | yes | optional (operator's APNs/FCM creds) | **route built; no delivery path** |
| `/api/v1/blobs/*` | yes (S3-backed) | yes (S3-backed) | **yes — local-disk-backed** |
| `/api/v1/shares/*` | yes | yes | **no route** |
| `/api/v1/pairing/*` | yes | yes | **yes — built, in memory** |
| `/api/v1/meta`, `/api/v1/health` | yes | yes | **yes — built** |
| `/metrics` (router root, not under `/api/v1`) | yes | yes | **yes — built, unauthenticated** |

Clients query `/api/v1/meta`'s `capabilities` field on connect and adapt UI affordances accordingly (e.g. hide "enable push notifications" if push is unavailable). Today that field is the constant `REQUIRED_SERVER_BITS`, so it reports the required set rather than what this deployment can actually do.
