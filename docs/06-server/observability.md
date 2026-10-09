---
status: accepted
---

# Observability

Operate the server without violating the E2EE guarantee.

> **Implementation status.** What is built is the identifier-hashing surface
> (`logging::account_h` / `id_h`), the request-log observer
> (`api::observe::RequestLog`) with its redaction tests — in-module in
> `api/observe.rs` and end-to-end in `crates/sunrise-server/tests/logging.rs` —
> and an in-process counter registry
> exposed at `/metrics` (`metrics.rs`), and redacted OpenTelemetry tracing with
> a capped sampler behind an off-by-default `[observability]` table
> (`crates/sunrise-telemetry`, §Tracing). **Not built:** labelled metrics of any
> kind, histograms, the deep health check's
> disk-free-ratio probe, alerting, and the per-account audit log. Each section says which it is.

## What we log

Target set:

- Aggregate request rates (per endpoint, per status).
- Aggregate WS connection counts.
- Aggregate per-account-bucket op rates, keyed by **`account_h`** — a hashed account ID, never the raw account ID.
- Slow query logs (with no payload bodies).
- Error frequencies by error code.

**Built today:** a request span per HTTP request carrying the method and a
*templated* endpoint only, plus the event set below. Per-endpoint/status rates,
connection counts, per-account op rates and slow-query logs have no
implementation; error frequency is recoverable from the `err_code` field on
rejection lines, not from a metric.

The 49 `ev` names the server emits, complete:

<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:
     grep -rhoE 'ev = "srv\.[a-z0-9_.]+"' crates/sunrise-server/src | sort -u
     `.github/scripts/observability-catalog-gate.py` reads the command above out
     of this comment and runs it, then diffs the result against the block below
     in both directions, so the block cannot go stale unnoticed and the pattern
     cannot drift from the gate. It checks the count in the sentence before the
     block too, which is why that count is a numeral.
     `Last extracted` names the commit this block was last reconciled against —
     NOT the commit that last changed the set. Now that the gate runs, it is
     provenance rather than the reader's assurance: diff that ref against HEAD
     over the grepped path to see what a human last looked at.
     Last extracted: 3bc16cd0 -->

```
srv.start                        srv.req.start
srv.start.failed                 srv.req.end
srv.start.refused                srv.store.failed
srv.start.single_tenant          srv.auth.device_sig_rejected
srv.start.metrics_withheld       srv.auth.step_up_required
srv.start.device_sig_optional
srv.stop                         srv.relay.fanout
srv.stop.draining                srv.relay.append_failed
srv.stop.failed                  srv.relay.replay_failed
srv.health.unready               srv.ratelimit.rejected
srv.store.encrypted
srv.store.migrated
srv.store.quick_check
srv.store.wal_unavailable
srv.sync.negotiate_refused       srv.relay.cursor_gap
srv.sync.session_open            srv.relay.batch_duplicate
srv.sync.subscribe               srv.sync.refresh_rejected
srv.sync.stream_open             srv.sync.refresh_identity_mismatch
srv.sync.stream_closed           srv.sync.refreshed
srv.sync.stream_drained          srv.sync.resume_conflict
srv.sync.token_expired
srv.sync.device_revoked
srv.sync.session_store_failed
srv.push.disabled                srv.push.lookup_failed
srv.push.token_unregistered      srv.push.delivery_failed
srv.account.delete_initiated     srv.blob.tombstoned
srv.account.delete_requested     srv.blob.gc_deleted
srv.account.delete_completed     srv.blob.upload_swept
srv.maintenance.failed
```

Four names earlier revisions of this file listed are **not emitted by anything**
and must not be quoted: `srv.auth.ok`, `srv.auth.rejected`, and the whole
`srv.ws.*` family — the last of these went with the socket under
[ADR-0023](../11-adr/0023-sse-sync-transport.md), and its refresh events came
back as `srv.sync.*`. A successful authentication produces no event at all; only
a rejection does (`srv.auth.device_sig_rejected`).

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

[`metrics.md`](./metrics.md) is the catalogue: every metric's type, labels and
their value sets, unit, bucket set, and whether it is **current** or
**target**, together with the label allowlist and the gate that enforces it.
This section holds no second copy of any of that. `metrics.rs` is a lock-free
registry of labelled counters, gauges and histograms, rendered as Prometheus
text at `/metrics`. What stays here is the list of names the tree defines,
extracted from the source and checked by a gate, so the catalogue's
**current** column has something mechanical to agree with:

