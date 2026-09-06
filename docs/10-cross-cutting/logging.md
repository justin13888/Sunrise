---
status: accepted
---

# Logging

This spec defines the logging contract for every package and binary in the Sunrise workspace. It is one of two operational substrates that every other spec depends on; the other is [protocol-versioning](./protocol-versioning.md).

> **Revised 2026-08.** This document originally specified a bespoke logger — its own levels, dispatch, sinks, span propagation, throttle, and record schema — implemented in `crates/sunrise-log`. That crate was never wired into anything. [ADR-0010](../11-adr/0010-logging-strategy.md#amendment-2026-08-tracing-carries-the-transport) records the decision to hand the transport to **`tracing` + `tracing-subscriber`** and keep only the redaction discipline in-house, and names exactly what was given up. The sections below describe what runs.

Logs are structured, machine-parsable, and redaction-safe by construction. The Rust logging API is `tracing`; ad-hoc `println!`, `eprintln!`, `console.log`, `print()`, `os_log`, `Log.i()`, etc. are forbidden in shipped code (the workspace clippy config denies `print_stdout` and `print_stderr`).

---

## 1. Goals and non-goals

| Goal | Why |
|---|---|
| Same log shape from every crate and binary so a single ingest pipeline parses all of it. (The original "six platforms" framing assumed a Bun server and native mobile clients; neither exists — see [ADR-0012](../11-adr/0012-web-wasm-deferred.md) and [ADR-0007](../11-adr/0007-mobile-strategy.md).) | One grep, one schema, one alert rule. |
| Plaintext user data MUST never reach a log sink. | E2EE app; logs are not encrypted. |
| Every log line carries enough context to reconstruct a causal chain across packages and devices without including identifiers that could deanonymize a user. | Debuggability without surveillance. |
| Logging is cheap when disabled and bounded when enabled. | Mobile battery, terminal scrollback, server cost. |
| **Non-goal**: logs are not a replacement for [telemetry](./telemetry-and-privacy.md) or [observability metrics](../06-server/observability.md). Logs answer "what happened on this device in this minute"; telemetry answers "how does the fleet behave"; metrics answer "is the server healthy". |

---

## 2. Levels

Five levels, RFC-5424-aligned semantics. In Rust these **are** `tracing::Level`; there is no parallel enum.

| Level | When to use | Default enabled in |
|---|---|---|
| `trace` | Per-byte parse decisions, inner-loop counters, frame-by-frame UI events. Off by default everywhere. | Dev builds with `SUNRISE_LOG=trace`. |
| `debug` | One-per-operation diagnostic detail: "applied op_id=…", "fetched chunk #3/12", "FTS query took 4 ms". | Dev/CI. Production releases ship with `debug` filtered out except for opt-in diagnostic mode. |
| `info` | One-per-significant-event: app launch, sync session opened, vault unlocked, key rotated, integration sync started/finished, billing tier changed. | All builds. |
| `warn` | Recoverable degradation: backoff after retry, push provider rate limit hit, non-canonical CBOR rejected, sync stall > 30 s. | All builds. |
| `error` | Operation failed and user-visible behavior is affected: vault decrypt failure, sync auth rejection, attachment upload aborted, crash about to be reported. | All builds. |

`fatal` does not exist as a separate level. A condition that would be `fatal` is logged at `error` immediately followed by a controlled abort path. Crash reporting is described in [telemetry-and-privacy.md](./telemetry-and-privacy.md).

**Production default**: `info` and above to disk + remote sink (where applicable); `warn` and above shown in user-facing diagnostic panel.

**Diagnostic mode** (not implemented): a per-device toggle raising verbosity to `trace` for 30 minutes and offering an "export diagnostic bundle" action. It has no code behind it and no sink to feed. Raising verbosity today means setting `SUNRISE_LOG` and restarting.

---

## 3. Structured field schema

Every log line is a JSON object produced by `tracing-subscriber`'s JSON
formatter with `flatten_event(true)`, so context fields sit at the top level
next to the envelope:

```json
{
  "timestamp": "2026-05-08T12:34:56.789Z",
  "level":     "INFO",
  "message":   "relay session opened",
  "ev":        "srv.sync.session_open",
  "target":    "sunrise_server::api::sync",
  "account_h": "a3f9c1d2",
  "wire_v":    1,
  "span":      { "name": "http.request", "method": "GET", "endpoint": "/api/v1/health" }
}
```

| Field | Required | Type | Notes |
|---|---|---|---|
| `timestamp` | yes | RFC 3339, millisecond precision, UTC, `Z` suffix | `sunrise_log::Rfc3339Millis`. Ordering within a process. |
| `level` | yes | `TRACE`\|`DEBUG`\|`INFO`\|`WARN`\|`ERROR` | `tracing::Level`, uppercase — the formatter's spelling, not ours. |
| `message` | yes | string | Human-readable summary, ≤ 200 bytes. NEVER interpolate plaintext user data. |
| `target` | yes | string | The emitting module path. Replaces the old `pkg` + `mod` pair, which were the same information split in two. |
| `ev` | yes | `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$` | Hierarchical event name; first segment is the package short id (`sync`, `db`, `ui`, `srv`, …). New values require an entry in [`log-events.md`](./log-events.md), enforced by `crates/sunrise-log/tests/event_catalog.rs`. |
| `span` | no | object | The innermost active span: its `name` plus its fields. Absent outside a span. The ancestor list is not emitted. |
| *context* | — | scalar | Any other top-level key. MUST come from the §6 allowlist; `sunrise_log::RedactionLayer` refuses the event otherwise. |
| `err_code`, `err_kind`, `retryable`, `cause` | no | see §5 | The error envelope, flattened rather than nested. |

Encoding: NDJSON (one JSON object per `\n`), UTF-8. `schemas/log-record.v1.json`
describes this shape and is validated against live subscriber output by
`crates/sunrise-log/tests/record_schema.rs`.

**Dropped from the original schema.** `pkg` and `mod` collapsed into `target`.
`dev` and `app` are not on every record — there is no `device_log_salt`
infrastructure, so a `dev` field would have been an unsalted device id or a
constant. `proto` moved from every record to the binary's startup event
(`srv.start` / `ui.start`), which costs a join and saves ~50 bytes a line to
restate three numbers that cannot change while the process lives. See
[ADR-0010](../11-adr/0010-logging-strategy.md).

---

## 4. Trace and span propagation

Spans are `tracing` spans. `#[instrument]`, `Span::in_scope`, and the automatic
`Instrument` future adapter give the causal chain, and the current span's
fields ride every record emitted inside it (the `span` object in §3).

Entry points that begin user-visible work open a span: the server opens
`http.request` per request (`sunrise_server::logging::RequestSpan`) and the
sync driver's session is one scope. Within it, nested spans are created only
where `target` + `ev` do not already say which operation is running.

**Not implemented: cross-device propagation.** The original spec carried a ULID
`trace` id across the wire in `x-sunrise-trace` / a `trace` frame field, so a
client operation and the relay's handling of it shared an id. `tracing` has no
opinion about wire formats and nothing in `sunrise-wire-protocol` populates
that field, so a server-side span today is rooted at the server. The `Hello`
frame already carries an (unused) `trace` string; wiring it is a follow-up, not
something this revision delivered.

---

## 5. Errors

The error envelope is four flat fields, set on `warn` and `error` records:

```json
"err_code":  "SYNC_AUTH_REJECTED",
"err_kind":  "transient" | "permanent" | "user" | "internal",
"retryable": true,
"cause":     "string, ≤ 500 bytes, MUST NOT contain plaintext user data"
```

Flat rather than a nested `err` object because `tracing` records scalars: a
nested object would mean serialising a struct into one field and losing the
ability to filter on `err_code` in an ingest query.

`err_code` MUST be a value from the `ErrorCode` enum in
[error-handling.md](./error-handling.md). `cause` is the bottom-most `source()`
chain message after redaction; if redaction is uncertain, `cause` is omitted.

---

## 6. Redaction — what may NEVER appear in any log field

The following classes of data MUST NOT appear in `message`, any context field, `cause`, span names, span fields, or any other log surface, at any level, ever:

- **Plaintext content** of any user-authored entity: task title/body, note body, attachment file name, integration external title, person name, email address (except as described in §6.1), Stream name, Context name, tag value, recurrence rule description, search query, dictation transcript.
- **Cryptographic key material**: identity private keys, device private keys, wrap keys, stream keys, recovery passphrases, OAuth refresh tokens, push tokens, session secrets, signature private halves.
- **Authentication tokens in plaintext**: bearer tokens, OAuth access tokens, password fragments, OTP codes.
- **Stable identifiers** that map to a person across logs: full `device_id`, full `account_id`, full `idn_…` identity id, full `tsk_…`/`stm_…`/etc. entity ids, IP address (server logs may store `/24` IPv4 or `/48` IPv6 prefixes only, see §6.2).

### The allowlist

The authoritative list is `ALLOWED` in `crates/sunrise-log/src/field.rs` — data,
not prose, so the enforcement below reads the same thing this section
describes. Adding a key there is an assertion that values under that name can
never be user-authored content. Broad shapes:

| Key shape | Form | Origin |
|---|---|---|
| `stream_h`, `task_h`, `block_h`, `routine_h`, `note_h`, `attachment_h`, `person_h`, `account_h`, `device_h` | first 4 bytes of `BLAKE3(id)`, 8 lowercase hex | Truncated hash of a **high-entropy, machine-minted** id. See §6.1 for why the spec'd salt is omitted. |
| `epoch`, `seq`, `attempt` | small integer | key-rotation epoch, op sequence, retry count |
| `op_kind`, `kind`, `result`, `mode`, `tier`, `provider`, `action_kind`, `view`, `aead_alg`, `sig_alg` | enum string | structural, not content |
| `n_ops`, `n_chunks`, `n_bytes`, `n_devices`, `n_streams`, `n_imported`, `n_exported`, `n_retained`, `n_dropped` | counters | |
| `lat_ms`, `delay_ms` | durations | |
| `status`, `method`, `endpoint` | HTTP | `endpoint` is **templated** by `sunrise_log::templatize_path`: query string dropped, opaque path segments replaced with `:id`. |
| `wire_v`, `doc_v`, `crypto_v`, `storage_v`, `from_v`, `to_v`, `app_v` | versions | |
| `bind`, `relay` | socket address / host | The server's own listen address, and the relay hostname §6.2 sanctions as the stand-in for a client IP. |
| `err_code`, `err_kind`, `retryable`, `cause` | error envelope | §5 |
| `ev`, `message` | envelope | §3 |

### How this is enforced

Two independent defences, both in `crates/sunrise-log`:

1. **`Plain<T>` closes the value path.** It implements no `Display`, no
   `serde::Serialize`, and no `tracing::Value`, and its `Debug` prints the
   fixed string `Plain<…>`. So `info!(title = plain)` and `info!(title = %plain)`
   do not compile, and `info!(title = ?plain)` records `Plain<…>`. There is no
   tracing path that renders the payload. `crates/sunrise-log/tests/redaction.rs`
   drives random payloads through every one of those paths and asserts the sink
   saw the marker and not one byte of the payload.

2. **`RedactionLayer` closes the field-name path.** A value that was
   legitimately `.expose()`d and then logged is not a `Plain<T>` any more, so
   defence 1 cannot see it. The layer checks every field name on every event
   from a `sunrise_*` target against the allowlist above and **vetoes the whole
   event** if one is unrecognised (`Layer::event_enabled` is `AND`-ed across the
   stack, so a `false` suppresses it for every layer beneath). In debug builds
   the default is to panic naming the field, so an unvetted name fails CI rather
   than shipping; in release it drops the record and bumps a counter. Losing a
   log line beats leaking a task title.

Neither defence covers `message`. A format string is arbitrary text by
construction; keeping plaintext out of it is what the `.expose()` CI grep is
for.

Spans are not gated: `on_new_span` has no veto and a span cannot be rewritten
once created. Sunrise spans are built only by this workspace's own
`#[instrument]` / `info_span!` sites with allowlisted field names, and defence 1
still applies to their values.

### 6.1 Email addresses

Email is **never** logged anywhere. When a diagnostic needs to identify a user
across logs, use `account_h`: the first 4 bytes of `BLAKE3(account_id)`, 8
lowercase hex characters (`sunrise_server::logging::account_h`).

**The salt in the original construction is omitted, deliberately.** Its job was
to stop a *low-entropy* identifier — an email address — from being recovered by
brute force. Sunrise account ids are not that: `Store::resolve_account` mints
them as 16 random bytes with no relation to the OIDC subject or the email, so
there is nothing to enumerate. What a salt would still have bought is
preventing a client-side and a server-side hash of the same account from being
joined without the operator's help; clients hash their *own* ids under a
device-local salt, so that join is already unavailable. The same reasoning
covers `stream_h` and the other entity hashes, which are also 16 random bytes.

If an id ever becomes derivable from user-supplied input, this stops holding
and the salt has to come back.

### 6.2 IP addresses

- Server access logs: **no client address is logged at all** in v1. The request
  span records method and templated endpoint and nothing else, which is
  stricter than the `/24` / `/48` truncation this section allows. Truncated
  prefixes become relevant when there is a rate limiter to explain.
- Client logs: never log the device's own IP. Connection diagnostics use the
  relay hostname (`relay`), which `sunrise_cli::livesync::relay_host` reduces
  from the configured URL — dropping any credentials and query string with it.

### 6.3 CI enforcement

The `log-redaction` job in `.github/workflows/ci.yml` greps
`\bplain[a-z_]*\.expose[[:space:]]*\(` across the `src` tree of every crate
that emits log records — today `sunrise-log`, `sunrise-server`, `sunrise-cli`,
`sunrise-storage`, `sunrise-core`, `sunrise-sync` — plus any `telemetry/`,
`logging/`, or `observability/` module in any crate. **A crate that starts
logging must be added to that list.** It runs through
`.github/scripts/grep-gate.sh`, which fails if none of the paths exist, so the
gate cannot degrade to a silent no-op the way its predecessor had (it searched
`packages/sunrise-log/src`, which never existed, and two glob patterns that
matched nothing).

Alongside it:

- `crates/sunrise-log/tests/redaction.rs` — property tests over both defences
  above, asserting on the bytes a sink received.
- `crates/sunrise-log/tests/event_catalog.rs` — every event name emitted from
  shipped source is grammatical and catalogued in
  [`log-events.md`](./log-events.md), and every **field name** those events
  carry is on the §6 allowlist, so the record the catalogue promises can reach
  a subscriber at all.
- `crates/sunrise-log/tests/record_schema.rs` — live records validate against
  `schemas/log-record.v1.json`, and every top-level key is either the fixed
  envelope or an allowlisted context key.
- `crates/sunrise-server/src/api/observe.rs` (in-module) — real requests through
  the real router: no `?access_token=`, no full entity ids, every field
  allowlisted. **This replaces `crates/sunrise-server/tests/logging.rs`, which
  §6.3 declares a MUST and which does not exist**: it did not survive
  [ADR-0021](../11-adr/0021-kynos-openapi-server.md)'s port. Restoring the
  integration-level test is open work.

**Not implemented:** the custom `sunrise::log_plaintext` clippy lint (a
type-aware lint needs a `dylint` driver, and `Plain<T>` having no `Value` impl
makes the compiler reject the same code with a plain type error), and the
TypeScript `lint:logging` rule (there is no TypeScript logging surface —
ADR-0010's Bun server does not exist).

---

## 7. Per-package responsibilities

The catalogue lives in [`log-events.md`](./log-events.md), split into what is
**implemented** and what is **reserved**. It is the contract, and it is checked:
`crates/sunrise-log/tests/event_catalog.rs` fails if any event emitted from
shipped source is missing from it. Which files count is taken from
`cargo metadata` and the compiler's own dep-info rather than from a directory
walk.

Implemented today: `sunrise-server` (startup, request lifecycle, auth outcome,
WebSocket session, relay fan-out), `sunrise-storage` (migrations),
`sunrise-core::sync_driver` + `sunrise-cli::livesync` (session lifecycle,
backoff), and `sunrise-cli` (startup, dev pairing).

Deliberately not implemented, with reasons, in the catalogue's *Reserved*
section: the `crypto` hot path, per-transaction storage events, per-keystroke UI
events, quota and push (no such features), and integrations (no provider).

---

## 8. Sinks

`tracing-subscriber` calls this the writer; the binary picks one at startup via
`sunrise_log::init`.

| Destination | Format | Who uses it |
|---|---|---|
| `stderr` | NDJSON | `sunrise-server`. `SUNRISE_LOG_FORMAT=pretty` switches to a human line — **dev builds only**; a release binary refuses the request and stays on NDJSON, because release output is something a pipeline parses. |
| file | NDJSON | `sunrise-cli`. `$XDG_STATE_HOME/sunrise/log/sunrise-cli.ndjson`, defaulting to `~/.local/state/sunrise/log/`; override with `SUNRISE_LOG_FILE`. **A command-line client cannot log to either standard stream**: stdout is its contract (`sunrise export … \| jq` has to work) and stderr is where it talks to the human running it. If the file cannot be opened the CLI runs with no logging at all — falling back to stderr would trade a missing log for corrupted output. |
| capture | NDJSON | Tests (`sunrise_log::Capture`), so assertions are on the exact bytes a sink would receive. |

**Rotation** is a size roll at 16 MiB keeping one previous generation
(`<name>.ndjson.1`), so a runaway loop costs at most 32 MiB rather than a
partition. Not gzipped, not time-based, no 14-day retention sweep — see the
throttling note in §9 for why the size cap is the part that had to exist.

**Removed: the `ring` and `remote` sinks.** `remote` was never implemented and
had no endpoint behind it; `ring` was implemented but existed only to feed a
diagnostic-bundle exporter that also does not exist. Both were deleted rather
than kept as an interface nothing satisfies. Reintroducing `remote` is a
privacy decision (opt-in only, per the original text), not a plumbing one.

---

## 9. Throttling and rate limits

**Not implemented.** The original spec mandated a token bucket per `(ev, lv)` —
100 tokens, refill 10/sec — with a once-a-minute `log.throttled` summary.
`crates/sunrise-log` implemented it and nothing ever called it.

`tracing` has no equivalent, and reintroducing one under it means a layer
holding a `HashMap` keyed by callsite behind a lock on the emit path. That is a
real cost for a hazard that has two cheaper answers already in place: level
discipline (`EnvFilter`, and the per-event level choices in the catalogue —
`srv.req.end` is `debug` precisely so healthy traffic is silent), and the
16 MiB size roll in §8, which bounds the actual failure mode the throttle
existed to prevent.

What is genuinely given up: a loop spinning at `warn` on the server will fill
stderr as fast as the pipeline reads it, and there is no `n_dropped` counter
to notice it by. If that happens, the bucket comes back as a layer.

---

## 10. Bootstrap

Every binary installs the subscriber as its **first statement**, before
anything that could want to log:

```rust
sunrise_log::init_stderr()?;                       // sunrise-server
let _ = sunrise_log::init_file("sunrise-cli");     // sunrise-cli
```

Both assemble the same stack:

```text
Registry
  └── EnvFilter        (SUNRISE_LOG)
      └── RedactionLayer  (§6 — the allowlist veto)
          └── fmt layer   (NDJSON or pretty, to the §8 destination)
```

| Variable | Default | Notes |
|---|---|---|
| `SUNRISE_LOG` | `info` | Per-target filter directives, `tracing-subscriber` syntax: `info,sunrise_sync=debug`. An unparsable value falls back to the default rather than failing startup. |
| `SUNRISE_LOG_FORMAT` | `ndjson` | `ndjson` \| `pretty`. `pretty` is refused in release builds. An unrecognised value falls back to `ndjson`. |
| `SUNRISE_LOG_FILE` | platform default | File destination for file-logging binaries. |

`SUNRISE_LOG_RING_BYTES` and `SUNRISE_LOG_DIAGNOSTIC` are gone with the sinks
and the mode they configured (§8, §2).

A log call made before `init` is dropped by `tracing`'s no-op default
subscriber — which is why `init` goes first rather than being defended against.
Calling it twice returns `LogError::AlreadyInitialized`; `tracing` allows only
one global dispatcher.

---

## 11. Tests

| Test | What it holds |
|---|---|
| `crates/sunrise-log/tests/redaction.rs` | Property tests over both §6 defences, against the real subscriber stack, asserting on captured sink bytes. |
| `crates/sunrise-log/tests/event_catalog.rs` | Every event emitted from shipped source is grammatical and catalogued, and every field name it carries is allowlisted. File set from `cargo metadata` + dep-info; the test's module doc enumerates what it does not cover. |
| `crates/sunrise-log/tests/record_schema.rs` | Live records validate against `schemas/log-record.v1.json`; every top-level key is envelope or allowlist. |
| `crates/sunrise-server/src/api/observe.rs` (in-module) | Real requests through the real router: no `?access_token=`, no full entity ids, every field allowlisted. Stands in for `tests/logging.rs`, which §6.3 requires and which no longer exists. |
| `crates/sunrise-cli/tests/logging.rs` | Records reach the file destination and parse as NDJSON; the log directory is created on first run; relay URLs are reduced to a host. |

**Removed: the per-package conformance triple.** The original §11 asked every
package for an `ev`-catalogue snapshot, a redaction property test, and a
throttling test, aggregated by a `log-conformance` target. The throttle is gone
(§9). The snapshot is replaced by the workspace-wide catalogue scan above, which
tests source against documentation rather than a constant against itself. The
redaction property test is centralised, because the guarantee it checks is a
property of `Plain<T>` and `RedactionLayer`, not of each caller.
