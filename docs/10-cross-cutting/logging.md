# Logging

`status: accepted (v1)`

This spec defines the logging contract for every package and binary in the Sunrise workspace. It is one of two operational substrates that every other spec depends on; the other is [protocol-versioning](./protocol-versioning.md).

The logging system is **layered**: each package owns its slice and forwards through a single shared sink. Logs are structured, machine-parsable, and redaction-safe by construction. There is exactly one logging API per language; ad-hoc `println!`, `eprintln!`, `console.log`, `print()`, `os_log`, `Log.i()`, etc. are forbidden in shipped code (lint-enforced).

---

## 1. Goals and non-goals

| Goal | Why |
|---|---|
| Same log shape on every platform (Rust core, Bun servers, web, iOS, Android, TUI) so a single ingest pipeline parses all of it. | One grep, one schema, one alert rule. |
| Plaintext user data MUST never reach a log sink. | E2EE app; logs are not encrypted. |
| Every log line carries enough context to reconstruct a causal chain across packages and devices without including identifiers that could deanonymize a user. | Debuggability without surveillance. |
| Logging is cheap when disabled and bounded when enabled. | Mobile battery, terminal scrollback, server cost. |
| **Non-goal**: logs are not a replacement for [telemetry](./telemetry-and-privacy.md) or [observability metrics](../06-server/observability.md). Logs answer "what happened on this device in this minute"; telemetry answers "how does the fleet behave"; metrics answer "is the server healthy". |

---

## 2. Levels

Five levels, RFC-5424-aligned semantics, mapped 1:1 to `tracing::Level`, `pino` levels, browser console, and platform loggers.

| Level | When to use | Default enabled in |
|---|---|---|
| `trace` | Per-byte parse decisions, inner-loop counters, frame-by-frame UI events. Off by default everywhere. | Dev builds with `SUNRISE_LOG=trace`. |
| `debug` | One-per-operation diagnostic detail: "applied op_id=…", "fetched chunk #3/12", "FTS query took 4 ms". | Dev/CI. Production releases ship with `debug` filtered out except for opt-in diagnostic mode. |
| `info` | One-per-significant-event: app launch, sync session opened, vault unlocked, key rotated, integration sync started/finished, billing tier changed. | All builds. |
| `warn` | Recoverable degradation: backoff after retry, push provider rate limit hit, non-canonical CBOR rejected, sync stall > 30 s. | All builds. |
| `error` | Operation failed and user-visible behavior is affected: vault decrypt failure, sync auth rejection, attachment upload aborted, crash about to be reported. | All builds. |

`fatal` does not exist as a separate level. A condition that would be `fatal` is logged at `error` immediately followed by a controlled abort path. Crash reporting is described in [telemetry-and-privacy.md](./telemetry-and-privacy.md).

**Production default**: `info` and above to disk + remote sink (where applicable); `warn` and above shown in user-facing diagnostic panel.

**Diagnostic mode**: a per-device toggle that raises the in-memory ring buffer to `trace` for 30 minutes (then auto-reverts) and offers the user an "export diagnostic bundle" action. Enabling diagnostic mode is the only way `trace` and most `debug` lines reach a remote sink.

---

## 3. Structured field schema

Every log line is a JSON object with these fields. No additional top-level fields are permitted; package-specific context lives under `ctx`.

```json
{
  "ts":      "2026-05-08T12:34:56.789Z",
  "lv":      "info",
  "ev":      "sync.session.opened",
  "pkg":     "sunrise-sync",
  "mod":     "sunrise_sync::session",
  "span":    "01HXYZ...",
  "trace":   "01HXYZ...",
  "dev":     "dev_<hash6>",
  "app":     "1.4.2+macos",
  "proto":   { "wire": 1, "doc": 1, "crypto": 1 },
  "ctx":     { "stream_h": "abc123", "epoch": 4 },
  "msg":     "session opened",
  "err":     null
}
```

Field rules:

