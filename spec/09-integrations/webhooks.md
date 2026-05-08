---
status: draft
---

# Outbound Webhooks

A user can wire automation rules (see [`../08-features/automation.md`](../08-features/automation.md)) to call HTTP endpoints when domain events occur.

## Mechanics

- Defined per rule.
- HTTP POST.
- Request body: JSON.
- Headers:
  - `X-Sunrise-Event` — event type.
  - `X-Sunrise-Signature` — HMAC-SHA-256 over the body using a per-rule shared secret.
  - `X-Sunrise-Delivery-Id` — UUID for idempotency at the receiver.
- Timeout: 10 seconds.
- Retries: exponential backoff, max 5 attempts.

## Body schema (default — no content)

```json
{
    "event": "task.created",
    "timestamp": "2026-05-08T10:00:00Z",
    "actor_device_id": "dev_…",
    "entity": { "kind": "task", "id": "tsk_…", "stream_id": "str_…" }
}
```

The body **does not include user content** (titles, bodies) by default.

## Body schema (extended — opt in per rule)

If the user explicitly opts in for a specific rule, the body may include:

```json
{
    …,
    "entity_summary": {
        "title": "Follow up with Carlos",
        "due_at": "2026-05-10T17:00:00Z"
    }
}
```

UI requires explicit opt-in with copy: "this will send the title of matching tasks to <URL>. Make sure you trust this destination."

## Where it runs

- On the primary scheduler device. Multi-device coordination prevents duplicate delivery.
- On a self-hosted server, the user might choose to run a small responder behind a private endpoint. We do not provide a hosted webhook receiver.

## Security

- Per-rule HMAC secret required.
- HTTPS only by default; HTTP allowed only in self-host mode with a config flag.
- Rate-limited per rule (default: 60 calls/hour per rule).
- Disabled if 100 consecutive failures.

## Use cases

- "Tell my home automation system that I've started focus mode."
- "Log to a Notion database via Notion's API gateway."
- "Send a message to a Slack incoming webhook when a high-priority task is created."

We are explicit that webhooks are a *power user* feature. The default user never sees them.
