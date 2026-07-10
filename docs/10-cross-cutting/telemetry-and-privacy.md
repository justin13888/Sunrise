---
status: accepted
---

# Telemetry and Privacy

## Default

Telemetry is **off** by default. Sunrise works fully without any telemetry. The user opts in.

## Categories

### Anonymous usage stats (opt-in)

- App launches per device class.
- Feature flags surfaced to user.
- Duration in focus mode (anonymized; no task content).
- Crash frequency.

Sent as aggregated counters at most once a day. No user identifier; no per-event timestamps that could reidentify.

#### Canonical event envelope

Every telemetry event conforms to this CDDL. Shipped at most once daily; empty if the user opted out. CI asserts that no field outside this set ever appears in a serialized event.

```cddl
TelemetryEvent = {
  v:           1,
  event:       tstr,             ; e.g. "app.launch", "focus.session", "sync.session"
  bucket_ms:   uint,             ; truncated to nearest day for app.launch, hour for sync.session
  count:       uint,             ; aggregated count over the bucket
  device_class: "phone" / "tablet" / "desktop" / "tui",
  app_v:       tstr,
  os:          tstr,             ; "ios18", "linux-x86_64", ...
  schema_v:    1,
  sample_p:    float,            ; sampling probability (1.0 = full)
  ; event-specific fields below, all bucketed/aggregated:
  duration_ms_p50: uint / null,
  duration_ms_p95: uint / null,
  result:          tstr / null,  ; "ok" / "failed" / null
}
```

No PII fields are permitted; in particular, **email is never logged or telemetered, and there is no rotating auth-buffer keyed on email or any other identifier**. Cross-log identification, when needed, uses the per-salt BLAKE3 short hashes defined in [`logging.md`](./logging.md).

### Crash reports (opt-in, two-step consent)

- Stack trace.
- App version, OS version, device model.
- A bounded amount of recent log lines (with content scrubbed via tagged-types — see below).

The user is shown the report contents and clicks "Send" per crash. No silent uploading.

Consent flow:

1. App relaunches after a crash.
2. Modal: "Sunrise crashed. Send a diagnostic report?"
   - **Send** — uploads the bundle.
   - **Don't send** — discards the bundle.
   - **Review** — shows the redacted bundle before send/discard.
3. The bundle is stored locally for 7 days regardless of choice (so the user can later go to Settings → Diagnostics → "Send last crash").
4. If a second crash occurs before the user responds, append to the existing pending bundle and update the modal's count.

### Diagnostic bundle (one-shot, user-initiated)

When the user contacts support:

- Bundle includes: vault metadata stats (op count, schema version), recent sync errors, OS info.
- Excludes: any plaintext content, identifiers, attachments.
- The user reviews the bundle before sending. The bundle is signed with the device's Ed25519 signing key. The server verifies the signature on `/api/v1/diagnostics/upload` against the device's known public key; mismatch → `400 DIAGNOSTIC_INVALID_SIGNATURE`.

## Tagged types for content

In Rust core code, plaintext content fields are wrapped in `Plain<T>`:

```rust
struct Plain<T>(T);
```

`Plain<T>` does not implement `Display`, `Debug`, or `Serialize`. To use, code must explicitly call `.expose()`. The CI lint that enforces this is canonical and is specified in [`logging.md` §6.3](./logging.md#63-ci-enforcement); a forbidden-module call to `Plain<T>::expose()` fails the build with: `"Plain<T>::expose() called from a forbidden module: redaction may be bypassed. See docs/10-cross-cutting/logging.md."`

This is the structural enforcement that prevents accidental telemetry of content.

## Third-party SDKs

- We don't ship third-party analytics SDKs in clients. Telemetry, when enabled, is sent to a Sunrise-operated endpoint over a minimal HTTPS POST.
- Crash reporting uses a self-hosted Sentry-compatible endpoint or open-source equivalent. No cloud SaaS that could carry content.

## Server-side privacy

The server-side observability commitments are in [`../06-server/observability.md`](../06-server/observability.md). Briefly:

- IP retained ≤14 days.
- No content; no push tokens in plaintext.
- Audit log per account-management action retained 30 days (unified across the system; see [`../06-server/observability.md`](../06-server/observability.md)).
- Aggregate metrics retained indefinitely (no PII).

## Privacy policy alignment

Every commitment in this spec maps to a clause in the public privacy policy. Privacy policy changes that increase data collection require a 30-day in-app notice.

## User exports

- "Export my data" produces a decrypted archive (JSON + Markdown).
- "Download server-side metadata about me" produces a JSON of what the server holds (sync metadata, account record, blob refs).
- "Delete my account" wipes server data within 30 days.

## Government / legal requests

- We publish a transparency report annually.
- We receive requests; we respond per applicable law.
- We have nothing in the clear to give: content is encrypted; metadata is what we have.
- Self-hosted servers respond per their operator's policy.
