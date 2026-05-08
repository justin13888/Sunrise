---
status: accepted
---

# Server API

Two surfaces: the **sync protocol** (over WebSocket; spec'd in [`../05-sync/wire-protocol.md`](../05-sync/wire-protocol.md)) and a small **REST API** for account lifecycle and blobs.

## REST endpoints

All endpoints are HTTPS. Authenticated requests carry a device-signed bearer in `Authorization: SunriseDeviceSig <…>` (see [`auth.md`](./auth.md)).

### Account

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/accounts/init` | `{ email }` | `{ otp_pending: true }` (sends OTP email) |
| POST | `/api/v1/accounts` | `{ email, otp, identity_pub_s, identity_pub_d, recovery_blob, terms_accepted_at }` | `{ account_id, server_id }` |
| GET | `/api/v1/accounts/me` | — | `{ account_id, email, devices: [DeviceMeta], plan }` |
| POST | `/api/v1/accounts/me/email_change` | `{ new_email, otp_code }` | 204 |
| POST | `/api/v1/accounts/me/recovery_blob` | `{ recovery_blob }` | 204 |
| POST | `/api/v1/accounts/recovery/init` | `{ email }` | `{ otp_pending: true }` (skipped on self-host with `auth.recovery_requires_email = false`) |
| POST | `/api/v1/accounts/recovery/blob` | `{ challenge_sig }` | `{ recovery_blob }` (auth via recovery-code-derived challenge; see auth.md) |
| DELETE | `/api/v1/accounts/me` | `{ confirm_phrase }` | 202 (deletion within 30 days) |

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

### Blobs

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/blobs/init` | `{ size, chunk_count }` | `{ upload_urls: [presigned], blob_id }` |
| POST | `/api/v1/blobs/<blob_id>/finalize` | `{ chunk_hashes }` | 204 |
| GET | `/api/v1/blobs/<blob_id>?chunk=<i>` | — | bytes (redirect to presigned URL) |
| DELETE | `/api/v1/blobs/<blob_id>` | — | 204 (only owner; tombstones) |

### Sharing

| Method | Path | Body | Returns |
|---|---|---|---|
| POST | `/api/v1/shares` | `{ stream_id, recipient_idn, share_envelope }` | `{ share_id }` |
| DELETE | `/api/v1/shares/<share_id>` | — | 204 |
| GET | `/api/v1/shares/incoming` | — | `[{ share_id, granter_idn, share_envelope, created_at }]` |

The `share_envelope` is opaque to the server.

### Health / meta

| Method | Path | Body | Returns |
|---|---|---|---|
| GET | `/api/v1/meta` | — | `{ server_version, protocol_versions, capabilities, max_op_size, max_blob_size }` |
| GET | `/api/v1/health` | — | 200 if alive |

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

## Rate limits

Per-IP: 60 RPM unauthenticated, 600 RPM authenticated.
Per-account: see [`../05-sync/backpressure-and-quotas.md`](../05-sync/backpressure-and-quotas.md).

## Why so few endpoints?

Most "API" calls in a typical app — CRUD on tasks — are *not* server API calls in Sunrise. They're local commands that produce ops, which sync via the WebSocket transport as opaque envelopes. The REST surface is intentionally minimal.

## Endpoint availability by deployment

| Endpoint | Managed | Self-host (default) | Self-host (single-binary) |
|---|---|---|---|
| `/api/v1/accounts/init`, `/api/v1/accounts` (OTP-gated) | yes | yes | yes (OTP optional via `auth.signup_requires_email`) |
| `/api/v1/accounts/me/*` | yes | yes | yes |
| `/api/v1/accounts/recovery/*` | yes | yes (email gate optional) | yes (email gate optional) |
| `/api/v1/identities` (discovery) | yes | yes | yes |
| `/api/v1/devices/*` | yes | yes | yes |
| `/api/v1/devices/<id>/push_token` | yes | optional (operator's APNs/FCM creds) | no |
| `/api/v1/blobs/*` | yes (S3-backed) | yes (S3-backed) | yes (local-disk-backed) |
| `/api/v1/shares/*` | yes | yes | yes |
| `/api/v1/meta`, `/api/v1/health` | yes | yes | yes |

Clients query `/api/v1/meta`'s `capabilities` field on connect and adapt UI affordances accordingly (e.g. hide "enable push notifications" if push is unavailable).
