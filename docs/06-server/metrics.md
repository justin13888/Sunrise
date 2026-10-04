---
status: accepted
---

# Metrics catalogue

This is the complete set of Prometheus metrics the relay exposes or is to expose, and the contract
each one answers to. Every row is marked **current** (the tree emits it) or **target** (specified,
not built, and the issue that builds it is named). [`observability.md`](./observability.md)
§Metrics carries the gate-checked list of names the tree emits, extracted from the source; a name
marked current here is in that block, and a name marked target is not.

Everything below is normative. A new metric is added here first, in the same pull request that
emits it.

## Rules

1. **Naming.** `sunrise_<area>_<measure>_<unit>`. Counters end in `_total`, durations in
   `_seconds`, sizes in `_bytes`. Units are always base units (seconds, bytes), never ms or KiB.
2. **Types.** Counters only go up. Gauges are sampled at scrape time from authoritative state
   (a row count, a map length, a file size), or are process constants set once at startup. They are
   never maintained by paired inc/dec calls that can drift, and the registry offers no such call.
   Histograms use the fixed bucket sets named below, so dashboards and alerts can compare across
   releases.
3. **Labels are bounded and never identify anyone.** Only the names in §Label allowlist may appear.
   An account, device, stream, op, entity, blob or upload id, an email, an IP address, a raw path or
   a token MUST NOT appear as a label value, in any form (hashed included). The registry refuses a
   label name off the allowlist at the call, and
   `crates/sunrise-server/tests/metric-label-safety.rs` holds the rest (see §Enforcement).
4. **Exposure.** `/metrics` stays loopback-only (`srv.start.metrics_withheld` otherwise). An
   operator who wants remote scraping puts an authenticated scraper on the host. The reverse-proxy
   configurations shipped in [`deploy/`](../../deploy/) MUST NOT route `/metrics`: each answers it
   with its own `404`, and the `Deploy test` CI job asserts that from outside.
5. **Cost.** Recording a metric on a hot path MUST NOT take a lock. The registry
   (`crates/sunrise-server/src/metrics.rs#Metrics`) is a fixed table of `OnceLock` slots probed from
   a hash of the name and labels: a series is written once, on first touch, and every later
   observation is a probe and an atomic add. The only wait is two threads racing to create the same
   slot's series, once per series for the life of the process.
6. **Bounded series.** The table holds a fixed number of series (`metrics::CAPACITY`, 4096) and
   never grows. An observation that would need a new series in a full table, that carries a label
   off the allowlist, or that reuses a name under a second type is dropped and counted in
   `sunrise_metrics_series_dropped_total`. That counter is always present, so a zero is a reading.
   A debug build panics on the off-allowlist label instead, so a test that adds one fails at the
   call.

## Label allowlist

Each label's value set is closed, and the right-hand column bounds it. This table and
`crates/sunrise-server/src/metrics.rs#LABEL_ALLOWLIST` are the same list; the gate test fails if
they diverge.

| Label | Values | Bound |
|---|---|---|
| `endpoint` | The matched operation's OpenAPI path template in its `{param}` spelling, e.g. `/api/v1/devices/{device_id}`; `unmatched` for a request no operation matched | the route table (~25), plus one |
| `method` | The matched operation's HTTP verb; `-` for `unmatched` | 6 |
| `status` | HTTP status code actually returned, as three digits | ~15 |
| `kind` | frame or op-batch kind | the wire enum |
| `provider` | `apns`, `fcm`, `web` | 3 |
| `result` | `ok`, `failed`, `rejected`, `rate_limited`, `timeout`, `dropped` | 6 |
| `reason` | The typed error code (`crates/sunrise-error/codes.toml`), or the closed enum the metric's own row names where every failure shares one code | the code registry, or the row's enum |
| `scope` | `ip`, `account`, `device` | 3 |
| `direction` | `upload`, `download` | 2 |
| `state` | `active`, `revoked` | 2 |
| `version`, `commit` | build metadata, on `sunrise_build_info` only | 1 series per process |
| `wire_proto`, `crypto_suite` | negotiated protocol constants | small integers |

`le` appears on a histogram's `_bucket` lines. It is the exposition format's own bucket bound,
written by the renderer from the fixed bucket set, and no call site supplies it.

### Enforcement

`crates/sunrise-server/tests/metric-label-safety.rs` builds the server through
`sunrise_server::build_service`, drives every operation in the published OpenAPI description with
an id-shaped sentinel in every path parameter and the query string, plus one path nothing matches,
and scrapes `/metrics`. It asserts that only allowlisted label names appear, that every family has a
`# TYPE` line, that every label value is drawn from the set above and is not id-shaped, and that a
second pass with a different id adds no series. The one exemption from the id-shape check is
`commit`, a build constant whose full git SHA is id-shaped by design: it is held instead to
`unknown` or 7 to 64 lowercase hex digits, and to a single value per process. A route added later is
in the description, so it is driven without anyone adding it to the test.

