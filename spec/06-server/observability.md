---
status: accepted
---

# Observability

Operate the server without violating the E2EE guarantee.

## What we log

- Aggregate request rates (per endpoint, per status).
- Aggregate WS connection counts.
- Aggregate per-account-bucket op rates (bucketed by account-hash, not raw account ID, beyond 24h).
- Slow query logs (with no payload bodies).
- Error frequencies by error code.

## What we *never* log

- Op envelopes or any subset thereof.
- Account emails (beyond 24h; only kept for short-term auth tracing).
- Push tokens.
- Any plaintext, anywhere.
- IPs joined to user IDs (beyond 24h).

## Metrics (Prometheus)

Naming convention: `sunrise_<area>_<measure>`.

Examples:

```
sunrise_sync_connections_active
sunrise_sync_ops_received_total{kind="OpBatch"}
sunrise_sync_ops_delivered_total{kind="Push"}
sunrise_sync_op_latency_seconds_bucket{le="…"}
sunrise_blob_uploads_total
sunrise_push_dispatch_total{provider="apns",result="ok|fail"}
sunrise_quota_exceeded_total{kind="storage|rate|devices"}
sunrise_db_query_seconds{op="…"}
```

No labels carrying user identity.

## Tracing

OTel-compatible tracing with a sampling rate (1% prod, 100% staging) and span attributes scrubbed of user identifiers. Spans cover: connection lifecycle, op-batch ingress/egress, push dispatch.

## Health

- `GET /api/v1/health` returns 200 + minimal JSON.
- A deeper readiness check at `/api/v1/health?deep=1` verifies DB and object store reachability.

## Alerts (managed)

| Alert | Trigger |
|---|---|
| WS connection drop spike | >5x baseline |
| Op ingress error rate | >1% over 5m |
| S3 errors | any non-zero rate over 5m |
| Push dispatch failure rate | >5% over 15m |
| Storage growth anomaly | >2x weekly trendline |
| Auth failure rate | >5x baseline over 10m |

## Audit trail (per-account, retained briefly)

For account-management actions only (not content):

- Account created.
- Device added/removed.
- Recovery blob fetched.
- Account deleted.
- Plan changed.

Visible in the user's "Security" page on the web app, derived from a per-account audit log retained 90 days.

## Privacy commitments to users

The privacy policy explicitly states what we keep and for how long. The numbers in this spec are what we commit to. Any operational change that increases retention requires a user-visible privacy policy update with a 30-day notice.

## Diagnostics from the client

A user can opt in to send a diagnostics package to support. The package is a curated bundle (no plaintext content; just sync stats, error codes, version info, op counts). The user is shown the bundle contents before sending and signs it with their device key for authenticity.
