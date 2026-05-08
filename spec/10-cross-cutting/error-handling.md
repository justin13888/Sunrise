---
status: accepted
---

# Error Handling

Errors flow up the stack with a stable code at every boundary. UIs translate codes into user-visible copy.

## Codes

A finite enum across the system. Examples:

- `AUTH_DEVICE_REVOKED`
- `AUTH_TOKEN_INVALID`
- `STORAGE_QUOTA_EXCEEDED`
- `STORAGE_VAULT_LOCKED`
- `SYNC_PROTOCOL_VERSION_MISMATCH`
- `SYNC_TAMPER_DETECTED`
- `SYNC_NETWORK_UNAVAILABLE`
- `CRYPTO_DECRYPT_FAILED`
- `CRYPTO_RECOVERY_BLOB_INVALID`
- `INTEGRATION_REAUTH_REQUIRED`
- `VALIDATION_INVALID_TITLE`
- `VALIDATION_DUE_BEFORE_SCHEDULED`

Codes are stable across versions; new codes can be added but never repurposed.

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

## Uncaught panics

- Rust panics in the core are caught at the FFI boundary and surfaced as `CoreError(FATAL_INTERNAL, …)` rather than crashing the host.
- A panic generates a crash report (off-by-default; user opt-in for delivery).

## Validation errors

- Validation runs at the command boundary in the core.
- Returns `VALIDATION_*` codes with field-specific diagnostic.
- UI surfaces inline at the offending field.

## Cross-cutting principles

- Never show raw stack traces to users.
- Never mask an error by silently doing nothing — either retry or surface.
- Log enough to reproduce without ever logging content.
