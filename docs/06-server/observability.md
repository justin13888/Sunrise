---
status: accepted
---

# Observability

Operate the server without violating the E2EE guarantee.

> **Implementation status.** What is built is the identifier-hashing surface
> (`logging::account_h` / `id_h`), the hand-assembled request trace layer
> (`logging::trace_layer`) with its redaction regression test
> (`crates/sunrise-server/tests/logging.rs`), and an in-process counter registry
> exposed at `/metrics` (`metrics.rs`). **Not built:** labelled metrics of any
> kind, histograms, OTel tracing and sampling, the deep health check, alerting,
> and the per-account audit log. Each section says which it is.

## What we log

Target set:

- Aggregate request rates (per endpoint, per status).
- Aggregate WS connection counts.
- Aggregate per-account-bucket op rates, keyed by **`account_h`** — a hashed account ID, never the raw account ID.
- Slow query logs (with no payload bodies).
- Error frequencies by error code.

**Built today:** a request span per HTTP request carrying the method and a
*templated* target only, plus event lines that already use `account_h` / `id_h`
(`srv.auth.ok`, `srv.auth.rejected`, `srv.ws.refreshed`,
`srv.ws.refresh_rejected`, `srv.ws.refresh_identity_mismatch`,
`srv.start.refused`). Per-endpoint/status rates, connection counts, per-account
op rates and slow-query logs have no implementation; error frequency is
recoverable from the `err_code` field on rejection lines, not from a metric.

`account_h` is defined as:

```
account_h = BLAKE3(account_id)[..4]   # lowercase hex, 8 chars
```

Implemented in `sunrise_server::logging::account_h`; `id_h` is the same
construction over a raw 16-byte id (`stream_h`, and the relay's per-session
account namespace). Server logs use these everywhere; **no email-tagged buffer
exists** at any point.