<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:
     grep -rhoE '"sunrise_[a-z0-9_]+"' crates/sunrise-server/src | sort -u
     Enforced the same way as the `ev` block above, by
     `.github/scripts/observability-catalog-gate.py`, which runs the command on
     this line and diffs it against the block in both directions.
     `Last extracted` names the commit this block was last reconciled against —
     NOT the commit that last changed the set. Now that the gate runs, it is
     provenance rather than the reader's assurance: diff that ref against HEAD
     over the grepped path to see what a human last looked at.
     Last extracted: 38a02c42 -->

```
sunrise_account_create_total
sunrise_account_delete_total
sunrise_blob_bytes_total                  {direction}
sunrise_blob_chunk_total
sunrise_blob_fetch_total
sunrise_blob_finalize_total
sunrise_blob_gc_deleted_total
sunrise_blob_hash_mismatch_total
sunrise_blob_init_total
sunrise_build_info                        {version, commit}
sunrise_db_size_bytes
sunrise_device_sig_rejected_total         {reason}
sunrise_devices_register_total
sunrise_devices_revoke_total
sunrise_http_request_duration_seconds     {endpoint, method}
sunrise_http_requests_total               {endpoint, method, status}
sunrise_metrics_series_dropped_total
sunrise_pairing_total                     {result}
sunrise_push_dispatch_duration_seconds    {provider}
sunrise_push_dispatch_total               {provider, result}
sunrise_push_register_total
sunrise_ratelimit_rejected_total          {endpoint, scope}
sunrise_recovery_blob_fetch_total
sunrise_recovery_step_up_refused_total
sunrise_relay_append_failed_total
sunrise_relay_batch_duplicate_total
sunrise_relay_batch_overlap_total
sunrise_relay_cursor_gap_total
sunrise_start_time_seconds
sunrise_sync_batch_ops
sunrise_sync_fanout_latency_seconds
sunrise_sync_negotiate_refused_total      {reason}
sunrise_sync_ops_received_total
sunrise_sync_refresh_total
sunrise_sync_resume_conflict_total
sunrise_sync_session_total
sunrise_sync_sessions_active
sunrise_sync_stream_total
```

38 metric names, and four that earlier revisions of this file listed and the
tree does not define: `sunrise_sync_token_expired_total`,
`sunrise_sync_token_refresh_rejected_total`, `sunrise_sync_token_refreshed_total`,
`sunrise_sync_unauthenticated_total`. The token-lifecycle counters collapsed into
`sunrise_sync_refresh_total` when sync moved to SSE
([ADR-0023](../11-adr/0023-sse-sync-transport.md)); the token *events* survive
under `srv.sync.*` above, which is why the names look familiar.

`sunrise_relay_batch_duplicate_total` is the counter for op-batch
de-duplication, and it and its near-miss counterpart below get commentary the
others do not, because the key they are both about is not the obvious
one. `POST /api/v1/sync/ops` keys on the batch's
**content**: `ops_h`, a domain-separated BLAKE3 hash over the ops, scoped to the channel —
`PRIMARY KEY (account_h, stream_id, ops_h)` in `relay_batches`. When that
content is already stored for the channel, the handler increments this counter,
logs `srv.relay.batch_duplicate`, publishes nothing to live subscribers, and
re-acks with the batch's *original* `server_first_seen_ms` rather than a fresh
one.

The client's `batch_id` is recorded beside the hash and echoed in the ack, but
it is **deliberately not part of the key**. The client's counter restarts at 1
on every reconnect, so keying on it would drop a later session's batch 1 as a
duplicate *while acking it* — and an acked batch leaves the client's outbox.
The schema comment on `relay_batches` names that outcome as silent data loss.
Two consequences for whoever is reading a spike: do not expect it to correlate
with client batch ids, because two different `batch_id`s carrying identical ops
both land here; and do not "fix" the client to persist its counter across
reconnects, because that is the change the key exists to prevent.

