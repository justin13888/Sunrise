---
status: accepted
---

# Observability

Operate the server without violating the E2EE guarantee.

## What we log

- Aggregate request rates (per endpoint, per status).
- Aggregate WS connection counts.
- Aggregate per-account-bucket op rates, keyed by **`account_h`** — a hashed account ID, never the raw account ID.
- Slow query logs (with no payload bodies).
- Error frequencies by error code.

`account_h` is defined as:

```
account_h = BLAKE3(account_id || server_log_salt, 4)   # lowercase hex, 8 chars
```

`server_log_salt` is generated once at server initialization and persisted under `<data_dir>/log_salt.bin` (mode 0600). Server logs use `account_h` everywhere; **no email-tagged buffer exists** at any point.

## What we *never* log

- Op envelopes or any subset thereof.
- **Account emails — anywhere, ever.** No buffer, no transient store, no rotating log carries email.
- Push tokens.
- Any plaintext, anywhere.
- Raw `account_id`, `device_id`, `stream_id`, `op_id`, `entity_id` — only their hashed forms (`account_h`, `stream_h`, etc.).
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
sunrise_push_dispatch_total{provider="apns",result="ok"}
sunrise_quota_exceeded_total{kind="storage"}
sunrise_db_query_seconds{op="…"}
```

### Label allowlist

Only the following label names may appear on any Prometheus metric:

```
endpoint     (path template, e.g. "/api/v1/streams/:id")
method       (HTTP verb)
status       (HTTP status code)
kind         (frame kind, OpBatch | Subscribe | …)
provider     (apns | fcm | web | google | …)
result       (ok | failed | rate_limited | …)
plan_tier    (free | pro)
wire_proto   (1)
crypto_suite (1)
```

Forbidden labels include `account_id`, `stream_id`, `device_id`, `email`, `email_hash`, `ip`, `path` (raw with id), `op_id`, `entity_id`. CI test `tests/metric-label-safety.rs` parses Prometheus exposition output and asserts that only allowlisted label names appear.

## Tracing

OTel-compatible tracing with a sampling rate (1% prod, 100% staging) and span attributes scrubbed of user identifiers. Spans cover: connection lifecycle, op-batch ingress/egress, push dispatch.

Span attributes pass through `tracing::Subscriber` → `sunrise_telemetry::span_redactor` → exporter. The redactor is the only allowed exporter shim; it strips `account_id`, `device_id`, `email`, `op_id`, and raw path segments matching id patterns. Unit test `tests/span-redaction.rs` asserts redaction on every `tracing::span!` call site (the test scans the codebase for span macros and dry-runs them).

## Health

- `GET /api/v1/health` returns 200 + minimal JSON.
- A deeper readiness check at `/api/v1/health?deep=1` verifies DB, object store, and disk free ratio. See [`api.md`](./api.md) for the exact contract.

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

Visible in the user's "Security" page on the web app, derived from a per-account audit log. Retention:

- **Managed: 30 days** (formerly 90; unified to 30 across the system per the v1 retention policy).
- **Self-host:** configurable via `[observability] audit_retention_days = 30` (default). A cron job at 02:00 UTC deletes expired records. There is no separate `auth_log_retention_hours` setting — server logs use `account_h` everywhere; no email-tagged buffer exists.

## Privacy commitments to users

The privacy policy explicitly states what we keep and for how long. The numbers in this spec are what we commit to. Any operational change that increases retention requires a user-visible privacy policy update with a 30-day notice.

## Diagnostics from the client

A user can opt in to send a diagnostics package to support. The package is a curated bundle (no plaintext content; just sync stats, error codes, version info, op counts). The user is shown the bundle contents before sending and signs it with their device key for authenticity.