> **Amended 2026-08.** This section previously specified
> `BLAKE3(account_id || server_log_salt, 4)`, with `server_log_salt` generated
> once at initialisation and persisted at `<data_dir>/log_salt.bin` (mode
> 0600). The salt is **not** implemented, and the omission is deliberate rather
> than pending.
>
> A salt earns its keep when the pre-image space is small enough to enumerate —
> which is the case for the email addresses [logging.md
> §6.1](../10-cross-cutting/logging.md#61-email-addresses) was guarding.
> Sunrise account ids are not that: `Store::resolve_account` mints them as 16
> random bytes with no relation to the OIDC subject or the email, so a
> truncated hash has a 2^128 pre-image space and nothing to brute-force back
> to. The salt's other job — stopping a client-side and a server-side hash of
> the same account from being joined without operator action — is already done
> by clients hashing their *own* ids under a device-local salt.
>
> **What this gives up:** two servers sharing an account id produce the same
> `account_h`, where per-server salts would not. In a self-host world that is a
> non-issue; in a fleet it is a correlation an operator could otherwise have
> withheld from themselves. **If an id ever becomes derivable from
> user-supplied input, the salt has to come back** — and so does the file.

## What we *never* log

- Op envelopes or any subset thereof.
- **Account emails — anywhere, ever.** No buffer, no transient store, no rotating log carries email.
- Push tokens. (They are *logged* nowhere; they are *stored* in plaintext — see [`relay-and-blob-storage.md`](./relay-and-blob-storage.md).)
- Any plaintext, anywhere.
- Raw `account_id`, `device_id`, `stream_id`, `op_id`, `entity_id` — only their hashed forms (`account_h`, `stream_h`, etc.).
- IPs joined to user IDs (beyond 24h).

## Metrics (Prometheus)

Naming convention: `sunrise_<area>_<measure>`.

`metrics.rs` is an in-process `BTreeMap<String, AtomicU64>` rendered as
Prometheus text at `/metrics`. It supports **counters only** — no gauges, no
histograms — and every call site increments a bare, unlabelled name. The
complete set the server emits today:

```
sunrise_account_create_total
sunrise_blob_chunk_total
sunrise_blob_fetch_total
sunrise_blob_finalize_total
sunrise_blob_hash_mismatch_total
sunrise_blob_init_total
sunrise_device_sig_rejected_total
sunrise_devices_list_total
sunrise_devices_register_total
sunrise_devices_revoke_total
sunrise_push_register_total
sunrise_relay_append_failed_total
sunrise_relay_cursor_gap_total
sunrise_sync_token_expired_total
sunrise_sync_token_refresh_rejected_total
sunrise_sync_token_refreshed_total
sunrise_sync_unauthenticated_total
sunrise_push_apns_total / _fcm_total / _web_total   (LoggingProvider; never reached)
```

`/metrics` is mounted at the router root, and **only when the listener binds
loopback** — a non-loopback bind withholds the route and logs
`srv.start.metrics_withheld`. See [`overview.md`](./overview.md)
§operational-requirements and [`self-hosting.md`](./self-hosting.md)
§operator-surfaces. It was previously mounted unconditionally with no
authentication and no bind check.

Target examples, none of which exist:

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

### Label allowlist — NOT ENFORCED

No metric carries a label today — `Metrics::add` takes a name and nothing else,
and `render` merely passes a `{…}` in a name through verbatim — so the allowlist
is vacuously satisfied rather than checked. **The CI test named below does not
exist**: there is no `crates/sunrise-server/tests/metric-label-safety.rs`. The
allowlist is still the contract for the first metric that takes a label.

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

Forbidden labels include `account_id`, `stream_id`, `device_id`, `email`, `email_hash`, `ip`, `path` (raw with id), `op_id`, `entity_id`. A CI test parsing the exposition output and asserting only allowlisted label names appear is the intended gate; it is **not written**.

## Tracing

**Not implemented as specified.** There is no `sunrise-telemetry` crate, no
`span_redactor`, no OTel exporter, no sampling rate, and no
`tests/span-redaction.rs`.

What exists is the redaction *at the source*, which is the stronger placement:
`logging::trace_layer` assembles `tower_http`'s `TraceLayer` by hand so the span
records the HTTP method and a **templated** target from
`sunrise_log::templatize_path` — query dropped, opaque id segments replaced —
and nothing else from the request ever reaches a field. That is deliberate
rather than incidental: the stock `MakeSpan` records `http.uri`, which is where
a bearer would sit if the `?access_token=` fallback existed. The regression test
is `crates/sunrise-server/tests/logging.rs`, and
`docs/10-cross-cutting/logging.md` §6.3 bans `Plain::expose` in this module with
a `log-redaction` CI gate over the path.

The target — OTel-compatible tracing with a sampling rate (1% prod, 100%
staging), span attributes scrubbed of user identifiers, spans covering
connection lifecycle, op-batch ingress/egress and push dispatch — is unchanged
and unbuilt.

## Health

- `GET /api/v1/health` returns 200 + `{"status":"ok"}`, unconditionally: the
  handler reads no state, so it is liveness only.
- A deeper readiness check at `/api/v1/health?deep=1` verifies DB, object store,
  and disk free ratio. **Not implemented** — the query parameter is ignored, so
  wiring `?deep=1` as a readiness probe today yields an unconditional 200. See
  [`api.md`](./api.md) for the contract it will have.

## Alerts (managed) — NOT IMPLEMENTED

No alerting exists, and several triggers below have no metric to fire from.

| Alert | Trigger |
|---|---|
| WS connection drop spike | >5x baseline |
| Op ingress error rate | >1% over 5m |
| S3 errors | any non-zero rate over 5m |
| Push dispatch failure rate | >5% over 15m |
| Storage growth anomaly | >2x weekly trendline |
| Auth failure rate | >5x baseline over 10m |

## Audit trail (per-account, retained briefly) — NOT IMPLEMENTED

There is no audit table in `store.rs`'s schema, no writer, no "Security" page,
and no retention job. `[observability]` is rejected outright by the config
parser (see [`self-hosting.md`](./self-hosting.md) §"Not yet wired"), so
`audit_retention_days` cannot be set. Two of the five actions below could not be
recorded anyway: "recovery blob fetched" and "account deleted" have no route.

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
