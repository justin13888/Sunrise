---
status: accepted
---

# Clients — Overview

Sunrise has **one** graphical client and **one** headless one. Both consume the
same shared core ([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md)),
so they cannot disagree about what a task is or when a routine fires.

| Client | Tech | Status | Spec |
|---|---|---|---|
| macOS | Swift + SwiftUI, core via UniFFI | **v1** | [`desktop.md`](./desktop.md) |
| CLI | Rust, core linked in-process | **v1** | this page, §CLI |
| iOS / iPadOS | Swift + SwiftUI + UniFFI core | deferred | [`mobile-ios.md`](./mobile-ios.md) |
| Android | Kotlin + Jetpack Compose + UniFFI core | deferred | [`mobile-android.md`](./mobile-android.md) |
| Web (PWA) | React + WASM core | deferred ([ADR-0012](../11-adr/0012-web-wasm-deferred.md)) | [`web.md`](./web.md) |
| Terminal (TUI) | — | **removed** ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)) | — |

"Deferred" means specified, not scheduled, and carrying no MUSTs. See
[`parity-matrix.md`](./parity-matrix.md).

Windows and Linux desktop were specified in an earlier revision of this
directory and never built. [ADR-0019](../11-adr/0019-swiftui-macos-client.md)
stops claiming them.

## CLI

`sunrise` (crate `sunrise-cli`) is a one-shot command-line client: open the
vault, do the thing, print the result, exit. It links the core directly — no
daemon, no socket, no second process.

```
capture and triage
  sunrise capture <text>...    parse and commit one task, then exit
  sunrise done <id>...         complete one or more tasks
  sunrise today | inbox        list today's tasks / the inbox
  sunrise next                 the focus planner's top picks
  sunrise search <query>...    full-text search

the vault's shape
  sunrise streams | contexts | routines
  sunrise stream <id|name>     list the tasks in one stream
  sunrise context <id|name>    list the tasks carrying one context

review and reporting
  sunrise review               this week's review summary
  sunrise export <trends|activity|focus|streaks> [json|csv] [path]

plumbing
  sunrise focus <id>           open a focus session on a task
  sunrise sync --once          drain the outbox and exit (cron / CI)
```

Capture takes the same grammar every surface does —
`#stream @context ^when !priority ~duration *due:when*` — because it calls the
same parser (`sunrise_domain::capture`).

`export` writes to **stdout** unless given a path, so it pipes into `jq`. That
is the general rule: stdout is the contract, human-facing notes go to stderr,
and log records go to a file
(`$XDG_STATE_HOME/sunrise/log/sunrise-cli.ndjson`) so neither stream is ever
corrupted by NDJSON.

`sync --once` is a **bounded** wait, not a loop: a cron job that hangs forever
because the relay is down is worse than one that fails, because nothing
downstream ever runs.

The CLI is also how the whole stack is proven without any UI —
`cargo test -p sunrise-cli` drives the real binary against a real vault, and a
real relay in-process. That is a standing requirement, not a transitional one.

## Sub-specs in this directory

- [`parity-matrix.md`](./parity-matrix.md) — what each client must / should / may support.
- [`shared-ui-system.md`](./shared-ui-system.md) — design tokens and components shared cross-platform.
- [`interaction-patterns.md`](./interaction-patterns.md) — common gestures and keyboard idioms.

## What clients share

- The core (one library, one seam, many bindings).
- The wire protocol.
- The design tokens (colors, type scale, spacing).
- The mental model (Today, Streams, Inbox, Focus).
- The **vocabulary**. Recurrence phrasing, the annotate grammar, relative days,
  why the planner ranked something where it did — all of it lives in
  `sunrise-domain`, so the CLI and the app say the same thing about the same
  data. Two clients inventing their own wording for one fact is how a product
  starts contradicting itself.

## What clients don't share

- UI implementation. Each is native to its platform.
- Specific keyboard shortcuts (HIG-conformant per OS).
- Background and lifecycle behaviors (vastly different per OS).
- Notification scheduling APIs.

## Why not React Native / Flutter / Compose Multiplatform?

Considered and rejected in
[ADR-0007](../11-adr/0007-mobile-strategy.md), for reasons that have not
changed: the integrations that matter — global hotkey, menu bar, notification
actions, Spotlight — are exactly the ones a cross-platform toolkit makes
hardest. The shared core gets the de-duplication; the UI being native preserves
quality. Compose Multiplatform remains the strongest contender for a v2.

## Distribution

| Client | Channel |
|---|---|
| macOS | Direct signed & notarized `.dmg`; Mac App Store under evaluation (sandboxing costs global-hotkey reliability — see [`desktop.md`](./desktop.md)) |
| CLI | Cargo, Homebrew, prebuilt binaries on GitHub releases |

Channels for the deferred clients are decided when they are scheduled.

## Versioning

Each client has its own version, independent of server version. Within a major
version, all clients support the previous protocol version (N-1) and the
current one. CI runs end-to-end tests across version pairs.
