# 0010 — Layered structured logging

**Status:** accepted

**Amended (2026-08):** the decision to build the logging *transport* ourselves
is reversed. `tracing` + `tracing-subscriber` now carry levels, spans,
filtering, dispatch, and formatting; `crates/sunrise-log` shrank to the
redaction discipline. See
[Amendment](#amendment-2026-08-tracing-carries-the-transport)
below for what was replaced, why, and what was given up. The redaction
guarantee and the CI gate — the parts that earn their keep — survive intact and
are, for the first time, actually running.

## Context

Sunrise is end-to-end encrypted, runs on six platforms (Linux/macOS/Windows desktops, iOS, Android, web, plus a TUI and a Bun server), and ships dual-distribution (managed cloud + self-host). Logs are the primary debug substrate when something goes wrong on a user's device — and they are the primary risk for accidental plaintext leakage.

The acceptance set (00–11) has no explicit logging spec. `06-server/observability.md` mentions structured logs and "no PII labels"; `10-cross-cutting/telemetry-and-privacy.md` introduces a `Plain<T>` wrapper. Neither defines log levels, field schemas, sinks, retention, redaction tests, or per-package event catalogs.

A one-shot implementing agent would either pick a different logging library per package (drift), use ad-hoc string formatting (leak risk), or produce verbose logs that swamp the file sink (operational risk).

## Decision

Add `docs/10-cross-cutting/logging.md` as the canonical logging spec. It:

1. Defines five levels (`trace`/`debug`/`info`/`warn`/`error`) with platform mappings.
2. Defines an NDJSON record schema with required fields (`ts`, `lv`, `ev`, `pkg`, `mod`, `span`, `trace`, `dev`, `app`, `proto`, `ctx`, `msg`, `err`).
3. Mandates trace/span propagation rules including cross-device handoff via wire-protocol headers.
4. Specifies a redaction allowlist (what classes of data MUST NEVER appear) and three CI-enforced checks (regex, clippy lint, snapshot test).
5. Lists per-package event catalogs (`sunrise-core`, `sunrise-crypto`, `sunrise-storage`, `sunrise-sync`, `sunrise-server`, client UI, integrations).
6. Defines four sinks (`stderr`, `file`, `ring`, `remote`) with rotation, retention, and access rules.
7. Mandates token-bucket throttling per `(ev, lv)` pair to prevent runaway log floods.
8. Standardizes bootstrap via `sunrise_log::init(LogConfig)` exactly once per binary.
9. Requires three conformance tests per package (event-catalog snapshot, redaction property test, throttling test).

## Amendment (2026-08): tracing carries the transport

### What the original decision said

Points 1, 2, 6, 7, 8 and 9 of the Decision above committed to building a logger:

> 1. Defines five levels (`trace`/`debug`/`info`/`warn`/`error`) with platform mappings.
> 2. Defines an NDJSON record schema with required fields (`ts`, `lv`, `ev`, `pkg`, `mod`, `span`, `trace`, `dev`, `app`, `proto`, `ctx`, `msg`, `err`).
> 6. Defines four sinks (`stderr`, `file`, `ring`, `remote`) with rotation, retention, and access rules.
> 7. Mandates token-bucket throttling per `(ev, lv)` pair to prevent runaway log floods.
> 8. Standardizes bootstrap via `sunrise_log::init(LogConfig)` exactly once per binary.
> 9. Requires three conformance tests per package (event-catalog snapshot, redaction property test, throttling test).

And in Consequences:

> - Adds a workspace-shared `sunrise-log` crate / npm package per language; cost is one-time.

### What changed

Two things, and the second is the one that matters.

**The premise moved.** The Context above reasons about "six platforms
(Linux/macOS/Windows desktops, iOS, Android, web, plus a TUI and a Bun server)"
and "exactly one logging API per language". None of that is the shape of the
workspace: [ADR-0001](./0001-bun-workspace.md)'s Bun server was replaced by the
Rust `sunrise-server`, [ADR-0012](./0012-web-wasm-deferred.md) deferred web, and
[ADR-0007](./0007-mobile-strategy.md) has no native client shipping. The
`packages/sunrise-log` npm package the CI gate searched for never existed. Every
line of logging in Sunrise is Rust, which removes the whole reason a
cross-language house format was worth hand-writing.

**The implementation was never connected.** `crates/sunrise-log` was 1,716 lines
implementing levels, records, a global dispatcher, a task-local span stack, a
ULID encoder, a token bucket, and `event!` / `error_event!` macros — and
`cargo tree` showed no crate depending on it. `sunrise_log::init` was called
from nowhere. Two of the four sinks in point 6 (`file`, `remote`) did not exist
as code. Both binaries did their startup logging with `eprintln!` behind a
file-level `#![allow(clippy::print_stderr)]`. So the situation was not "a house
logger with some rough edges" but "a specification, an unreferenced crate, and
no logs" — and the `log-redaction` CI gate passed because it searched a tree
that contained no `.expose()` calls because it contained no callers.

Faced with connecting 1,716 lines of bespoke dispatch to the workspace, versus
deleting them in favour of the library that already does that work correctly and
that `tower-http` had already pulled into the lock file, the second is the
obvious call. It is only *arguably* the obvious call in the world the original
Context describes; in the actual one it is not close.

### What is now true

`tracing` supplies: levels (`tracing::Level` *is* the level enum), spans and
`#[instrument]`, callsite caching, per-target filtering via `EnvFilter`, global
dispatch, and NDJSON/pretty formatting. `crates/sunrise-log` is ~1,000 lines,
most of it tests and rationale, and supplies:

- **`Plain<T>`** — unchanged, and now with a test that proves what it claims.
  It implements no `Display`, no `serde::Serialize`, and no `tracing::Value`,
  and its `Debug` is opaque, so no `tracing` path renders the payload.
- **`RedactionLayer`** — new. A `tracing` layer that vetoes any event from a
  `sunrise_*` target carrying a field name outside the allowlist, via
  `Layer::event_enabled`. This is what catches a value that was legitimately
  `.expose()`d and then logged, which the type-level defence structurally
  cannot see.
- **the field allowlist** (`field::ALLOWED`) — logging.md §6 expressed as data,
  so the enforcement reads the same list the spec describes.
- **`EventName` + the catalogue test** — the `ev` grammar, plus a test that
  scans every `ev = "…"` literal in `crates/*/src` and fails if one is not in
  [`log-events.md`](../10-cross-cutting/log-events.md).
- **subscriber assembly and two writers** — `init`, an RFC 3339 millisecond
  timer over `jiff`, a size-capped file writer, and an in-memory capture writer
  for tests. I/O glue that `MakeWriter` exists to accept, not dispatch.

Deleted: `level.rs`, `record.rs`, `span.rs`, `throttle.rs`, `macros.rs`, and the
whole `sink/` module.

Both binaries now initialise logging as their first statement and are
instrumented — see [`log-events.md`](../10-cross-cutting/log-events.md) for
what they emit and what was judged not worth emitting.

### What we give up

Being precise about this rather than pretending the deleted code was
decorative:

- **The `(ev, lv)` token bucket.** A runaway `warn` loop on the server will fill
  stderr as fast as the pipeline drains it, with no `n_dropped` counter to
  notice it by. Mitigated but not replaced: the file destination has a 16 MiB
  size roll, which bounds the disk failure the bucket mostly existed to prevent,
  and level discipline keeps healthy traffic out of the log. Reintroducing it
  means a layer holding a lock-guarded map on the emit path, which is a real
  cost to pay only once the hazard is observed.
- **The exact §3 record schema.** `pkg` and `mod` collapse into `target`; `err`
  flattens into `err_code`/`err_kind`/`retryable`/`cause`; `lv` becomes an
  uppercase `level`. Keeping the old field names would have meant a custom
  `FormatEvent` — precisely the parallel implementation this amendment exists to
  remove. `schemas/log-record.v1.json` was rewritten to describe what is emitted
  and is now validated against live output, which it never was before.
- **`proto` on every record.** The three protocol versions now ride the
  binary's startup event instead. Correlating a later record with them costs a
  join on the process; carrying them cost ~50 bytes a line to restate three
  numbers that cannot change while the process lives.
- **`dev` and `app` on every record.** There is no `device_log_salt`
  infrastructure, so a `dev` field would have been an unsalted device id or a
  constant. Neither is worth a required field.
- **Cross-device trace propagation.** logging.md §4's `x-sunrise-trace` header
  and `trace` frame field are still unimplemented — as they were before — so a
  server-side span is rooted at the server. `tracing` makes this *easier* to add
  later (`Span::follows_from`, a propagator layer) but does not add it.
- **The `ring` and `remote` sinks.** `remote` never existed; `ring` existed but
  fed a diagnostic-bundle exporter that does not. Deleted rather than kept as an
  interface nothing satisfies.
- **The custom `sunrise::log_plaintext` clippy lint** (point 4's second CI
  check). A type-aware lint needs a `dylint` driver, and it is largely redundant:
  `Plain<T>` having no `Value` impl means the compiler already rejects
  `info!(x = plain)` with a type error.

### Status change

`accepted` → `accepted`. The decision's *goals* — one log shape, CI-enforced
redaction, a stable event catalogue, bounded output — are unchanged and are now
met by running code rather than by a document. What is reversed is the
implementation strategy for the transport, which the original ADR never argued
for on its merits; it simply assumed a house logger followed from a
cross-language house format.

---

## Alternatives considered

1. **Per-platform native loggers.** Rejected: would produce six different log shapes, making cross-package debugging painful.
2. **Free-form structured logging without an event catalog.** Rejected: alerts and dashboards depend on stable `ev` names; without a catalog they drift.
3. **No type-level redaction.** Rejected: human discipline is insufficient for an E2EE app; the `Plain<T>` ban + lint is the only defense that survives refactoring.
4. **Ship logs to a remote sink by default.** Rejected: violates the privacy promise; remote sink is opt-in via diagnostic mode only.

## Consequences

**Positive:**
- One log shape across all packages and platforms; one ingest schema.
- CI-enforced redaction; auditable.
- Per-package event catalog gives operators a contract for alerts.
- Throttling protects against bug-induced log floods.

**Negative:**
- ~~Adds a workspace-shared `sunrise-log` crate / npm package per language; cost is one-time.~~ *Amended: the cost was not one-time and was not paid — the crate was written and never connected. `sunrise-log` is now a `tracing` layer, and there is no npm package.*
- Lint and snapshot tests add CI overhead (~2 minutes per PR).
- ~~Log records carry a `proto` block on every line; ~50 bytes overhead per record. Acceptable.~~ *Amended: `proto` moved to the startup event; see [What we give up](#what-we-give-up).*
- `tracing-subscriber` adds `sharded-slab`, `thread_local`, `matchers`, and `tracing-serde` to the lock file (all MIT/Apache-2.0; `cargo deny check` clean). `tracing` and `tracing-core` were already there transitively via `tower-http`.
