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
| macOS | Swift + SwiftUI, core via UniFFI | **shipping** | [`desktop.md`](./desktop.md) |
| CLI | Rust, core linked in-process | **shipping** | this page, §CLI |
| iOS / iPadOS | Swift + SwiftUI + UniFFI core | **shipping** at SHOULD level ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)) | [`mobile-ios.md`](./mobile-ios.md) |
| Android | Kotlin + Jetpack Compose + UniFFI core | not built | [`mobile-android.md`](./mobile-android.md) |
| Web (PWA) | React + WASM core | not built; the core runs locally without sync ([ADR-0055](../11-adr/0055-web-wasm-core.md)), and the Pages deploy is wired but not enabled ([#11](https://github.com/justin13888/Sunrise/issues/11)) | [`web.md`](./web.md) |
| Terminal (TUI) | — | **removed** ([ADR-0019](../11-adr/0019-swiftui-macos-client.md)) | — |

"Not built" means specified and carrying no MUSTs — that is Android and Web,
each ranked on the roadmap ([`../roadmap.md`](../roadmap.md)) where an issue
exists. macOS and the CLI carry the MUSTs. The remaining client is neither:
iOS / iPadOS ships, and carries SHOULDs rather than MUSTs
([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)); moving it to parity is
roadmap phase P5. See [`parity-matrix.md`](./parity-matrix.md).

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
  sunrise schedule <id>... <when>|-
                               set or clear the plan, counting nothing
  sunrise done <id>...         complete one or more tasks
  sunrise drop <id>...         soft-delete one or more tasks
  sunrise note <id> [<text>...|-|--file <path|->] [--force]
                               print, replace or clear a task's note
  sunrise show <id>            one task's fields, each time with its kind
  sunrise undo | redo          reverse or replay this vault's last write
  sunrise today | inbox        list today's tasks / the inbox
  sunrise upcoming [7|14|30]   the next 7, 14 or 30 days, day by day, then
                               each stream's undated tasks
  sunrise next                 the focus planner's top picks
  sunrise search <query>...    full-text search

the vault's shape
  sunrise streams | contexts | routines
  sunrise stream <id|name>     list the tasks in one stream
  sunrise context <id|name>    list the tasks carrying one context
  sunrise streams move <x> before <y> | last
                               reorder the stream list; syncs, unlike task order
  sunrise streams create|edit|archive|unarchive|delete ...
  sunrise contexts create|edit|archive|unarchive|delete ...
  sunrise routines create <title>... --rule <RRULE> [--zone <IANA>] ...
  sunrise routines edit|archive|unarchive|pause|resume|delete <id|title>
  sunrise routines skip <id|title> [--at <occurrence-key>]

calendar
  sunrise blocks [day|week] [<when>]
  sunrise block create --from <when> (--to <when> | --for <length>) ...
  sunrise block move <block-id> --from <when> [--to <when>]
  sunrise block bind|unbind <block-id> <task-id>...
  sunrise block merge <block-id> <block-id>
  sunrise block delete <block-id>

calendar interchange
  sunrise ical import <path|-> read an .ics into Blocks, idempotently
  sunrise ical export [today|day|week] [path]
                               write a window of Blocks as .ics

review and reporting
  sunrise review               this week's review summary
  sunrise review daily         the glance since the start of yesterday
  sunrise review save [note]   record that the week was reviewed
  sunrise brief morning|evening
  sunrise reminders plan       this device's reminders for the next day
  sunrise export <trends|activity|focus|streaks> [json|csv] [path]
  sunrise export all <dir> [--no-fetch]
                               everything, decrypted, in the open export format

views and preferences
  sunrise views [save <name> <view> [--query q] [--contexts a,b] | apply <name> | delete <name>]
  sunrise prefs [list | get <key> | set <key> <value> [--device] | clear <key> [--device]]

attachments
  sunrise attachments <task-id>
  sunrise attachment add <task-id> <path> | remove <id> | get <id> [path] | cancel <id>

account
  sunrise vaults               list this machine's vaults, marking the open one
  sunrise login | logout | whoami

plumbing
  sunrise focus <id>           open a focus session on a task
  sunrise focus end [--done]   close every running session; --done completes
                               the task it was opened on
  sunrise sync --once          drain the outbox and exit (cron / CI)
  sunrise sync --watch [--rounds <n>]
                               keep draining until Ctrl-C / SIGTERM, backing
                               off while the relay is unreachable
  sunrise tz [set <IANA> | clear]
                               the zone times are read in, and its source
  sunrise rebuild              rebuild every row from this vault's own op log,
                               in one transaction; nothing is sent
```

### A client of the client layer

Every verb above is a lowering onto `sunrise-client-core`
([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md)
§The client layer), not a second implementation of it: `defer` is
`schedule::defer_command`, `schedule` is `schedule::scheduled_on_day`,
`block create` and `block move` are `calendar::new_block_draft` and
`calendar::moved_block_patch`, `block merge` is `calendar::merge_blocks`,
`routines edit` is `editor::routine_patch`, `views` is `views::SavedViewStore`,
`brief` and `review daily` are `brief` and `review`, `reminders plan` is
`reminders::schedule`, and `undo` / `redo` are `history::UndoHistory`. So the
CLI and the Apple apps cannot disagree about what "defer to tomorrow" does to a
floating 09:00, or about which rows a merge leaves behind.

The plural owns the verbs that change the set — `streams create`,
`contexts archive`, `routines skip` — the way `streams move` always has, because
the singular (`stream <name>`) lists what is *in* one and a verb there would
collide with a stream called `create`.

Two flags apply to every subcommand and are recognised anywhere on the line
(`sunrise today --json` and `sunrise --json today` are the same); a `--` ends
them, so `sunrise capture -- --json is a title` captures that title:

* `--json` — a query prints one JSON document on stdout instead of its table
  (§JSON output below).
* `--tz <IANA>` — the reader's zone for this one command (§The reader's zone).

### The reader's zone

The CLI never reads the host's zone itself
([ADR-0066](../11-adr/0066-times-survive-travel.md) §5): the core's injected
`Clock` does, and the CLI reports a better answer when it has one through
`Core::on_time_zone_changed`, the one entry point every client uses. So
queries, the capture and annotate parsers, `defer`, `schedule` and the client
layer all read in the same zone. In order of precedence:

1. `--tz <IANA>`, for one command;
2. `SUNRISE_TZ`, for a shell or a cron job;
3. `sunrise tz set <IANA>`, kept for this device in the vault directory
   (`cli-timezone`) and reported at every open; `sunrise tz clear` forgets it;
4. otherwise the OS's zone, as `SystemClock` reads it.

`sunrise tz` prints the zone in force and which of the four it came from
(`flag`, `env`, `set`, `system`). A name the tzdb does not know is refused with
a typed error naming it and its source, before the vault is opened — never
read as UTC (§3). When the OS names no zone and none is given, times are read
in UTC and the commands that parse a time say so on stderr. A zone change is
not an edit: nothing stored is rewritten, so a floating 09:00 is 09:00 in the
new zone and a zoned one keeps its own (`crates/sunrise-e2e/tests/cli_traveller.rs`
drives exactly that through the binary).

### JSON output

With `--json`, each query command prints exactly one JSON document followed by
a newline, and nothing else on stdout; notes still go to stderr. Field names
are a contract: one may be added, none is renamed or removed. They are built in
`crates/sunrise-cli/src/json.rs`, not serialised from the domain types, so a
change of the domain's serde names does not move them.

A **time** is `{kind, value, zone, at_ms}`: `kind` is `instant`, `zoned`,
`floating`, `all_day` (or a newer build's kind verbatim), `value` the stored
value's round-trippable rendering, `zone` a zoned value's own zone (else
`null`), and `at_ms` where it falls for this reader (`null` when it cannot be
placed). A **task** is `{id, title, state, stream, contexts, priority, energy,
estimate_s, scheduled_at, due_at, completed_at, deferred_count, blocked_by,
note, updated_at_ms}`, its times as above and `note` as Markdown text or
`null`.

| Command | Document |
|---|---|
| `today`, `inbox`, `search`, `stream`, `context`, `views apply` (task views) | array of tasks |
| `show <id>` | one task |
| `upcoming [7\|14\|30]` | `{today, span_days, days: [{date, start_ms, end_ms, blocks, planned, due}], no_date: [{stream, no_stream, tasks, total}]}`, `blocks` as in `blocks`, `planned`, `due` and `tasks` arrays of tasks |
| `next` | array of `{task, unblocks}` |
| `streams` | array of `{id, name, color, open_tasks, archived, paused, sort_order}` |
| `contexts` | array of `{id, name, description, tasks, archived}` |
| `routines` | array of `{id, title, rule, next_ms, paused, streak}` |
| `blocks [day\|week]` | array of `{id, title, stream, starts_at, ends_at, tasks}` |
| `devices` | array of `{id, nickname, platform, this_device, revoked, read_bounded, current, admitted_after_revocation}` |
| `identity [status]` | `{account, current, rotations, this_device_is_current, holds_recovery_key, can_sponsor}` |
| `attachments <task>` | array of `{id, filename, mime_type, size_bytes, state}`, `state` one of `here`, `downloading`, `partial`, `fetchable`, `never_uploaded` |
| `review` | `{window: {start_ms, end_ms}, totals: {completed, deferred, dropped, created, reopened}, streams: [{id, name, completed, deferred, untouched}], slipped: [task]}` |
| `review daily` | `{since_ms, inbox, today, blocked}`, each an array of tasks |
| `brief morning\|evening` | `{brief, headline, sections: [{id, title, tasks}]}` |
| `reminders plan` | `{enabled, zone_unknown, planned: [{identifier, fire_at_ms, kind, entity, title, body}], unscheduled: [{entity, kind, reason}]}` |
| `views` | array of `{name, spec, query, contexts}` |
| `prefs`, `prefs get <key>` | array of / one `{key, value, source, scope}`, `value` in the spelling `prefs set` reads |
| `tz` | `{zone, source, zone_unknown}` |
| `sync status` | `{badge, pending, known_incomplete}`, `badge` one of `synced`, `sending`, `catching_up`, `offline`, `changes_missing`, `stopped` (the client layer's `sync_status::sync_badge`) |

Write commands, `export` (which has its own `json` format), `export all`
(whose `manifest.json` is its document, [`../04-storage/export-format.md`](../04-storage/export-format.md))
and the account commands print as they always have.

### Undo across processes

`undo` reverses the most recent write this vault recorded, and `redo` replays
the most recent undo; every write verb records into the history, through
`UndoHistory::submit`. A CLI session is one process, so the history is written
back into the vault directory after each write (`cli-undo`) and read at the next
open. It is task content, so it is sealed — XChaCha20-Poly1305 under a key
derived from the vault's root, which never lives in the vault directory —
rather than written beside the ciphertext as plaintext. Its encoding is the
client layer's (`history::persist`), which keeps "clear this field" and "leave
it" apart: plain serde collapses both to `null`, and the undo of setting a
priority on a task that had none would then do nothing.

A delete, a revocation, a focus session and a routine skip have no inverse;
they are still written, and leave nothing to undo. An undo is a new write and
merges like one, so a field another device has changed since loses the tie
exactly as a manual edit would. A history that does not open — another
vault's, a damaged file — is a fresh one, with a note on stderr.

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
keystore. It is an explicit dev affordance for handing two vaults the same
at-rest key, and how to open a vault created before per-vault keys existed. It
does **not** make them one account: each still mints its own identity, so
neither reads the other's ops. Joining an account is `sunrise pair`. Such a vault is refused
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

`defer` is not `edit ^when`. It is `Command::DeferTaskTo`, which also bumps
the Task's `deferred_count` — the counter `sunrise review` reports as
"deferred", and the signal the weekly review exists to surface. A day
(`tomorrow`, `next friday`, `+3d`) counts civil days in the reader's zone and
the plan keeps its kind and time of day, so a floating 09:00 deferred to
tomorrow is a floating 09:00 tomorrow wherever its owner is
([ADR-0066](../11-adr/0066-times-survive-travel.md) §4); a phrase that names a
time (`tomorrow 3pm`) is the user choosing it, and is written as typed. It used
to send `Command::DeferTask`, an instant, which pinned a travelling plan to
the zone the command happened to run in.

`schedule` sets the plan without counting a deferral: a day is that day, all
day, because "schedule for tomorrow" names no time of day; `-` clears it.
`show` prints every stored time with its kind (`[floating]`, `[all_day]`,
`[instant]`, or a zoned value's own zone), because a rendering that hides the
kind is how "sometime Tuesday" became "Monday 20:00".

`export` writes to **stdout** unless given a path, so it pipes into `jq`. That
is the general rule: stdout is the contract, human-facing notes go to stderr,
and log records go to a file
(`$XDG_STATE_HOME/sunrise/log/sunrise-cli.ndjson`) so neither stream is ever
corrupted by NDJSON.

`sync --once` is a **bounded** wait, not a loop: a cron job that hangs forever
because the relay is down is worse than one that fails, because nothing
downstream ever runs. `sync --watch` is the loop, for a terminal left open or a
service manager: each round is that same bounded wait; a live round is followed
by a five-second pause, a failed one by a delay that starts at one second,
doubles to a minute, and resets on the next live round. It prints
`sync: live, outbox empty` each time it becomes live and a retry line on stderr
each time a round fails, and Ctrl-C or `SIGTERM` ends it at once with exit 0 —
the core writes each op in its own transaction, so a round cut short leaves
nothing half applied. `--rounds <n>` stops it after `n` live rounds. The loop's
policy is `crates/sunrise-cli/src/watch.rs`, written against an injected round,
sleep and stop so it is unit-tested without a relay or a signal. A failed round
names the client layer's sync badge (`offline`, `stopped`, `changes_missing`…),
because those call for different things: offline retries on its own, the
other two do not. `sync status` prints that badge and the outbox depth, offline
or not. `SUNRISE_SYNC_URL` is checked by the client layer's relay-URL rule
(`relay_url::parse_relay_url`: `https://`, or `http://` to this machine, and
nothing past the port) before `sync`, `bootstrap` or `recover` dial it.

The CLI is also how the whole stack is proven without any UI —
`cargo test -p sunrise-cli` drives the real binary against a real vault, and a
real relay in-process. That is a standing requirement, not a transitional one.

## Sub-specs in this directory

- [`parity-matrix.md`](./parity-matrix.md) — what each client must / should / may support.
- [`shared-ui-system.md`](./shared-ui-system.md) — design tokens and components shared cross-platform.
- [`interaction-patterns.md`](./interaction-patterns.md) — common gestures and keyboard idioms.
- [`releasing.md`](./releasing.md) — operator runbook for the signed, notarized macOS `.dmg`.

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
quality. Compose Multiplatform remains the strongest contender if that choice is revisited.

## Distribution

| Client | Channel |
|---|---|
| macOS | Direct `.dmg`, Developer ID-signed, notarized and stapled, attached to the GitHub Release beside the CLI tarballs. **Not** the Mac App Store, which would require the App Sandbox and so cost global-hotkey reliability — decided in [ADR-0031](../11-adr/0031-macos-distribution.md); built by `release.yml`'s `macos-app` job; operated per [`releasing.md`](./releasing.md). Updates afterwards go through Sparkle over an EdDSA-signed `appcast.xml` on the same Release, `stable` and `beta` — [ADR-0038](../11-adr/0038-macos-update-feed.md), and [`desktop.md`](./desktop.md) §Update channel |
| CLI | Cargo, Homebrew, prebuilt binaries on GitHub releases |
| iOS / iPadOS | App Store Connect. An `app-vX.Y.Z` tag builds, signs and uploads a build, which lands in TestFlight; submitting one for App Store review and releasing it are manual acts outside this repository, so the App Store version stream runs on its own cadence — [ADR-0039](../11-adr/0039-ios-distribution.md), built by `release.yml`'s `ios-release` job. That job **no-ops visibly** until the six secrets and the App Store Connect app record exist, which only the account holder can create. Locally it still installs ad-hoc on the simulator (`CODE_SIGN_IDENTITY=-`) via `mise run ios-run`; ad-hoc signing is not optional, because iOS gates the Keychain on an application-identifier entitlement only a signed binary carries |
| Web | Static assets on Cloudflare Pages, Pages project `sunrise-web` (`apps/web/wrangler.toml`). An `app-vX.Y.Z` tag builds and deploys them via `release.yml`'s `web-pages` job; a prerelease tag goes to the `prerelease` preview alias, never production. That job **no-ops visibly** until the owner creates the Pages project and the `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` secrets — the intended state while the web core cannot sync. The relay is chosen per browser at run time, so one deployment serves every self-hosted relay — [`web.md`](./web.md) §Self-host vs managed cloud |

The channel for Android, which is not built, is decided when it is built.

## Versioning

Each client has its own version, independent of server version. Within a major
version, all clients support the previous protocol version (N-1) and the
current one. CI runs end-to-end tests across version pairs.
