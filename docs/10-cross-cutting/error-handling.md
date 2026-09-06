---
status: accepted
---

# Error Handling

Errors flow up the stack with a stable code at every boundary. UIs translate codes into user-visible copy.

## Codes

A finite enum across the system. Examples:

- `AUTH_DEVICE_REVOKED`
- `AUTH_DEVICE_SIG_INVALID`
- `AUTH_TOKEN_INVALID`
- `STORAGE_VAULT_LOCKED`
- `SYNC_PROTOCOL_VERSION_MISMATCH`
- `SYNC_TAMPER_DETECTED`
- `SYNC_NETWORK_UNAVAILABLE`
- `CRYPTO_DECRYPT_FAILED`
- `CRYPTO_RECOVERY_BLOB_INVALID`
- `INTEGRATION_REAUTH_REQUIRED`
- `VALIDATION_INVALID_TITLE`
- `VALIDATION_DUE_BEFORE_SCHEDULED`
- `INTERNAL_UNKNOWN_CODE`

Codes are stable across versions; new codes can be added but never repurposed.

### Registry

- The single source of truth is the TOML manifest at `crates/sunrise-error/codes.toml`.
- The Rust enum at `crates/sunrise-error/src/codes.rs` and the TypeScript enum at `packages/sunrise-error-ts/src/codes.ts` are **generated** mirrors. Hand-editing either generated file is a CI failure.
- Codes are added at minor-version boundaries; never reused, never renamed.
- Adding a code requires updating the manifest. CI checks that ids are monotonically increasing and never re-used.
- Ids 203 (`AUTH_QUOTA_EXCEEDED`) and 300 (`STORAGE_QUOTA_EXCEEDED`) were removed under ADR-0027 (self-host first), which takes per-account quotas out of v1; nothing ever emitted either. Both ids are **burned** — never re-issued under another name — which is why the auth block continues at 204 (`AUTH_DEVICE_SIG_INVALID`) and the storage block at 304.
- An older client receiving an unknown code maps it to `INTERNAL_UNKNOWN_CODE` and preserves the original wire string in `diagnostic` for support tooling.

## Error envelope (core → UI)

```rust
pub struct CoreError {
    pub code: ErrorCode,
    pub recoverable: Recoverability,
    pub diagnostic: String,           // dev-only, never user-facing verbatim
}

pub enum Recoverability {
    Transient,                        // retry
    UserActionRequired,               // user intervention will resolve
    Fatal,                            // engineer needed
}
```

UI maps `code` → localized user copy + (optionally) an action button.

## What the user sees

- **Transient:** small unobtrusive toast, auto-dismiss, no action.
- **UserActionRequired:** persistent banner + button (e.g. "Sign in again" for `INTEGRATION_REAUTH_REQUIRED`).
- **Fatal:** modal-ish dialog with "Send diagnostics" affordance.

## Logging

- Errors logged at `warn` (transient), `error` (user-action), `error` + `level=fatal` (fatal).
- Logs include code + diagnostic + a span ID; never include `Plain<T>` content.

## Recovery

- Transient retries are bounded with exponential backoff.
- The UI never silently re-runs a destructive operation. Retries happen only for *idempotent* operations.

### Canonical retry policy

```
initial_delay_ms = 100
max_delay_ms     = 30000
jitter_pct       = ±20%
max_retries      = 5
```

Applies only to errors with `kind: transient` AND `retryable: true`, and only to idempotent operations. v1 op writes are eligible because **the receiver** is idempotent, not because the request carries a key: a re-sent op is an `INSERT OR IGNORE` on an `op_id` derived from `(stream_id, device_id, seq)`, so a second copy materializes nothing and raises no event. `batch_id` is a correlation id and the relay dedups on nothing — see [05-sync/wire-protocol.md](../05-sync/wire-protocol.md). An operation whose receiver has no such gate never auto-retries.

## Uncaught panics

- Rust panics in the core are caught at the FFI boundary and surfaced as `CoreError(FATAL_INTERNAL, …)` rather than crashing the host.
- A panic generates a crash report (off-by-default; user opt-in for delivery).

## Validation errors

- Validation runs at the command boundary in the core.
- Returns `VALIDATION_*` codes with a structured `diagnostic` object.
- UI surfaces inline at the offending field, keying off `diagnostic.field`.

The wire shape of a validation error:

```json
{
  "code": "VALIDATION_FIELD",
  "kind": "user",
  "retryable": false,
  "diagnostic": {
    "field": "title",
    "constraint": "max_length",
    "limit": 512,
    "actual": 538
  }
}
```

`field` is dotted-path into the command payload; `constraint` is one of the documented constraint names per entity (`max_length`, `non_empty`, `before`, `after`, `pattern`, `enum`, …); `limit` and `actual` are the constraint's documented numeric or string operands. UIs must not parse `code` for human copy; they must read `diagnostic.field` to highlight the input and look up the localized message via the error-code registry.

## Cross-cutting principles

- Never show raw stack traces to users.
- Never mask an error by silently doing nothing — either retry or surface.
- Log enough to reproduce without ever logging content.