| Field | Required | Type | Notes |
|---|---|---|---|
| `ts` | yes | RFC 3339 with millisecond precision, UTC, `Z` suffix | Single source of truth for ordering within a device. |
| `lv` | yes | `"trace"\|"debug"\|"info"\|"warn"\|"error"` | Lowercase. |
| `ev` | yes | `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$` | Hierarchical event name. The first segment is the package's short id (`sync`, `crypto`, `core`, `db`, `ui`, `srv`, `int`, …). New `ev` values require a one-line entry in [`log-events.md`](./log-events.md) so analysts can grep for meaning. |
| `pkg` | yes | string | Cargo crate name, npm package name, or platform module name. |
| `mod` | yes | string | Rust module path / TS file path / Swift file / Kotlin class. |
| `span` | yes | ULID | The current span id (see §4). |
| `trace` | yes | ULID | The root trace id (see §4). For top-level events `trace == span`. |
| `dev` | yes | `dev_<8 hex chars>` | `BLAKE3(device_id \|\| device_log_salt, 4)` lowercase hex. 32 bits is enough to distinguish devices in a single user's logs; not reversible to `device_id`. `device_log_salt` is 16 random bytes generated once per install and stored next to `device_salt`. |
| `app` | yes | `<semver>+<platform>` | e.g. `1.4.2+ios18`, `1.4.2+linux-x86_64`. |
| `proto` | yes | `{ wire, doc, crypto }` | Numeric protocol version constants from [protocol-versioning](./protocol-versioning.md). Lets you reason about cross-version logs. |
| `ctx` | yes | object | Free-form package context. Keys MUST come from §6 redaction allowlist; unknown keys are dropped at sink. Always present (empty object if no context). |
| `msg` | yes | string | Human-readable summary. ≤ 200 bytes. NEVER interpolate plaintext user data; use `ctx` keys. |
| `err` | no | object \| null | Set on `warn`/`error` with `{ code, kind, retryable, cause }` (see §5). |

Encoding: NDJSON (one JSON object per `\n`). UTF-8. No trailing whitespace. Implementations MUST reject (drop + bump a counter) any record that fails JSON-schema validation against `schemas/log-record.v1.json`.

---

## 4. Trace and span propagation

Every operation has a `trace` id. Every nested step within an operation has a `span` id. Both are ULIDs (26-char Crockford base32).

Generation:
- A new `trace` is started by the entry point that initiates user-visible work: a UI event, a wire-protocol message arrival, a scheduled task firing, an integration sync run.
- Within a trace, a new `span` is created on every async boundary that cannot reasonably be inferred from `mod` + `ev` alone (sync session, op-batch apply loop, blob upload, KDF run, FTS query).
- When work crosses a wire-protocol boundary (client → server, device → device via relay), the originating `trace` id is carried in the message header (`x-sunrise-trace` for HTTP, frame field `trace` for WebSocket). The receiver creates a new `span` under that `trace`. This is the **only** identifier that crosses the device boundary in logs; `dev` is per-device and is not propagated.

Storage: each runtime keeps the active `(trace, span)` in task-local state — `tracing::Span` (Rust), `AsyncLocalStorage` (Bun), `Zone` (Dart not used; web uses a custom dispatcher). Implementations MUST surface "current span" from any synchronous code path so logging always has the right ids without manual passing.

---

## 5. Errors

`err` is set on `warn` and `error` records.

```json
"err": {
  "code":      "SYNC_AUTH_REJECTED",
  "kind":      "transient" | "permanent" | "user" | "internal",
  "retryable": true,
  "cause":     "string, ≤ 500 bytes, MUST NOT contain plaintext user data"
}
```

`code` MUST be a value from the `ErrorCode` enum in [error-handling.md](./error-handling.md). `cause` is the bottom-most `source()` chain message after redaction; if redaction is uncertain, `cause` is omitted.

---

## 6. Redaction — what may NEVER appear in any log field

The following classes of data MUST NOT appear in `msg`, `ctx`, `err.cause`, span names, or any other log surface, at any level, ever:

- **Plaintext content** of any user-authored entity: task title/body, note body, attachment file name, integration external title, person name, email address (except as described in §6.1), Stream name, Context name, tag value, recurrence rule description, search query, dictation transcript.
- **Cryptographic key material**: identity private keys, device private keys, wrap keys, stream keys, recovery passphrases, OAuth refresh tokens, push tokens, session secrets, signature private halves.
- **Authentication tokens in plaintext**: bearer tokens, OAuth access tokens, password fragments, OTP codes.
- **Stable identifiers** that map to a person across logs: full `device_id`, full `account_id`, full `idn_…` identity id, full `tsk_…`/`stm_…`/etc. entity ids, IP address (server logs may store `/24` IPv4 or `/48` IPv6 prefixes only, see §6.2).

What IS allowed in `ctx`:

