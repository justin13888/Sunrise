# ADR 0011 — Layered structured logging

**Status:** accepted

## Context

Sunrise is end-to-end encrypted, runs on six platforms (Linux/macOS/Windows desktops, iOS, Android, web, plus a TUI and a Bun server), and ships dual-distribution (managed cloud + self-host). Logs are the primary debug substrate when something goes wrong on a user's device — and they are the primary risk for accidental plaintext leakage.

The acceptance set (00–11) has no explicit logging spec. `06-server/observability.md` mentions structured logs and "no PII labels"; `10-cross-cutting/telemetry-and-privacy.md` introduces a `Plain<T>` wrapper. Neither defines log levels, field schemas, sinks, retention, redaction tests, or per-package event catalogs.

A one-shot implementing agent would either pick a different logging library per package (drift), use ad-hoc string formatting (leak risk), or produce verbose logs that swamp the file sink (operational risk).

## Decision

Add `spec/10-cross-cutting/logging.md` as the canonical logging spec. It:

1. Defines five levels (`trace`/`debug`/`info`/`warn`/`error`) with platform mappings.
2. Defines an NDJSON record schema with required fields (`ts`, `lv`, `ev`, `pkg`, `mod`, `span`, `trace`, `dev`, `app`, `proto`, `ctx`, `msg`, `err`).
3. Mandates trace/span propagation rules including cross-device handoff via wire-protocol headers.
4. Specifies a redaction allowlist (what classes of data MUST NEVER appear) and three CI-enforced checks (regex, clippy lint, snapshot test).
5. Lists per-package event catalogs (`sunrise-core`, `sunrise-crypto`, `sunrise-storage`, `sunrise-sync`, `sunrise-server`, client UI, integrations).
6. Defines four sinks (`stderr`, `file`, `ring`, `remote`) with rotation, retention, and access rules.
7. Mandates token-bucket throttling per `(ev, lv)` pair to prevent runaway log floods.
8. Standardizes bootstrap via `sunrise_log::init(LogConfig)` exactly once per binary.
9. Requires three conformance tests per package (event-catalog snapshot, redaction property test, throttling test).

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
- Adds a workspace-shared `sunrise-log` crate / npm package per language; cost is one-time.
- Lint and snapshot tests add CI overhead (~2 minutes per PR).
- Log records carry a `proto` block on every line; ~50 bytes overhead per record. Acceptable.
