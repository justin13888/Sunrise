---
status: accepted
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

## No feature flags

v1 has no per-user feature gates and no flag-service plumbing. The product is opinionated: every feature listed above ships on every supported platform, or it doesn't ship at all. Platform capability is detected at runtime (e.g. iOS web push lights up on Safari 16.4+ automatically); it isn't user-configurable.