| Key | Form | Origin |
|---|---|---|
| `stream_h` | `BLAKE3(stream_id \|\| device_log_salt, 4)` lowercase hex | per-device hash; not correlatable across devices |
| `task_h`, `block_h`, `routine_h`, etc. | same construction | |
| `epoch` | small integer | from key-rotation epoch |
| `seq` | integer | op sequence number |
| `op_kind` | enum string (e.g. `"task.update"`) | structural, not content |
| `n_ops`, `n_chunks`, `n_bytes` | counters | |
| `lat_ms` | latency | |
| `tier` | enum (`free`/`pro`/...) | server-side, no user link |
| `provider` | enum (`apns`/`fcm`/`web`/`google_calendar`/...) | |
| `result` | enum (`ok`/`failed`/`skipped`) | |

The `Plain<T>` wrapper (Rust) and equivalent `Plain<T>` types in TS/Swift/Kotlin tag values that originated from plaintext domain data. The logging API rejects `Plain<T>` arguments at the type level — they cannot be formatted into log fields without first calling `.expose()`, which is itself banned in `telemetry/`, `logging/`, and `observability/` modules.

### 6.1 Email addresses

Email is **never** logged anywhere. When a diagnostic needs to identify a user across logs, use `account_h = BLAKE3(account_id \|\| log_salt, 4)` rendered as 8 lowercase hex characters. `log_salt` is `device_log_salt` on clients and `server_log_salt` on the server (different salts means client and server hashes for the same account do not correlate without a join — that join is the operator's privileged action).

### 6.2 IP addresses

- Server access logs: store IPv4 truncated to `/24` and IPv6 truncated to `/48`. Full IP MAY be retained in the rate-limiter's in-memory state for at most 60 seconds.
- Client logs: never log the device's own IP. Connection diagnostics use the relay hostname instead.

### 6.3 CI enforcement

The CI build runs three checks; any failure blocks merge:

1. `ripgrep -nP '\bplain[a-z_]*\.expose\(' src/{telemetry,logging,observability}` — no matches allowed.
2. `cargo clippy -- -D sunrise::log_plaintext` — custom lint that flags any call into `tracing::*!` whose argument types include `Plain<_>`.
3. `pnpm -r run lint:logging` — typescript rule rejecting `console.*` calls outside `*.dev.ts` and rejecting log calls whose argument set includes any value of type `Plain<*>`.

A snapshot test (`tests/log-redaction.rs`) generates 10 000 random log calls across all packages with synthetic `Plain<T>` payloads and asserts that the resulting NDJSON contains none of the synthetic payload bytes.

---

## 7. Per-package responsibilities

Each package emits a fixed catalog of `ev` names. New events require a doc entry. The catalog is the contract.

### `sunrise-core` (Rust)
- `core.open.start`, `core.open.ok`, `core.open.failed` — vault open lifecycle.
- `core.unlock.attempt`, `core.unlock.ok`, `core.unlock.failed` — passphrase / OS keystore unlock. Failures count attempts, never log the passphrase.
- `core.submit.queued`, `core.submit.applied`, `core.submit.rejected` — operation submission.
- `core.query.slow` — emitted when any read query exceeds [performance budgets](./performance-budgets.md) p99.
- `core.shutdown.start`, `core.shutdown.ok`.

### `sunrise-crypto`
- `crypto.kdf.start`, `crypto.kdf.ok` — Argon2id / HKDF runs (with `lat_ms`, never the secret).
- `crypto.envelope.encrypt`, `crypto.envelope.decrypt`, `crypto.envelope.reject` — `reject` includes `err.code` (e.g. `CRYPTO_AAD_MISMATCH`, `CRYPTO_NON_CANONICAL_CBOR`).
- `crypto.rotate.start`, `crypto.rotate.complete` — stream/device/identity key rotation.
- `crypto.sig.verify.failed`.

### `sunrise-storage`
- `db.migrate.start`, `db.migrate.ok`, `db.migrate.failed` — emits `from_v`/`to_v`.
- `db.tx.commit`, `db.tx.rollback` — at debug level; `lat_ms`.
- `db.query.slow` — over budget.
- `blob.upload.start/ok/failed`, `blob.fetch.start/ok/failed`.
- `compact.start/ok/failed`.

### `sunrise-sync`
- `sync.session.opening`, `sync.session.opened`, `sync.session.closed`, `sync.session.error`.
- `sync.frame.recv`, `sync.frame.send` — debug only; includes `kind`, `n_bytes`.
- `sync.batch.applied`, `sync.batch.rejected`.
- `sync.snapshot.req`, `sync.snapshot.applied`.
- `sync.transport.fallback` — on switch from T-1 to T-2 to T-3.
- `sync.backoff` — when entering exponential backoff.

### `sunrise-server` (Bun)
- `srv.req.start`, `srv.req.end` — HTTP request lifecycle; `lat_ms`, `status`, `endpoint`.
- `srv.ws.connect`, `srv.ws.disconnect`.
- `srv.auth.ok`, `srv.auth.rejected` — `rejected` includes the OIDC error code (no token bytes).
- `srv.quota.warning`, `srv.quota.exceeded`.
- `srv.relay.fanout` — debug; `n_recipients`.
- `srv.push.send.ok`, `srv.push.send.failed` — `provider`, `n_devices`.

### Client packages (`sunrise-ui-shared`, platform apps)
- `ui.view.open`, `ui.view.close` — view name (no entity content).
- `ui.action` — explicit user actions: `ctx.action_kind`.
- `ui.error.shown` — when a user-facing error toast is displayed.
- `ui.input.lat` — debug; keystroke-to-paint p95 sampler.

### `sunrise-integrations`
- `int.run.start`, `int.run.ok`, `int.run.failed` — `provider`, `n_imported`, `n_exported`.
- `int.auth.refresh`, `int.auth.expired`.
- `int.rate_limited` — when an external API returns 429.

---

## 8. Sinks

Each runtime supports up to four sinks; the binary chooses which to enable.

| Sink | Format | Where it goes |
|---|---|---|
| `stderr` | NDJSON | Always available. Default for servers and dev runs. CLIs use the colorized "pretty" formatter only when stderr is a TTY AND `SUNRISE_LOG_FORMAT=pretty`. |
| `file` | NDJSON, gzipped on rotation | `~/.local/state/sunrise/log/<binary>.ndjson` (Linux), platform-equivalent path on macOS/Windows/iOS/Android. Rotated at 16 MiB or 24 h, whichever first. Retention: 14 days or 8 rotated files, whichever smaller. |
| `ring` | NDJSON in memory | 4 MiB circular buffer. Always on. Source for diagnostic-bundle export. |
| `remote` | NDJSON over HTTPS POST `/api/v1/diagnostics/upload` | Disabled by default. Enabled only during diagnostic mode and only for log records explicitly tagged `share: true` (the bundle exporter does this on the user's behalf). |

A log record reaches a sink only if `record.lv >= sink.min_level`. The `ring` sink always accepts `trace`+. The `remote` sink rejects records lacking `share: true`.

---

## 9. Throttling and rate limits

Every package's logger MUST apply a token-bucket rate limit per `(ev, lv)` pair: 100 tokens, refill 10/sec. When the bucket is empty, records are dropped and a single `log.throttled` record is emitted at most once per minute carrying `n_dropped`.

This protects against a runaway loop spamming the file sink and turning a minor bug into an out-of-disk incident.

---

## 10. Bootstrap

Every binary calls `sunrise_log::init(LogConfig)` exactly once at startup, before any other workspace code runs. The config is read from environment variables:

| Variable | Default | Notes |
|---|---|---|
| `SUNRISE_LOG` | `info` | Per-target filter directives, `tracing-subscriber` syntax: `info,sunrise_sync=debug`. |
| `SUNRISE_LOG_FORMAT` | `ndjson` | `ndjson` \| `pretty`. `pretty` is dev-only and refused in release builds. |
| `SUNRISE_LOG_FILE` | platform default | Override file sink path. |
| `SUNRISE_LOG_RING_BYTES` | `4194304` | Ring buffer size. |
| `SUNRISE_LOG_DIAGNOSTIC` | `0` | `1` enables diagnostic mode for 30 min from process start. |

Until `init` succeeds, log calls go to a fallback stderr formatter that does not enforce redaction (because the type-level ban is sufficient). After `init`, any uninstalled subscriber path is a panic in dev and a `log.bootstrap.late` `error` in release.

---

## 11. Tests required

Every package MUST include:

1. A snapshot test of its `ev` catalog vs. the package contract above. Adding/removing/renaming an `ev` value fails the test until the contract is updated.
2. A redaction property test: 1 000 random invocations of every public API surface with `Plain<T>` synthetic data; assert no synthetic byte appears in any sink output.
3. A throttling test: emit 10 000 of the same `ev` in 1 second; assert sink saw ≤ 110 records and exactly one `log.throttled`.

The workspace root has a single `cargo test -p log-conformance && pnpm test:log-conformance` target that aggregates these.
