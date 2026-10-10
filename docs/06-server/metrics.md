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
2. **Types.** Counters only go up. A counter is incremented where its event happens, or, where the
   authoritative count lives outside the registry (the kernel's CPU time, a count kept beside a
   call site that holds no registry), copied from that count at scrape time
   (`Metrics::set_counter`); one name has one source. Gauges are sampled at scrape time from
   authoritative state (a row count, a map length, a file size), or are process constants set once
   at startup. They are never maintained by paired inc/dec calls that can drift, and the registry
   offers no such call. The one counted shape allowed is an occupancy
   (`crates/sunrise-server/src/metrics.rs#Occupancy`): each holder takes a guard whose `Drop`
   releases it, so every exit — completion, an early return, a panic, a dropped future — releases
   it and nothing can forget to. Histograms use the fixed bucket sets named below, so dashboards and
   alerts can compare across releases.
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
| `result` | `ok`, `failed`, `rejected`, `rate_limited`, `timeout`, `dropped`; and on `sunrise_pairing_total`, `opened`, `relayed`, `gone`, `aborted`, `expired` | 11 |
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
| `process_*` | standard | — | current | The Prometheus process collector, sampled from `/proc/self` at scrape time (`metrics::process`): `process_cpu_seconds_total` (counter), `process_resident_memory_bytes`, `process_virtual_memory_bytes`, `process_threads`, `process_open_fds`, `process_max_fds` and `process_start_time_seconds`. Linux only, which is what the relay ships as; another platform has no series rather than zeros. `process_max_fds` is absent when the limit is unlimited, `process_start_time_seconds` when the boot time cannot be read. These names are outside the `sunrise_` pattern, so the extracted block in `observability.md` does not list them. |
| `sunrise_metrics_series_dropped_total` | counter | — | current | Observations the registry refused (Rule 6). Any increase is a defect at a call site. |

### HTTP

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_http_requests_total` | counter | `endpoint`, `method`, `status` | current | Every request that produced a response head, recorded by `api::observe::HttpMetrics`. |
| `sunrise_http_request_duration_seconds` | histogram (latency buckets) | `endpoint`, `method` | current | Time to the response head. For a buffered response that is the whole request; for the SSE `events` endpoint it is time to the first byte, because the stream is long-lived by design. |
| `sunrise_http_in_flight_requests` | gauge | — | current | Matched requests inside the handler stack right now, the scrape that reads it included. An occupancy (Rule 2) held by `api::observe::RequestMeter` for the request's whole future, so a client that leaves mid-handler releases it when the future is dropped, which no observer hook reports. It ends with the response head, as the duration does: an open event stream is `sunrise_sync_streams_active`. A request no route matched is answered without entering the stack and is not counted. |
| `sunrise_ratelimit_rejected_total` | counter | `endpoint`, `scope` | current | Requests refused with `429 RATE_LIMITED` under [`api.md`](./api.md) §Rate limits, by the matched route's template and by what the refusal counted against: `ip` for a route group or the failed-auth budget, `device` or `account` for a per-device or per-account budget (`account` also where a caller signed with no device). Recorded by `api::ratelimit`. |

### Authentication

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_auth_verify_total` | counter | `result`, `reason` | current | Bearer verification outcomes, counted by `auth::Metered`, which every verifier the server installs sits behind. `ok`, with no `reason`; `rejected` with `reason` `missing` (no or malformed token), `invalid` (signature or claims), or `expired`; `failed` with `reason` `unreachable`, the issuer's key set could not be read, which is the relay's failure rather than the caller's. A closed enum rather than the error code, because `missing` and `invalid` both answer `AUTH_TOKEN_INVALID`. |
| `sunrise_oidc_jwks_fetch_total` | counter | `result` | current | Key-set fetches by the OIDC verifier, `ok` or `failed`, including the throttled `kid`-miss refetch. A key served from the cache is not a fetch. |
| `sunrise_device_sig_rejected_total` | counter | `reason` | current | A device signature that failed verification, after the device resolved to an active row. Every one answers `AUTH_DEVICE_SIG_INVALID`, so `reason` is this closed enum instead: `skew` (the `Date` is outside the window), `bad_signature`, `malformed` (a header or body the scheme cannot read), `bad_device_key` (the registered key is unusable). An unknown or revoked device is refused before verification and is not counted here. |
| `sunrise_recovery_step_up_refused_total` | counter | — | current | |
| `sunrise_recovery_blob_fetch_total` | counter | — | current | |

