---
status: accepted
---

# Clients — Overview

Sunrise has **two** graphical clients — macOS and iOS / iPadOS, compiled from
one shared view layer in `apps/apple` — and **one** headless one. All three
consume the same shared core ([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md)),
so they cannot disagree about what a task is or when a routine fires.

| Client | Tech | Status | Spec |
|---|---|---|---|
| macOS | Swift + SwiftUI, core via UniFFI | **v1** | [`desktop.md`](./desktop.md) |
| CLI | Rust, core linked in-process | **v1** | this page, §CLI |
| iOS / iPadOS | Swift + SwiftUI + UniFFI core | **v1** at SHOULD level ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)) | [`mobile-ios.md`](./mobile-ios.md) |
| Android | Kotlin + Jetpack Compose + UniFFI core | deferred | [`mobile-android.md`](./mobile-android.md) |
| Web (PWA) | React + WASM core | deferred ([ADR-0012](../11-adr/0012-web-wasm-deferred.md)) | [`web.md`](./web.md) |
| Terminal (TUI) | — | **removed** ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)) | — |

"Deferred" means specified, not scheduled, and carrying no MUSTs — that is
Android and Web. macOS and the CLI carry the v1 MUSTs. The remaining client is
neither: iOS / iPadOS ships, and carries SHOULDs rather than MUSTs until a
release is cut ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)). See
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
  sunrise edit <id>... <tokens>...
                               change a task's fields (annotate grammar)
  sunrise retitle <id> <text>...
                               give one task a new title
  sunrise defer <id>... <when> push tasks out, counting the deferral
  sunrise done <id>...         complete one or more tasks
  sunrise drop <id>...         soft-delete one or more tasks
  sunrise today | inbox        list today's tasks / the inbox
  sunrise next                 the focus planner's top picks
  sunrise search <query>...    full-text search

the vault's shape
  sunrise streams | contexts | routines
  sunrise stream <id|name>     list the tasks in one stream
  sunrise context <id|name>    list the tasks carrying one context
  sunrise streams move <x> before <y> | last
                               reorder the stream list; syncs, unlike task order

calendar interchange
  sunrise ical import <path|-> read an .ics into Blocks, idempotently
  sunrise ical export [today|day|week] [path]
                               write a window of Blocks as .ics

review and reporting
  sunrise review               this week's review summary
  sunrise export <trends|activity|focus|streaks> [json|csv] [path]

account
  sunrise vaults               list this machine's vaults, marking the open one
  sunrise login | logout | whoami

plumbing
  sunrise focus <id>           open a focus session on a task
  sunrise focus end [--done]   close every running session; --done completes
                               the task it was opened on
  sunrise sync --once          drain the outbox and exit (cron / CI)
```

## Multi-account, on the CLI

`SUNRISE_VAULT` names the vault directory, and each one is a separate
**account**: the first open of a directory mints a random 32-byte vault root
for it, and every subsequent open reads that same root back. Two vaults
therefore share no data, no SQLCipher key, and no Stream keys.

The roots live in a keystore — `SUNRISE_KEYSTORE`, default
`$XDG_DATA_HOME/sunrise/keys` — one mode-0600 file per vault, keyed by an id
the vault directory carries in a `vault-id` file. This is the same split the
macOS app makes between its vault registry and the login Keychain, for the same
reason: **the key is deliberately not in the vault directory.** A vault
directory copied on its own has to stay ciphertext, or its encryption is
decorative. The cost is that a backup must include both; a vault whose key is
missing says exactly that rather than failing as a corrupt read.

`SUNRISE_VAULT_ROOT` (64 hex characters) supplies a root outright and touches no
keystore. It is how two vaults are told to be one account until pairing lands —
the same kind of explicit dev affordance as `SUNRISE_TRUST_CERT_FILE` — and how
to open a vault created before per-vault keys existed. Such a vault is refused
with its own typed error rather than opened by guessing the old constant, and
the error quotes that constant so the data can be read out; the refusal follows
the precedent of the `BASELINE_STORAGE_V = 13` refusal in
[`../11-adr/0018-storage-baseline-reset.md`](../11-adr/0018-storage-baseline-reset.md).

There is no passphrase, here or on macOS: the root is random, something local
holds it, and a second device is meant to get it by pairing rather than by the
user retyping anything.

Capture takes the same grammar every surface does —
`#stream @context ^when !priority ~duration *due:when*` — because it calls the
same parser (`sunrise_domain::capture`).

`edit` is the other half of that grammar: `sunrise_domain::annotate`, applied
to a Task that already exists, so `#stream @ctx @-ctx !N %energy ~30m ^when
due:when` reach the facets a triage pass changes, and a trailing `-` clears one.
It reaches six of `TaskPatch`'s fifteen fields — priority, energy, estimated
duration, scheduled, due and contexts — plus the Stream, which travels as
`Command::PromoteToStream` rather than as a patch field because the move
re-keys the task's storage. **The title is not among them**, and neither is the
`body`: a bare word is refused rather than read as a new title. That refusal is
the deliberate difference from capture, and it goes one step further: **one bad
token rejects the whole line**. A capture line is a title, so unrecognised text
belongs in it; an edit line is not, and a script that mistyped one token is
better served by a non-zero exit than by four of its five changes landing.

`retitle` is the title, and it is a **separate verb** precisely so that refusal
can stand. A title is free text that will sooner or later contain a `#` or a
`!`, so a title token inside the annotate grammar — or a `--title` flag, which
has to swallow the rest of the line — would make a bare word mean "title text"
in one reading and "malformed token" in another, and `edit` would have to stop
refusing it to tell the two apart. Split in two, each verb keeps the rule that
is right for it: `retitle`'s tail is a title by construction and absorbs
anything, exactly as `capture`'s does, while `edit`'s stays all grammar.

It takes **one** task where the other mutating verbs take `<id>...`: a title is
the field that tells two tasks apart, so one title applied to five is a mistake
worth refusing rather than a bulk operation worth offering. Empty is refused
too — `!-` clears a priority, and nothing clears a title.

`focus end` closes what `focus <id>` and `next` opened, so a session started
here no longer has to be closed from the app. It looks the session up rather
than taking an `fcs_` id, because neither of those commands ever printed one,
and it closes **every** running session: two devices starting concurrently both
mint a valid session, so "end my focus" means all of them. `--done` is the focus
screen's "complete" action, which also completes the task.

`defer` is not `edit ^when`. It is `Command::DeferTask`, which also bumps the
Task's `deferred_count` — the counter `sunrise review` reports as "deferred",
and the signal the weekly review exists to surface.

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
| iOS / iPadOS | TestFlight → App Store when a release is cut. Today it installs ad-hoc on the simulator (`CODE_SIGN_IDENTITY=-`) via `mise run ios-run`; ad-hoc signing is not optional, because iOS gates the Keychain on an application-identifier entitlement only a signed binary carries |

Channels for the deferred clients (Android and Web) are decided when they are
scheduled.

## Versioning

Each client has its own version, independent of server version. Within a major
version, all clients support the previous protocol version (N-1) and the
current one. CI runs end-to-end tests across version pairs.
