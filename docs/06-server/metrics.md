---
status: accepted
---

# Metrics catalogue

This is the complete set of Prometheus metrics the relay is to expose, and the contract each one
answers to. [`observability.md`](./observability.md) §Metrics remains the extracted, gate-checked
record of what the tree emits **today**. This document is the target that record converges on,
tracked by [#356](https://github.com/justin13888/Sunrise/issues/356). A name listed here and absent from that extracted block is not built.

Everything below is normative once built. A new metric is added here first, in the same pull
request that emits it.

## Rules

1. **Naming.** `sunrise_<area>_<measure>_<unit>`. Counters end in `_total`, durations in
   `_seconds`, sizes in `_bytes`. Units are always base units (seconds, bytes), never ms or KiB.
2. **Types.** Counters only go up. Gauges are sampled at scrape time from authoritative state
   (a row count, a map length), never maintained by paired inc/dec calls that can drift. Histograms
   use the fixed bucket sets named below, so dashboards and alerts can compare across releases.
3. **Labels are bounded and never identify anyone.** Only the names in §Label allowlist may appear.
   An account, device, stream, op, entity, blob or upload id, an email, an IP address, a raw path or
   a token MUST NOT appear as a label value, in any form (hashed included). The allowlist test named
   in [`observability.md`](./observability.md) §Label allowlist becomes a required gate with the
   first labelled metric.
4. **Exposure.** `/metrics` stays loopback-only (`srv.start.metrics_withheld` otherwise). An
   operator who wants remote scraping puts an authenticated scraper on the host. The reverse-proxy
   configurations shipped by [#355](https://github.com/justin13888/Sunrise/issues/355) MUST NOT route `/metrics`.
5. **Cost.** Recording a metric on a hot path MUST NOT take a lock that request handling also
   takes. The current `Mutex<BTreeMap>` registry (`crates/sunrise-server/src/metrics.rs#Metrics`) is
   replaced by a pre-registered, lock-free registry as part of [#356](https://github.com/justin13888/Sunrise/issues/356).

## Label allowlist

This extends the allowlist in [`observability.md`](./observability.md) §Label allowlist, which it
supersedes when [#356](https://github.com/justin13888/Sunrise/issues/356) lands. Each label's value set is closed, and the right-hand column bounds it.

| Label | Values | Bound |
|---|---|---|
| `endpoint` | OpenAPI path template, e.g. `/api/v1/devices/{device_id}` | the route table (~25) |
| `method` | HTTP verb | 5 |
| `status` | HTTP status code actually returned | ~15 |
| `kind` | frame or op-batch kind | the wire enum |
| `provider` | `apns`, `fcm`, `web` | 3 |
| `result` | `ok`, `failed`, `rejected`, `rate_limited`, `timeout` | 5 |
| `reason` | the typed error code (`crates/sunrise-error/codes.toml`) | the code registry |
| `scope` | `ip`, `account`, `device` | 3 |
| `direction` | `upload`, `download` | 2 |
| `state` | `active`, `revoked` | 2 |
| `version`, `commit` | build metadata, on `sunrise_build_info` only | 1 series per process |
| `wire_proto`, `crypto_suite` | negotiated protocol constants | small integers |

## Catalogue

"Today" marks a name the extracted block in [`observability.md`](./observability.md) already
carries. For those, the target column records any change of shape.

### Process and build

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_build_info` | gauge (=1) | `version`, `commit` | Build identity, so every other series can be joined to a release. |
| `sunrise_start_time_seconds` | gauge | — | Unix time the process started; restarts show as steps. |
| `process_*` | standard | — | The Prometheus process collector: CPU, RSS, open fds, threads. |

### HTTP

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_http_requests_total` | counter | `endpoint`, `method`, `status` | Every completed request. |
| `sunrise_http_request_duration_seconds` | histogram (latency buckets) | `endpoint`, `method` | Time to the last response byte. For the SSE `events` endpoint this is time to the first byte, because the stream is long-lived by design. |
| `sunrise_http_in_flight_requests` | gauge | — | Requests currently being handled. |
| `sunrise_ratelimit_rejected_total` | counter | `endpoint`, `scope` | Requests refused with 429 by the policy that [#355](https://github.com/justin13888/Sunrise/issues/355) defines. |

### Authentication

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_auth_verify_total` | counter | `result`, `reason` | Bearer verification outcomes. `reason` is set only when the result is not `ok`. |
| `sunrise_oidc_jwks_fetch_total` | counter | `result` | Key-set fetches, including the throttled `kid`-miss refetch. |
| `sunrise_device_sig_rejected_total` | counter | `reason` | **Today, unlabelled.** Gains `reason` (skew, unknown device, bad signature, revoked). |
| `sunrise_recovery_step_up_refused_total` | counter | — | Today. |
| `sunrise_recovery_blob_fetch_total` | counter | — | Today. |

### Accounts and devices

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_accounts` | gauge | — | Account rows. |
| `sunrise_devices` | gauge | `state` | Device rows by state. |
| `sunrise_account_create_total` | counter | — | Today. |
| `sunrise_account_delete_total` | counter | — | Account deletion ([#359](https://github.com/justin13888/Sunrise/issues/359)). |
| `sunrise_devices_register_total` | counter | — | Today. |
| `sunrise_devices_revoke_total` | counter | — | Today. |
| `sunrise_devices_list_total` | counter | — | Today. **Retired** once `sunrise_http_requests_total` exists, which subsumes it. |

### Sync and relay

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_sync_sessions_active` | gauge | — | Live sync sessions (`sync_session.rs`). |
| `sunrise_sync_streams_active` | gauge | — | Open SSE event streams. |
| `sunrise_sync_session_total` | counter | — | Today. |
| `sunrise_sync_refresh_total` | counter | — | Today. |
| `sunrise_sync_stream_total` | counter | — | Today. |
| `sunrise_sync_negotiate_refused_total` | counter | `reason` | **Today, unlabelled.** Gains `reason` (wire, schema floor, suite, capability). |
| `sunrise_sync_resume_conflict_total` | counter | — | Today. |
| `sunrise_sync_ops_received_total` | counter | — | Ops accepted by `POST /sync/ops`. |
| `sunrise_sync_ops_delivered_total` | counter | — | Ops written to subscriber streams. |
| `sunrise_sync_batch_ops` | histogram (count buckets) | — | Ops per uploaded batch. |
| `sunrise_sync_fanout_latency_seconds` | histogram (latency buckets) | — | From accepting a batch to flushing it to the last live subscriber. This is the server's share of the end-to-end sync budget in [`performance-budgets.md`](../10-cross-cutting/performance-budgets.md) ([#366](https://github.com/justin13888/Sunrise/issues/366)). |
| `sunrise_relay_append_failed_total` | counter | — | Today. |
| `sunrise_relay_batch_duplicate_total` | counter | — | Today (see [`observability.md`](./observability.md)). |
| `sunrise_relay_batch_overlap_total` | counter | — | Today. |
| `sunrise_relay_cursor_gap_total` | counter | — | Today. |
| `sunrise_relay_log_bytes` | gauge | — | Bytes held in the durable relay log. |
| `sunrise_relay_log_evicted_total` | counter | — | Frames evicted by retention or the per-channel byte cap. |

### Blobs

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_blob_init_total`, `_chunk_total`, `_finalize_total`, `_fetch_total` | counter | — | Today. |
| `sunrise_blob_hash_mismatch_total` | counter | — | Today. |
| `sunrise_blob_bytes_total` | counter | `direction` | Ciphertext bytes moved. |
| `sunrise_blob_storage_bytes` | gauge | — | Ciphertext bytes at rest. |
| `sunrise_blob_upload_duration_seconds` | histogram (transfer buckets) | — | From init to finalize. |
| `sunrise_blob_gc_deleted_total` | counter | — | Blobs reclaimed by GC ([#359](https://github.com/justin13888/Sunrise/issues/359)). |

### Push

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_push_register_total` | counter | — | Today. |
| `sunrise_push_dispatch_total` | counter | `provider`, `result` | Replaces today's unreachable `sunrise_push_{apns,fcm,web}_total` once delivery exists ([#362](https://github.com/justin13888/Sunrise/issues/362)). |
| `sunrise_push_dispatch_duration_seconds` | histogram (latency buckets) | `provider` | Time for the provider round trip. |

### Storage

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `sunrise_db_query_duration_seconds` | histogram (latency buckets) | `endpoint` | Time spent in the store per request route. Per-statement labels are forbidden: an unbounded set in disguise. |
| `sunrise_db_busy_total` | counter | — | `SQLITE_BUSY` retries, after the [#358](https://github.com/justin13888/Sunrise/issues/358) `busy_timeout`. |
| `sunrise_db_size_bytes` | gauge | — | The main database file plus its WAL. |

## Bucket sets

| Name | Upper bounds (seconds, or a count) |
|---|---|
| latency | 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 |
| transfer | 0.1, 0.5, 1, 2.5, 5, 10, 30, 60, 120, 300 |
| count | 1, 2, 4, 8, 16, 32, 64, 128, 256, 512 |

The latency set places a bound at the 0.5 s p99 sync target, so that SLO can be read straight
from `_bucket{le="0.5"}`.

## Alerts this catalogue exists to feed

These are the minimum alert rules the self-hosting guide ships once the metrics exist ([#356](https://github.com/justin13888/Sunrise/issues/356)):

- `sunrise_sync_fanout_latency_seconds` p99 > 0.1 s for 10 min. That is the relay's leg of the 500 ms
  end-to-end budget, so a breach here spends headroom before users feel it.
- A 5xx ratio on `sunrise_http_requests_total` above 1 % for 5 min.
- `sunrise_ratelimit_rejected_total` rising with `scope="account"`. That means a client is looping, not an attack.
- `sunrise_relay_cursor_gap_total` increasing at all, since a gap is data a device will never receive from the relay.
- `sunrise_blob_hash_mismatch_total` increasing at all.
- `sunrise_db_busy_total` sustained above zero.
