---
status: accepted
---

# Server API

Two surfaces: the **sync protocol** (over WebSocket; spec'd in [`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md)) and a small **REST API** for account lifecycle and blobs.

## REST endpoints

All endpoints are HTTPS. Authenticated requests carry a standard OIDC access token in `Authorization: Bearer <jwt>` plus an `X-Sunrise-Device: <dev_id>` header (see [`auth.md`](./auth.md)). Sign-up, login, password reset, MFA, and email change are all handled by the configured OIDC issuer — they have no Sunrise REST endpoints.

### Device binding (per-request)

The `X-Sunrise-Device` header carries the `device_id` (Crockford base32 of 16 bytes) on every authenticated request. Every authenticated request is also accompanied by an `X-Sunrise-Device-Sig` header containing an Ed25519 detached signature over the canonical request line, the `Date` header, and a hash of the request body. The server validates that:

1. `device_id` exists in the account's device set and is not revoked.
2. `X-Sunrise-Device-Sig` verifies under the device's signing key.
3. The OIDC token's `https://sunrise.app/device_id` claim (URI-namespaced per RFC 7519 §4.2; issued by the IdP from the client's `claims` parameter) matches `X-Sunrise-Device` (defense in depth).

This `header_sig_v1` mode is the only mode for v1. Clients can probe forward-compat via `GET /api/v1/meta` which returns `{"device_binding_mode":"header_sig_v1"}`.

### Account

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/accounts` | `{ identity_pub_s, identity_pub_d, recovery_blob, terms_accepted_at }` | `{ account_id }` (called once by the first device after OIDC login auto-provisions the account) |
| GET | `/api/v1/accounts/me` | — | `{ account_id, email, devices: [DeviceMeta], plan }` |
| PUT | `/api/v1/accounts/me/recovery_blob` | `{ recovery_blob }` | 204 |
| GET | `/api/v1/accounts/me/recovery_blob` | — | `{ recovery_blob }` (opaque ciphertext; useless without the offline recovery code) |
| POST | `/api/v1/accounts/me/delete/initiate` | — | 202; issues a single-use confirmation token (32 bytes Crockford base32, 52 chars, TTL 15 minutes) which the OIDC issuer relays to the user's verified email |
| DELETE | `/api/v1/accounts/me` | `{ confirm_phrase }` | 202 (deletion within 30 days) |

The `recovery_blob` is stored opaquely. The server does not validate its internal format or version; it only checks size (10 MiB cap, see error table) and that the upload is signed by an active device. Recovery blobs follow the uniform 5-byte magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 — the server stores opaque bytes and does not introspect.

#### Account errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `VALIDATION_*` | Malformed body, missing field. | No (fix request). |
| 401 | `AUTH_TOKEN_INVALID` | OIDC token signature/issuer/audience invalid. | No. |
| 401 | `AUTH_TOKEN_EXPIRED` | Token `exp` past. | After OIDC refresh. |
| 403 | `AUTH_SIGNUP_DISABLED` | First-login attempt for unknown account when `auth.allow_signup = false`. | No. |
| 403 | `ACCOUNT_DELETE_PHRASE_INVALID` | `confirm_phrase` token consumed, expired, or never issued. | After re-running `/initiate`. |
| 413 | `VALIDATION_PAYLOAD_TOO_LARGE` | `recovery_blob` body > 10 MiB. Body: `{ "code":"VALIDATION_PAYLOAD_TOO_LARGE", "max_bytes": 10485760 }`. | No (shrink). |

### Identity discovery (for sharing)

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/v1/identities?handle=<h>` | — | `{ identity_id, identity_pub_s, identity_pub_d, fingerprint_qr }` |
| GET | `/api/v1/identities/<idn_…>` | — | as above |

The user's `identity_pub_*` is signed-by-self; a hostile server substituting keys is detectable via OOB fingerprint check.

