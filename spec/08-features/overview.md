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

Some features ship behind a per-user flag (default off) for early users:

- `automation_rules` — power-user feature.
- `live_activities` (iOS) — gates Live Activities until they're polished.
- `web_push` — iOS web push works in Safari 16.4+.
- `tui_daemon` — daemon mode.

Flags are stored in the vault-meta CRDT doc (`settings.feature_flags: { name: bool }`, LWW-register per name). Enabling on one device propagates via normal sync to all paired devices. There is no server-side flag service in v1; the relay does not gate features. There is no separate `feature-flags.md` cross-cutting spec — this section is the normative reference.