## Catalogue

### Process and build

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_build_info` | gauge (=1) | `version`, `commit` | current | Build identity, so every other series can be joined to a release. `version` is the crate version; `commit` is `SUNRISE_BUILD_COMMIT` at compile time, or `unknown` when the build did not set it. |
| `sunrise_start_time_seconds` | gauge | — | current | Unix time the process started, from the server's clock; restarts show as steps. |
| `process_*` | standard | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | The Prometheus process collector: CPU, RSS, open fds, threads. |
| `sunrise_metrics_series_dropped_total` | counter | — | current | Observations the registry refused (Rule 6). Any increase is a defect at a call site. |

### HTTP

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_http_requests_total` | counter | `endpoint`, `method`, `status` | current | Every request that produced a response head, recorded by `api::observe::HttpMetrics`. |
| `sunrise_http_request_duration_seconds` | histogram (latency buckets) | `endpoint`, `method` | current | Time to the response head. For a buffered response that is the whole request; for the SSE `events` endpoint it is time to the first byte, because the stream is long-lived by design. |
| `sunrise_http_in_flight_requests` | gauge | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Requests currently being handled. Not built from the observer: a client that leaves mid-handler reaches no observer hook, so an increment there has no guaranteed decrement (Rule 2). |
| `sunrise_ratelimit_rejected_total` | counter | `endpoint`, `scope` | current | Requests refused with `429 RATE_LIMITED` under [`api.md`](./api.md) §Rate limits, by the matched route's template and by what the refusal counted against: `ip` for a route group or the failed-auth budget, `device` or `account` for a per-device or per-account budget (`account` also where a caller signed with no device). Recorded by `api::ratelimit`. |

### Authentication

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_auth_verify_total` | counter | `result`, `reason` | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Bearer verification outcomes. `reason` is set only when the result is not `ok`. |
| `sunrise_oidc_jwks_fetch_total` | counter | `result` | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Key-set fetches, including the throttled `kid`-miss refetch. |
| `sunrise_device_sig_rejected_total` | counter | `reason` | current | A device signature that failed verification, after the device resolved to an active row. Every one answers `AUTH_DEVICE_SIG_INVALID`, so `reason` is this closed enum instead: `skew` (the `Date` is outside the window), `bad_signature`, `malformed` (a header or body the scheme cannot read), `bad_device_key` (the registered key is unusable). An unknown or revoked device is refused before verification and is not counted here. |
| `sunrise_recovery_step_up_refused_total` | counter | — | current | |
| `sunrise_recovery_blob_fetch_total` | counter | — | current | |

### Accounts and devices

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_accounts` | gauge | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Account rows. |
| `sunrise_devices` | gauge | `state` | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Device rows by state. |
| `sunrise_account_create_total` | counter | — | current | |
| `sunrise_account_delete_total` | counter | — | current | Accounts erased, by the maintenance pass once their grace period has run or by `admin account delete --immediately`. A confirmed request that has not been erased yet is not counted; `admin stats` reports those as `accounts_pending_deletion`. |
| `sunrise_devices_register_total` | counter | — | current | |
| `sunrise_devices_revoke_total` | counter | — | current | |

### Sync and relay

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_sync_sessions_active` | gauge | — | current | Live sync sessions, sampled at scrape time after reaping expired and idle ones (`sync_session.rs`). |
| `sunrise_sync_streams_active` | gauge | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Open SSE event streams. |
| `sunrise_sync_session_total` | counter | — | current | |
| `sunrise_sync_refresh_total` | counter | — | current | |
| `sunrise_sync_stream_total` | counter | — | current | |
| `sunrise_sync_negotiate_refused_total` | counter | `reason` | current | A refused `POST /sync/session`. `reason` is the refusal's typed error code, `NegotiationError::as_error_code`. |
| `sunrise_sync_resume_conflict_total` | counter | — | current | |
| `sunrise_sync_ops_received_total` | counter | — | current | Ops in batches `POST /sync/ops` stored as fresh. A duplicate batch is re-acked and not counted here; `sunrise_relay_batch_duplicate_total` counts it. |
| `sunrise_sync_ops_delivered_total` | counter | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Ops written to subscriber streams. |
| `sunrise_sync_batch_ops` | histogram (count buckets) | — | current | Ops per fresh batch, observed beside `sunrise_sync_ops_received_total`. |
| `sunrise_sync_fanout_latency_seconds` | histogram (latency buckets) | — | target ([#366](https://github.com/justin13888/Sunrise/issues/366)) | From accepting a batch to flushing it to the last live subscriber. This is the server's share of the end-to-end sync budget in [`performance-budgets.md`](../10-cross-cutting/performance-budgets.md). |
| `sunrise_relay_append_failed_total` | counter | — | current | |
| `sunrise_relay_batch_duplicate_total` | counter | — | current | See [`observability.md`](./observability.md). |
| `sunrise_relay_batch_overlap_total` | counter | — | current | |
| `sunrise_relay_cursor_gap_total` | counter | — | current | |
| `sunrise_relay_log_bytes` | gauge | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Bytes held in the durable relay log. |
| `sunrise_relay_log_evicted_total` | counter | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Frames evicted by retention or the per-channel byte cap. |

### Blobs

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_blob_init_total`, `_chunk_total`, `_finalize_total`, `_fetch_total` | counter | — | current | |
| `sunrise_blob_hash_mismatch_total` | counter | — | current | |
| `sunrise_blob_bytes_total` | counter | `direction` | current | Ciphertext bytes moved: `upload` as each chunk is stored, `download` as each chunk is read for a fetch, so an abandoned fetch counts what was read for it. |
| `sunrise_blob_storage_bytes` | gauge | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Ciphertext bytes at rest. |
| `sunrise_blob_upload_duration_seconds` | histogram (transfer buckets) | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | From init to finalize. |
| `sunrise_blob_gc_deleted_total` | counter | — | current | Tombstoned blobs reclaimed by the maintenance pass, once `[storage] gc_grace_days` had passed and every active device had acknowledged the tombstone. Blobs removed with an erased account are not counted here. |