### Accounts and devices

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_accounts` | gauge | — | current | Account rows, counted at scrape time (`Store::stats`), those pending deletion included. |
| `sunrise_devices` | gauge | `state` | current | Device rows by state, `active` or `revoked`, counted at scrape time in the same read. |
| `sunrise_account_create_total` | counter | — | current | |
| `sunrise_account_delete_total` | counter | — | current | Accounts erased, by the maintenance pass once their grace period has run or by `admin account delete --immediately`. A confirmed request that has not been erased yet is not counted; `admin stats` reports those as `accounts_pending_deletion`. |
| `sunrise_devices_register_total` | counter | — | current | |
| `sunrise_devices_revoke_total` | counter | — | current | |

### Sync and relay

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_sync_sessions_active` | gauge | — | current | Live sync sessions, sampled at scrape time after reaping expired and idle ones (`sync_session.rs`). |
| `sunrise_sync_streams_active` | gauge | — | current | Open SSE event streams, replaying or live: the guards `drain::Drain::track_stream` holds for each stream's task, which ends when the client goes away. |
| `sunrise_sync_session_total` | counter | — | current | |
| `sunrise_sync_refresh_total` | counter | — | current | |
| `sunrise_sync_stream_total` | counter | — | current | |
| `sunrise_sync_negotiate_refused_total` | counter | `reason` | current | A refused `POST /sync/session`. `reason` is the refusal's typed error code, `NegotiationError::as_error_code`. |
| `sunrise_sync_resume_conflict_total` | counter | — | current | |
| `sunrise_sync_ops_received_total` | counter | — | current | Ops in batches `POST /sync/ops` stored as fresh. A duplicate batch is re-acked and not counted here; `sunrise_relay_batch_duplicate_total` counts it. |
| `sunrise_sync_ops_delivered_total` | counter | — | current | Ops handed to subscriber streams, by the durable replay a stream opens with and by the live fan-out after it, counted once the event is queued for the response body. A stream's own batch, which it skips, is not counted. The count rides with the frame (`RelayFrame::n_ops`, `relay_frames.n_ops`), so no delivery decodes one. |
| `sunrise_sync_batch_ops` | histogram (count buckets) | — | current | Ops per fresh batch, observed beside `sunrise_sync_ops_received_total`. |
| `sunrise_sync_fanout_latency_seconds` | histogram (latency buckets) | — | current | From accepting a batch to handing it to the last live subscriber's event stream. The clock starts in `POST /sync/ops` once the signed body has been read and verified, so the rate charge and the durable append are inside it; it stops when the last stream that `relay::RelayHub::publish` reached has queued the event for its response body (`relay::FanoutClock`). The socket write after that belongs to the HTTP stack and is not seen. One observation per fresh batch that was delivered to at least one live stream: a batch with no stream open is not a fan-out and is not observed, nor is one that only its author's own stream received (that stream skips its own batch, and a skip closes its copy without counting as a delivery), and neither is one whose receiving stream ended or lagged past the broadcast buffer before delivering it. This is the server's share of the end-to-end sync budget in [`performance-budgets.md`](../10-cross-cutting/performance-budgets.md). |
| `sunrise_relay_append_failed_total` | counter | — | current | |
| `sunrise_relay_batch_duplicate_total` | counter | — | current | See [`observability.md`](./observability.md). |
| `sunrise_relay_batch_overlap_total` | counter | — | current | |
| `sunrise_relay_cursor_gap_total` | counter | — | current | |
| `sunrise_relay_log_bytes` | gauge | — | current | Ciphertext bytes held in the durable relay log, summed at scrape time. |
| `sunrise_relay_log_evicted_total` | counter | — | current | Frames evicted by the age bound or the per-channel byte cap, counted by the append whose transaction deleted them. Frames removed with an erased account are not evictions and are not counted. |

### Blobs

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_blob_init_total`, `_chunk_total`, `_finalize_total`, `_fetch_total` | counter | — | current | |
| `sunrise_blob_hash_mismatch_total` | counter | — | current | |
| `sunrise_blob_bytes_total` | counter | `direction` | current | Ciphertext bytes moved: `upload` as each chunk is stored, `download` as each chunk is read for a fetch, so an abandoned fetch counts what was read for it. |
| `sunrise_blob_storage_bytes` | gauge | — | target ([#510](https://github.com/justin13888/Sunrise/issues/510)) | Ciphertext bytes at rest. |
| `sunrise_blob_upload_duration_seconds` | histogram (transfer buckets) | — | target ([#510](https://github.com/justin13888/Sunrise/issues/510)) | From init to finalize. |
| `sunrise_blob_gc_deleted_total` | counter | — | current | Tombstoned blobs reclaimed by the maintenance pass, once `[storage] gc_grace_days` had passed and every active device had acknowledged the tombstone. Blobs removed with an erased account are not counted here. |

### Push

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_push_register_total` | counter | — | current | |
| `sunrise_push_dispatch_total` | counter | `provider`, `result` | current | One per wake-up push, by how it ended: `ok` delivered; `rejected` refused by the provider, including a dead token, which is deleted; `rate_limited` throttled by the provider past every retry, or not sent because the device was at its ten-a-minute cap; `failed` a server or transport error past every retry; `timeout` no answer past every retry; `dropped` the dispatch queue was full, so the wake was discarded before any device was looked up. Coalesced ops are not counted. No series exists until `[push]` configures a provider. |
| `sunrise_push_dispatch_duration_seconds` | histogram (latency buckets) | `provider` | current | Time for one provider round trip, observed per attempt, retries included. |

### Pairing

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_pairing_total` | counter | `result` | current | The pairing rendezvous (`api/pairing.rs`), one per outcome: `opened` a new device's first message opened a session, which is one pair attempt; `relayed` any later message was buffered; `gone` a send or receive named no live session of the caller's account, including a fourth message from one role, which drops the session; `rate_limited` opening a session was over a pair-attempt limit or the session table was full; `aborted` an abort dropped a live session; `expired` a session reached its 300 s lifetime and was swept. |

### Storage

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `sunrise_db_query_duration_seconds` | histogram (latency buckets) | `endpoint` | current | Time one request spent in the store, summed over every store operation it took (`Store::tx`), the wait for the connection included, by the matched route's template; one observation per request that took the store at all, recorded by `api::observe::RequestMeter`. An event stream's replay reads run in its own task after the response head and are not part of its request. Per-statement labels are forbidden: an unbounded set in disguise. |
| `sunrise_db_busy_total` | counter | — | current | Store statements that failed with `SQLITE_BUSY` once the [#358](https://github.com/justin13888/Sunrise/issues/358) `busy_timeout` had run out: another connection (an `admin` command, a backup) held the database longer than the operator allowed. SQLite's own retries inside the timeout are not visible and not counted. Process-wide, counted where a SQLite error becomes a `StoreError` and copied in at scrape time. |
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
- `sunrise_db_busy_total` increasing at all, since each one is a request the store refused.
- `sunrise_metrics_series_dropped_total` increasing at all.
