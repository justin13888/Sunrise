---
status: draft
---

# Features — Overview

User-visible feature specs. Each spec defines the user contract; implementations live across [`../07-clients/`](../07-clients/) and [`../01-architecture/shared-core.md`](../01-architecture/shared-core.md).

| Spec | Topic |
|---|---|
| [`inbox-and-capture.md`](./inbox-and-capture.md) | Fast capture + Inbox triage |
| [`planning-views.md`](./planning-views.md) | Today, Upcoming, Stream view, saved views, calendar |
| [`focus-mode.md`](./focus-mode.md) | Single-task focus with timer |
| [`time-blocking.md`](./time-blocking.md) | Calendar grid + Block bindings + external calendar bridge |
| [`recurrence-engine.md`](./recurrence-engine.md) | Routine generation |
| [`search.md`](./search.md) | Local FTS with structured operators |
| [`notifications.md`](./notifications.md) | Local reminders + peer-driven notifications |
| [`reviews-and-stats.md`](./reviews-and-stats.md) | Weekly review flow + per-Stream stats |
| [`automation.md`](./automation.md) | Limited if-this-then-that rules |
| [`keyboard.md`](./keyboard.md) | Per-platform key maps + vim mode |

## Feature interactions

```
                  ┌──────────────┐
                  │ Capture/Inbox│
                  └──────┬───────┘
                         ▼
                  ┌──────────────┐
                  │   Streams    │ ◀───── Sharing (cross-user)
                  └──┬────┬──────┘
                     │    │
            ┌────────┘    └────────┐
            ▼                      ▼
      ┌──────────┐           ┌──────────────┐
      │ Routines │ ──────▶   │ Tasks        │
      └──────────┘           └────┬─────────┘
                                  │
                ┌─────────────────┴─────────────────┐
                ▼                ▼                  ▼
          ┌────────┐       ┌──────────┐      ┌────────────┐
          │ Today  │       │  Focus   │      │ Time-blocks│
          └────────┘       └──────────┘      └────────────┘
                                                    │
                                                    ▼
                                       Calendar integration
```

## Feature flags

Some features ship behind a flag (default off) for early users:

- `automation_rules` — power-user feature.
- `live_activities` (iOS) — gates Live Activities until they're polished.
- `web_push` — iOS web push works in Safari 16.4+.
- `tui_daemon` — daemon mode.

Flags are sync-only (a user enables on one device, applies on all). See [`../10-cross-cutting/feature-flags.md`](../10-cross-cutting/feature-flags.md) (referenced; v1 keeps flags simple in-vault).