So the counter measures re-submitted content, which is the graphable form of
reconnect churn — a client that loses an ack re-drains its outbox and the relay
absorbs the replay. Rising against a steady op rate is a transport problem
rather than a load one. The
[`srv.relay.batch_duplicate`](../10-cross-cutting/log-events.md) event carries
the same fact per stream, for grepping; this is the fleet-level rate. One
caution before trusting it as a *total*: it counts only what reaches
`Appended::Duplicate`, and three classes of re-sent-but-already-held ops take
the `Fresh` arm instead and are never counted.

- **A re-partitioned re-send.** The key covers the whole batch —
  `batch_ops_hash` mixes in `ops.len()` and each op's length — so `[O1]` and
  `[O1, O2]` hash differently and both store. A client that authored something
  between the lost ack and the reconnect re-sends a *wider* batch, which is the
  active-client case and the dominant one in a real reconnect storm. Intended
  behaviour, asserted by `a_batch_with_different_ops_is_never_deduped`, and
  tracked as a design question on
  [#73](https://github.com/justin13888/Sunrise/issues/73), whose own words for
  it are "relay-log growth on exactly the churn the dedup was added to stop".
- **A re-send after retention evicted the frame.** `relay_batches.frame_id` is
  `ON DELETE CASCADE` against `relay_frames` with `PRAGMA foreign_keys = ON`,
  and `evict()` runs inside every append, so the dedup window *is* the retention
  window: past it, the batch is forgotten and stores again. Deliberate, and
  asserted by `eviction_forgets_the_batch_it_deduped_on` — a batch remembered
  past the frame it named would refuse ops the relay no longer holds while
  acking them.
- **An empty batch.** `batch_ops_hash` returns `None` for one and `relay_append`
  skips the lookup entirely, so it is never de-duplicated
  (`an_empty_batch_is_never_deduped`).

Read the counter as a floor on re-submission, then — reliable as a *signal*
that clients are replaying, and not a census of it. In particular a reconnect
storm among **active** clients can leave it flat while the relay log grows and
subscribers re-receive ops, because every one of those re-sends is
re-partitioned. Do not read flat as healthy on its own.

`sunrise_relay_batch_overlap_total` is the first of those three classes made
visible: it counts an append that was **stored** — fresh by the whole-batch
content key — while carrying at least one op the channel already held. That is
the re-partitioned re-send, and it is the number
[ADR-0033](../11-adr/0033-relay-batch-dedup-is-whole-batch.md) named as the
direct measurement its revisit trigger needs and did not have. The two counters
are complements and are read as a pair: `duplicate` is the churn the key caught,
`overlap` is the churn it accepted, and a fleet whose overlap rate approaches
its append rate is one where a whole-batch key is buying very little.

It is derived from sequence numbers rather than from per-op identity, because
per-op identity is exactly what that ADR declined to store. A device's ops reach
a channel in sequence order, so the channel's highest sequence for a device
bounds what it has already been sent, and an op at or below that bound has been
here before — read from `relay_frame_heads` and from `relay_evicted` together,
so a re-send of an op retention has already deleted still counts. The error is
one-directional: a batch mixing re-sent ops with new ones is counted, and new
work is never counted as old. It is a counter and not an event on purpose — a
per-occurrence record on the path whose whole question is *how often this
happens* would be log volume proportional to the thing being measured.

`/metrics` is mounted at the router root, and **only when the listener binds
loopback** — a non-loopback bind withholds the route and logs
`srv.start.metrics_withheld`. See [`overview.md`](./overview.md)
§operational-requirements and [`self-hosting.md`](./self-hosting.md)
§operator-surfaces. It was previously mounted unconditionally with no
authentication and no bind check.

### Label allowlist

The allowlist, each label's closed value set, and the gate that enforces them
are in [`metrics.md`](./metrics.md) §Label allowlist. In short: the registry
refuses a label name off the list at the call, and
`crates/sunrise-server/tests/metric-label-safety.rs` drives every route in the
published description and fails on a label name off the list, a value outside
its set, an id-shaped value, or a series count that grows with distinct ids. No
label ever carries an account, device, stream, op, entity, blob or upload id,
an email or its hash, an IP address, or a raw path.

Note the one spelling difference from the logs above: a metric's `endpoint` is
the description's own template, `/api/v1/devices/{device_id}`, where
`srv.req.*` records `/api/v1/devices/:id`. Both are templates and neither is
the request's path.

## Tracing

OpenTelemetry traces, exported over OTLP/HTTP, **off unless `[observability]`
is written** in `sunrise.toml` ([`self-hosting.md`](./self-hosting.md) documents
the table). Without it there is no exporter, no batch thread and no span: every
span call returns at its first branch, and a request's log records are exactly
what they were before tracing existed. `crates/sunrise-telemetry` holds the
mechanism and `crates/sunrise-server` the call sites.

### What is traced

One trace per HTTP operation, rooted at a server span the outermost interceptor
(`api::observe::TraceRequest`) opens. It runs the rest of the request — every
other interceptor, the extractors, the handler — under that span, so the spans
below find their parent without being handed it.

| Span | Where | Attributes and events |
|---|---|---|
| `"{METHOD} {endpoint}"`, e.g. `POST /api/v1/sync/ops` | the root, per operation | `method`, `endpoint`, `status`; status `error` on a 5xx |
| `auth.verify_token` | `api::auth::resolve_bearer`; the account lookup beneath it | status `error` on a refused bearer |
| `auth.verify_signature` | `api::signed::verify_bytes`; the device lookup beneath it | status `error` on a refused signature |
| `store.<operation>`, e.g. `store.relay_append` | every store operation the request paths reach (`store/tx.rs`) | none: the operation's name only |
| `relay.append` | the durable append in `POST /sync/ops`, its `store.relay_append` beneath it | `n_ops`, `n_bytes`, `result` (`fresh` / `duplicate`) |
| `relay.fanout` | `RelayHub::publish` | `n_streams`: the live subscribers reached, and nothing else |
| `blob.chunk_write`, `blob.chunk_read` | chunk upload, finalize's read-back and commit, and the fetch body | `n_chunks`, `n_bytes` |
| `sync.stream` | one per `GET /sync/events`, alive as long as the stream | `resumed`, `n_streams`; events `gap`, `caught_up`, `frame`, `closed` (with `reason`) |
| `push.dispatch` → `push.attempt` | each push delivery, the root of its own trace | `provider`; per attempt `attempt`, `result` |

The SSE stream is one long-lived span with events, not a span per frame: a stream
lives for hours, and what a trace of one should show is its shape. A push
delivery is a root rather than a child because the dispatcher coalesces wakes
from many requests and sends on its own schedule, so no one request is its
parent.

### Redaction by construction

A span is a log surface in the sense of
[`../10-cross-cutting/logging.md`](../10-cross-cutting/logging.md) §6, and it
leaves the process for a collector the operator runs. Nothing redacts a span
after the fact; what a span can hold is fixed by the types that build it:

- an attribute is a `sunrise_telemetry::Attr`, which has no constructor that
  takes a key. Its twelve keys (`attr::KEYS`) are names `sunrise_log`'s
  allowlist already admits, with the meaning they have in a log record, and a
  unit test holds every one to that allowlist. Values are numbers, booleans and
  `&'static str` literals chosen at the call site; the one runtime string is
  `endpoint`, the matched route's template. There is no constructor for an id,
  hashed or not: a trace already groups one request's work.
- a span name, an event name and a status description are `&'static str`,
  except the root's name, which is the method and the route template.
- the request is read for its W3C `traceparent` and nothing else. Its URI is
  never consulted, the rule the request log below keeps, and no `tracestate`
  or baggage is carried.

No bearer, session id, signature, device or account id, email, push token or raw
path can therefore become span data. `crates/sunrise-server/tests/span-redaction.rs`
is the gate: it drives every operation in the published description with a
sentinel bearer, `?access_token=`, session id (a real one too), device id,
signature and email, and searches the whole `Debug` rendering of every exported
span — name, attributes, events, status, links, trace state — for each. It also
holds every attribute key to the allowlist and every span name to the closed
set above.

### Sampling

`sunrise_telemetry::CappedSampler`, at `[observability] sample_ratio` — 1% by
default, which is the production figure; a staging relay sets `1.0`. It is
parent-based so a client can join its trace to the relay's, but a
`traceparent` can lower the relay's sampling and never raise it:

| Parent | Decision |
|---|---|
| none | the ratio, over the trace id the relay minted |
| local (a span of this process) | the parent's decision, so a trace is whole or absent |
| remote, `sampled=0` | dropped |
| remote, `sampled=1` | the ratio, over a draw the relay makes |

The last row does not read the client's trace id, because the stock ratio
sampler compares that id's low bits against a threshold and a client could pick
ids under it. `span-redaction.rs` asserts the ratio holds both for the relay's
own roots and for a client sending `sampled=1` with such an id, and that
`sampled=0` is obeyed at a ratio of 1.

### The export

OTLP over HTTP with protobuf bodies, to `[observability] endpoint` verbatim,
from the SDK's batch processor on a thread of its own. The HTTP client is the
`hyper` + `rustls` stack the server already links for its JWKS fetch and APNs,
so tracing adds no second TLS stack and no second `reqwest`; an export that has
not finished in 10 s is abandoned. The resource carries `service.name`,
`service.version`, `vcs.ref.head.revision` (the build's `SUNRISE_BUILD_COMMIT`,
as `sunrise_build_info` reports it) and, when configured,
`deployment.environment.name` — nothing the SDK would detect from the host or
the environment. The queue is flushed after the drain on shutdown. An export
failure is reported through the OpenTelemetry SDK's own log records.

### Correlation

While a sampled request runs, its handler also runs inside an `http.request` log
span carrying `method`, `endpoint`, `trace_id` and `span_id`, so a record written
during the request carries the ids of its root span under `span`
(`a_log_record_inside_a_traced_request_carries_its_trace_and_span_ids`). Both
ids are on `sunrise_log`'s field allowlist, carved out of its `_id` rule by name
because they name a trace and not an entity
([`../10-cross-cutting/logging.md`](../10-cross-cutting/logging.md) §4, §6). A
request that is not sampled, or a relay without `[observability]`, writes
exactly the records it did before.

### The request log

What predates tracing is a request log that never consults the URI, which is a better
placement than redacting one. `api::observe::RequestLog` implements
`kynos::middleware::Observer<ServerState>` and is mounted with
`.observe(observe::RequestLog)` in `api/mod.rs`. kynos hands the observer the
**matched** `kynos::router::operation::Route` — the `paths` key the request
resolved to, with its `{}` expressions intact — and the span records the HTTP
method plus that template, rewritten to the log's `:id` spelling by
`api::observe`'s private `templated` helper. Nothing else from the request
reaches a field.

Be exact about how much of that is structural, because this is the paragraph the
E2EE log promise rests on. `on_response` genuinely cannot leak a URI: it is
handed a response, a route and an elapsed duration, and no request at all. But
`on_request` is handed `&kynos::http::Request`, which is an `http::Request<Body>`
and therefore one `.uri()` call away from the query string —
`RequestLog::on_request` already reads `request.method()` off that same value.
The URI is never consulted, and what holds it that way is the *tests*, not the
type signature: `the_query_string_never_reaches_the_log` (`api/observe.rs`) and
`a_bearer_in_the_query_string_and_in_the_header_both_stay_out_of_the_log`
(`crates/sunrise-server/tests/logging.rs`) each drive a real `?access_token=` through and assert the
sentinel never appears. Neither is redundant, and neither may be dropped on the
grounds that the leak is impossible by construction. A stock request log records
the URI verbatim, which is where a bearer would sit if the `?access_token=`
fallback existed — and a bearer reached this server's log that way once already
(`api/observe.rs:3-7`).

`sunrise_log::templatize_path` is the older mitigation — it strips the query and
templates opaque segments out of a *raw* target — and it is still exported from
`sunrise-log`, but nothing on this path calls it.

> **`crates/sunrise-server/tests/logging.rs` exists again.** It did not survive
> [ADR-0021](../11-adr/0021-kynos-openapi-server.md)'s port, despite
> [`../10-cross-cutting/logging.md`](../10-cross-cutting/logging.md) §6.3 and §11
> declaring it a MUST; it was restored afterwards, so the guarantee now stands
> at two levels. The integration file drives the *public* surface —
> `ServerState::new` and `sunrise_server::build_service`, the assembly an
> operator's deployment has — through
> `a_bearer_in_the_query_string_and_in_the_header_both_stay_out_of_the_log`,
> `a_signed_request_that_is_refused_logs_no_signature_bytes`,
> `the_request_records_carry_the_catalogued_shape`,
> `healthy_traffic_is_silent_at_info`, and
> `the_refusal_records_survive_redaction_and_carry_their_cause`. `api/observe.rs`
> keeps its in-module suite over the same observer, reached through the
> crate-private `api::testing::Client` —
> `the_query_string_never_reaches_the_log`,
> `an_opaque_path_segment_is_templated`,
> `request_records_carry_status_and_latency`,
> `every_server_field_survives_the_redaction_allowlist`.
> `crates/sunrise-server/tests/` holds `logging.rs` and `oidc_verifier.rs`.
>
> Those nine tests are the whole of the enforcement. `api/observe.rs`'s module
> comment used to read as though a tenth, structural guarantee stood behind them
> — "the concrete URI is never consulted, so there is no query string to leak:
> the property is structural" — and it now carries the distinction instead: the
> structural half holds only for `on_response`, `on_disconnect` and `on_panic`,
> which are handed no request; `on_request` is handed one, and nothing but the
> tests keeps its `.uri()` unread.
>
> [`../10-cross-cutting/logging.md`](../10-cross-cutting/logging.md) §6.3 and §11
> now record this file as present and list the five tests it holds, and its §6
> allowlist table names `api/observe.rs`'s route template as the source of
> `endpoint`. All three previously said the opposite — the file "no longer
> exists", the template produced by `sunrise_log::templatize_path` — and the
> second of those understated the guarantee as well as misplacing it, because
> `templatize_path` sanitises a raw path whereas `observe::templated` is never
> given one. Corrected under
> [#99](https://github.com/justin13888/Sunrise/issues/99) and
> [#109](https://github.com/justin13888/Sunrise/issues/109).

`docs/10-cross-cutting/logging.md` §6.3 additionally bans `Plain::expose` here.
The `log-redaction` job in `.github/workflows/ci.yml` greps
`\bplain[a-z_]*\.expose[[:space:]]*\(` over `crates/sunrise-server/src` — so
both `api/observe.rs` and `logging/` are covered — along with
`crates/sunrise-telemetry/src`, every other crate that emits log records, and
any `crates/*/src/logging` module.

## Health

- `GET /api/v1/health` returns 200 + `{"status":"ok"}`, unconditionally: the
  handler reads no state, so it is liveness only. It stays 200 while the server
  drains, because a draining process is alive.
- `GET /api/v1/health?deep=1` is the readiness probe. It answers 200 only when
  the server is not draining, the store answers `SELECT 1` within 2 s, and the
  blob root takes a probe write within 5 s; otherwise 503 with `failed` naming
  the checks. A failed dependency check logs `srv.health.unready`; a drain does
  not, since it is expected. The disk-free-ratio check `api.md` lists is not
  built. See [`api.md`](./api.md) §Health / meta for the body.
- On `SIGTERM` or `SIGINT` the server logs `srv.stop.draining`, flips readiness
  to 503, ends every open SSE stream with a retryable `closed` event, and gives
  in-flight requests `[server] shutdown_grace_secs` (default 25) to finish
  before logging `srv.stop`. A second signal stops it at once.
- `sunrise-server healthcheck` probes the configured listener's liveness route
  from inside the container, so the image's `HEALTHCHECK` needs no `curl`; see
  [`self-hosting.md`](./self-hosting.md).

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

There is no audit table in `store/`'s schema, no writer, no "Security" page,
and no retention job. `[observability]` is rejected outright by the config
parser (see [`self-hosting.md`](./self-hosting.md) §"Not yet wired"), so
`audit_retention_days` cannot be set. Two of the five actions below could not be
recorded anyway: "recovery blob fetched" and "account deleted" have no route.

For account-management actions only (not content):

- Account created.
- Device added/removed.
- Recovery blob fetched.
- Account deleted.

Visible in the user's "Security" page on the web app, derived from a per-account audit log. Retention:

- 30 days, configurable via `[observability] audit_retention_days = 30`
  (default). A cron job at 02:00 UTC deletes expired records. There is one
  deployment profile ([ADR-0027](../11-adr/0027-v1-self-host-first.md)),
  so there is no managed/self-host split to state. There is no separate
  `auth_log_retention_hours` setting — server logs use `account_h` everywhere;
  no email-tagged buffer exists.

## Privacy commitments to users

The privacy policy explicitly states what we keep and for how long. The numbers in this spec are what we commit to. Any operational change that increases retention requires a user-visible privacy policy update with a 30-day notice.

## Diagnostics from the client

A user can opt in to send a diagnostics package to support. The package is a curated bundle (no plaintext content; just sync stats, error codes, version info, op counts). The user is shown the bundle contents before sending and signs it with their device key for authenticity.