### Push

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_push_register_total` | counter | — | current | |
| `sunrise_push_dispatch_total` | counter | `provider`, `result` | current | One per wake-up push, by how it ended: `ok` delivered; `rejected` refused by the provider, including a dead token, which is deleted; `rate_limited` throttled by the provider past every retry, or not sent because the device was at its ten-a-minute cap; `failed` a server or transport error past every retry; `timeout` no answer past every retry; `dropped` the dispatch queue was full, so the wake was discarded before any device was looked up. Coalesced ops are not counted. No series exists until `[push]` configures a provider. |
| `sunrise_push_dispatch_duration_seconds` | histogram (latency buckets) | `provider` | current | Time for one provider round trip, observed per attempt, retries included. |

### Storage

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_db_query_duration_seconds` | histogram (latency buckets) | `endpoint` | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | Time spent in the store per request route. Per-statement labels are forbidden: an unbounded set in disguise. |
| `sunrise_db_busy_total` | counter | — | target ([#435](https://github.com/justin13888/Sunrise/issues/435)) | `SQLITE_BUSY` retries, after the [#358](https://github.com/justin13888/Sunrise/issues/358) `busy_timeout`. |
| `sunrise_db_size_bytes` | gauge | — | current | The main database file plus its WAL, sampled at scrape time. An in-memory store has no series. |

## What became of the counter-only names

[#356](https://github.com/justin13888/Sunrise/issues/356) replaced a registry of 25 unlabelled
counters. Each is kept, gained a label, folded, or deleted:

| Before | After |
|---|---|
| `sunrise_device_sig_rejected_total` | Kept, gains `reason`. |
| `sunrise_sync_negotiate_refused_total` | Kept, gains `reason`. |
| `sunrise_devices_list_total` | Folded into `sunrise_http_requests_total{endpoint="/api/v1/devices",method="GET"}`, which counts the same requests and their outcome. |
| `sunrise_push_apns_total`, `sunrise_push_fcm_total`, `sunrise_push_web_total` | Deleted. They were reachable only through `LoggingProvider`, which nothing constructed and which [#362](https://github.com/justin13888/Sunrise/issues/362) removed; `sunrise_push_dispatch_total{provider,result}` supersedes them. |
| Every other name | Kept unchanged, as an unlabelled counter. |

## Bucket sets

| Name | Upper bounds (seconds, or a count) |
|---|---|
| latency | 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 |
| transfer | 0.1, 0.5, 1, 2.5, 5, 10, 30, 60, 120, 300 |
| count | 1, 2, 4, 8, 16, 32, 64, 128, 256, 512 |

These are `metrics::LATENCY_BUCKETS`, `TRANSFER_BUCKETS` and `COUNT_BUCKETS`. The latency set
places a bound at the 0.5 s p99 sync target, so that SLO can be read straight from
`_bucket{le="0.5"}`.

## Alerts this catalogue exists to feed

These are the minimum alert rules the self-hosting guide ships once the metrics they read are
current:

- `sunrise_sync_fanout_latency_seconds` p99 > 0.1 s for 10 min. That is the relay's leg of the 500 ms
  end-to-end budget, so a breach here spends headroom before users feel it.
- A 5xx ratio on `sunrise_http_requests_total` above 1 % for 5 min.
- `sunrise_ratelimit_rejected_total` rising with `scope="account"`. That means a client is looping, not an attack.
- `sunrise_relay_cursor_gap_total` increasing at all, since a gap is data a device will never receive from the relay.
- `sunrise_blob_hash_mismatch_total` increasing at all.
- `sunrise_db_busy_total` sustained above zero.
- `sunrise_metrics_series_dropped_total` increasing at all.