### Devices

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/devices` | `{ device_pub_s, device_pub_d, device_cert, nickname, platform }` | `{ device_id }` |
| DELETE | `/api/v1/devices/<dev_id>` | — | 204 (revocation; only callable by another paired device) |
| GET | `/api/v1/devices` | — | `[DeviceMeta]` |
| POST | `/api/v1/devices/<dev_id>/push_token` | `{ provider, token }` | 204 |

Device revocation deletes the Postgres row in the request's transaction (synchronous DELETE). Tokens already in flight may still succeed for up to 5 s while connection-affinity caches expire; clients SHOULD NOT rely on instantaneous propagation, and operator docs document this 5 s window.

#### `DeviceMeta` schema

```cddl
DeviceMeta = {
  device_id:       tstr,        ; Crockford base32 of 16 bytes
  nickname:        tstr,        ; user-set; ≤ 64 bytes; default = platform default ("MacBook Air", etc.)
  platform:       "ios" / "android" / "macos" / "windows" / "linux" / "web",
  app_version:    tstr,         ; e.g. "1.4.2"
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
| 403 | `AUTH_DEVICE_NOT_OWNER` | Caller is not a paired device of the account. | No. |
| 404 | `DEVICE_NOT_FOUND` | `<dev_id>` does not match any device on this account. | No. |

### Blobs

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/blobs/init` | `{ size, chunk_count }` | `{ upload_urls: [presigned], blob_id }` |
| POST | `/api/v1/blobs/<blob_id>/finalize` | `{ chunk_hashes }` | 204 |
| GET | `/api/v1/blobs/<blob_id>?chunk=<i>` | — | bytes (redirect to presigned URL) |
| DELETE | `/api/v1/blobs/<blob_id>` | — | 204 (only owner; tombstones) |

`chunk_hashes` is an array of `BLAKE3(ciphertext_chunk, 32)` lowercase hex (32 chars per entry — short hash, 16 bytes). There is exactly one hash per chunk; server-side validation uses the storage backend's native ETag equivalent (S3) or computes BLAKE3-of-ciphertext on receipt for the local backend. No `plaintext_hash` exists server-side; that's a client-only construct. Blob payloads are stored as opaque bytes — the magic prefix from [`../10-cross-cutting/protocol-versioning.md`](../10-cross-cutting/protocol-versioning.md) §3 is enforced by clients, not the server. Presigned upload URLs expire 1 hour after issuance; download URLs expire 24 hours after issuance.

#### Blob errors

| HTTP | Code | When | Retry |
|---|---|---|---|
| 400 | `BLOB_HASH_MISMATCH` | A chunk's ciphertext BLAKE3 disagrees with the supplied `chunk_hashes`. Body: `{ "code":"BLOB_HASH_MISMATCH", "chunk_index": <int> }`. | No (re-upload). |
| 409 | `BLOB_FINALIZE_CONFLICT` | Two finalize requests for the same `blob_id` carry non-matching hash arrays. (Identical hashes → idempotent `200 OK`.) | No. |
| 413 | `VALIDATION_PAYLOAD_TOO_LARGE` | Init's `size` exceeds `max_blob_size`. | No. |
| 429 | `AUTH_QUOTA_EXCEEDED` | Account is hard-capped (>110% of plan, see [`billing.md`](./billing.md)). Header `Retry-After` carries seconds until period end. | After upgrade or period reset. |

### Sharing

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/shares` | `{ stream_id, recipient_idn, share_envelope }` | `{ share_id }` |
| DELETE | `/api/v1/shares/<share_id>` | — | 204 |
| GET | `/api/v1/shares/incoming` | — | `[{ share_id, granter_idn, share_envelope, created_at }]` |

`POST /api/v1/shares` is idempotent on `(stream_id, recipient_idn)`:
- If a `pending` or `accepted` grant already exists, return `200 OK` with the existing `share_id`.
- If only `revoked` or `declined` grants exist, create a new grant and return `201 Created`.

The `share_envelope` is opaque to the server.

### Health / meta

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/v1/meta` | — | `{ server_version, protocol_version, oidc_issuer, oidc_client_id, capabilities, max_op_size, max_blob_size, device_binding_mode }` |
| GET | `/api/v1/health` | — | 200 if alive |
| GET | `/api/v1/health?deep=1` | — | 200 if every backing dependency is healthy; 503 otherwise |

The deep readiness check returns `200 {"ok":true,"checks":{...}}` only if all of:

1. `SELECT 1` from primary DB within 2 s.
2. Object store HEAD on `_health/probe` within 5 s.
3. Local disk free ratio > 5%.

Otherwise it returns `503` with a body listing the failed checks.

`oidc_issuer` and `oidc_client_id` let an unauthenticated client bootstrap the OIDC flow without static configuration.

## Errors

JSON body:

```json
{
    "error": {
        "code": "QUOTA_EXCEEDED",
        "message": "…",        // safe-for-logs
        "retry_after_seconds": 300
    }
}
```

Codes are **stable** (clients map them to translated strings). New codes can be added; clients see unknown codes as a generic error.

### Quota responses

- **Hard quota exceeded** (over 110% of plan, or post-grace downgrade): `429 Too Many Requests`, body `{ "code":"AUTH_QUOTA_EXCEEDED", ... }`, `Retry-After: <seconds-until-period-end>` header.
- **Soft warning** (within 7-day grace, 100%–110%): `202 Accepted`, header `X-Sunrise-Quota-Warning: true`, body includes `quota_used_ratio`.

## Rate limits

Per-IP: 60 RPM unauthenticated, 600 RPM authenticated.
Per-account: see [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md).

## Why so few endpoints?

Most "API" calls in a typical app — CRUD on tasks — are *not* server API calls in Sunrise. They're local commands that produce ops, which sync via the WebSocket transport as opaque envelopes. The REST surface is intentionally minimal.

## Endpoint availability by deployment

| Endpoint | Managed | Self-host (default) | Self-host (single-binary) |
|---|---|---|---|
| `/api/v1/accounts` | yes | yes | yes |
| `/api/v1/accounts/me/*` | yes | yes | yes |
| `/api/v1/identities` (discovery) | yes | yes | yes |
| `/api/v1/devices/*` | yes | yes | yes |
| `/api/v1/devices/<id>/push_token` | yes | optional (operator's APNs/FCM creds) | no |
| `/api/v1/blobs/*` | yes (S3-backed) | yes (S3-backed) | yes (local-disk-backed) |
| `/api/v1/shares/*` | yes | yes | yes |
| `/api/v1/meta`, `/api/v1/health` | yes | yes | yes |

Clients query `/api/v1/meta`'s `capabilities` field on connect and adapt UI affordances accordingly (e.g. hide "enable push notifications" if push is unavailable).
