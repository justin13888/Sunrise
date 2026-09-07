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
*templated* endpoint only, plus the event set below. Per-endpoint/status rates,
connection counts, per-account op rates and slow-query logs have no
implementation; error frequency is recoverable from the `err_code` field on
rejection lines, not from a metric.

The 25 `ev` names the server emits, complete:

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
     Last extracted: c3c54ac -->

```
srv.start                        srv.req.start
srv.start.failed                 srv.req.end
srv.start.refused                srv.store.failed
srv.start.single_tenant          srv.auth.device_sig_rejected
srv.start.metrics_withheld
srv.stop                         srv.relay.fanout
srv.stop.failed                  srv.relay.append_failed
                                 srv.relay.replay_failed
srv.sync.negotiate_refused       srv.relay.cursor_gap
srv.sync.session_open            srv.relay.batch_duplicate
srv.sync.subscribe               srv.sync.refresh_rejected
srv.sync.stream_closed           srv.sync.refresh_identity_mismatch
srv.sync.token_expired           srv.sync.refreshed
srv.sync.device_revoked
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

`metrics.rs` is an in-process `BTreeMap<String, AtomicU64>` rendered as
Prometheus text at `/metrics`. It supports **counters only** — no gauges, no
histograms — and every call site increments a bare, unlabelled name. The
complete set the server emits today:

<!-- Extracted from the tree; do not edit by hand. Re-run and reconcile:
     grep -rhoE '"sunrise_[a-z0-9_]+"' crates/sunrise-server/src | sort -u
     Enforced the same way as the `ev` block above, by
     `.github/scripts/observability-catalog-gate.py`, which runs the command on
     this line and diffs it against the block in both directions.
     `Last extracted` names the commit this block was last reconciled against —
     NOT the commit that last changed the set. Now that the gate runs, it is
     provenance rather than the reader's assurance: diff that ref against HEAD
     over the grepped path to see what a human last looked at.
     Last extracted: c3c54ac -->

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
sunrise_push_apns_total          (LoggingProvider; never reached)
sunrise_push_fcm_total           (LoggingProvider; never reached)
sunrise_push_web_total           (LoggingProvider; never reached)
sunrise_relay_append_failed_total
sunrise_relay_batch_duplicate_total
sunrise_relay_cursor_gap_total
sunrise_sync_negotiate_refused_total
sunrise_sync_refresh_total
sunrise_sync_session_total
sunrise_sync_stream_total
```

21 metric names, and four that earlier revisions of this file listed and the
tree does not define: `sunrise_sync_token_expired_total`,
`sunrise_sync_token_refresh_rejected_total`, `sunrise_sync_token_refreshed_total`,
`sunrise_sync_unauthenticated_total`. The token-lifecycle counters collapsed into
`sunrise_sync_refresh_total` when sync moved to SSE
([ADR-0023](../11-adr/0023-sse-sync-transport.md)); the token *events* survive
under `srv.sync.*` above, which is why the names look familiar.

`sunrise_relay_batch_duplicate_total` is the counter for op-batch
de-duplication, and it gets commentary the other twenty do not because its key
is not the obvious one. `POST /api/v1/sync/ops` keys on the batch's **content**:
`ops_h`, a domain-separated BLAKE3 hash over the ops, scoped to the channel —
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
sunrise_db_query_seconds{op="…"}
```

### Label allowlist — NOT ENFORCED

No metric carries a label today — `Metrics::add` takes a counter name and an
increment and nothing else, with no label argument anywhere on the type, and
`render` merely passes a `{…}` in a name through verbatim — so the allowlist
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
wire_proto   (1)
crypto_suite (1)
```

Forbidden labels include `account_id`, `stream_id`, `device_id`, `email`, `email_hash`, `ip`, `path` (raw with id), `op_id`, `entity_id`. A CI test parsing the exposition output and asserting only allowlisted label names appear is the intended gate; it is **not written**.

## Tracing

**Not implemented as specified.** There is no `sunrise-telemetry` crate, no
`span_redactor`, no OTel exporter, no sampling rate, and no
`tests/span-redaction.rs`.

What exists is a request log that never consults the URI, which is a better
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
(`tests/logging.rs`) each drive a real `?access_token=` through and assert the
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
both `api/observe.rs` and `logging/` are covered — along with every other crate
that emits log records and any `crates/*/src/logging` module.

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

Visible in the user's "Security" page on the web app, derived from a per-account audit log. Retention:

- 30 days, configurable via `[observability] audit_retention_days = 30`
  (default). A cron job at 02:00 UTC deletes expired records. There is one
  deployment profile in v1 ([ADR-0027](../11-adr/0027-v1-self-host-first.md)),
  so there is no managed/self-host split to state. There is no separate
  `auth_log_retention_hours` setting — server logs use `account_h` everywhere;
  no email-tagged buffer exists.

## Privacy commitments to users

The privacy policy explicitly states what we keep and for how long. The numbers in this spec are what we commit to. Any operational change that increases retention requires a user-visible privacy policy update with a 30-day notice.

## Diagnostics from the client

A user can opt in to send a diagnostics package to support. The package is a curated bundle (no plaintext content; just sync stats, error codes, version info, op counts). The user is shown the bundle contents before sending and signs it with their device key for authenticity.
