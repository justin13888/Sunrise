---
status: draft
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

### Crash reports (opt-in, two-step consent)

- Stack trace.
- App version, OS version, device model.
- A bounded amount of recent log lines (with content scrubbed via tagged-types — see below).

The user is shown the report contents and clicks "Send" per crash. No silent uploading.

### Diagnostic bundle (one-shot, user-initiated)

When the user contacts support:

- Bundle includes: vault metadata stats (op count, schema version), recent sync errors, OS info.
- Excludes: any plaintext content, identifiers, attachments.
- The user reviews the bundle before sending. Signed by the user's device for authenticity.

## Tagged types for content

In Rust core code, plaintext content fields are wrapped in `Plain<T>`:

```rust
struct Plain<T>(T);
```

`Plain<T>` does not implement `Display`, `Debug`, or `Serialize`. To use, code must explicitly call `.expose()`. CI greps reject `Plain.*expose` in any file under `telemetry/` or `logging/`.

This is the structural enforcement that prevents accidental telemetry of content.

## Third-party SDKs

- We don't ship third-party analytics SDKs in clients. Telemetry, when enabled, is sent to a Sunrise-operated endpoint over a minimal HTTPS POST.
- Crash reporting uses a self-hosted Sentry-compatible endpoint or open-source equivalent. No cloud SaaS that could carry content.

## Server-side privacy

The server-side observability commitments are in [`../06-server/observability.md`](../06-server/observability.md). Briefly:

- IP retained ≤14 days.
- No content; no push tokens in plaintext.
- Audit log per account-management action retained 90 days.
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
