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

There is no Windows or Linux desktop client
([ADR-0019](../11-adr/0019-swiftui-macos-client.md)).

## CLI

`sunrise` (crate `sunrise-cli`) is a one-shot command-line client: open the
vault, do the thing, print the result, exit. It links the core directly — no
daemon, no socket, no second process.

`sunrise help` is the command list: each verb, its arguments and a line on
what it does. Its text is `[cli.usage]` in `i18n/en.toml`. This page states the
contracts the list cannot: the JSON output, the reader's zone, pairing, undo,
and the rules each verb follows.

### A client of the client layer

Every verb is a lowering onto `sunrise-client-core`
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
estimate_s, scheduled_at, due_at, target_at, completed_at, deferred_count,
blocked_by, note, updated_at_ms, sort_order, cancel_reason,
late_acknowledged_at_ms, routine, hard_constraint_violated}`, its times as above, `note` as
Markdown text or `null`, and `sort_order` the task's manual-order key or `null`
when nobody placed it. `scheduled_at` is the plan (`planned_at`) and `due_at`
the hard deadline (`hard_due_at`), under the names they always had here;
`target_at` is the soft deadline (ADR-0047); `cancel_reason` is a dropped
task's reason (`null` unless it is cancelled with one), and
`late_acknowledged_at_ms` when it was last kept in the triage queue,
`routine` the id of the routine that generated the task (`null` for any other
task), and `hard_constraint_violated` `true` for a routine occurrence planned
where it breaks one of its routine's `hard` constraints, else `false`. A **lateness** is `{state,
since_ms}`: `state` one of `late_hard`, `late_soft`, `stale`, `on_track`, and
`since_ms` when it began, `null` when on track.

| Command | Document |
|---|---|
| `today` | array of tasks in Today's group order, each with `today_group` (`planned`, `carried_over`, `due_today`, `late`), `deadline` (`hard` or `target` for a `due_today` row, else `null`) and `lateness` |
| `inbox`, `stream`, `context`, `views apply` (task views) | array of tasks; `inbox`, `stream` and `context` in manual order |
| `move <id>... before\|after <id>`, `move <id>... first\|last` | array of tasks: the stream's list in its new order |
| `search [--kind <k>] [--all] <query>` | `{canonical, notices: [{code, start, end, detail}], groups: [{kind, total, hits: [{id, opens, title, snippet, task}]}]}`, groups in the core's fixed order; `title` and `snippet` are `{text, ranges}` with each range `[start, end]` in Unicode scalars, `snippet` `null` when the body did not match, and `task` the whole task for a task hit, else `null`. A script reads the tasks as `.groups[] \| select(.kind == "task") \| .hits[].task` |
| `show <id>` | one task, with its `lateness` |
| `triage`, `triage list` | array of tasks in the triage queue's order, each with its `lateness` |
| `upcoming [7\|14\|30]` | `{today, span_days, days: [{date, start_ms, end_ms, blocks, planned, due, targets}], no_date: [{stream, no_stream, tasks, total}]}`, `blocks` as in `blocks`, `planned`, `due` (hard deadlines), `targets` (soft ones) and `tasks` arrays of tasks |
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
| `reminders plan` | `{enabled, zone_unknown, planned: [{identifier, fire_at_ms, kind, entity, title, body}], unscheduled: [{entity, kind, reason}], wind_down, catalog: [notification], alerts: [notification]}`, `wind_down` `{identifier, date, fire_at_ms, sleep_at_ms}` or `null` when this device fires none, `catalog` the notification kinds' scheduled notifications and `alerts` the ones posted now, each notification `{identifier, kind, fire_at_ms, deferred_from_ms, entity, device, subject, count, title, body, link, level, actions}` |
| `reminders kinds` | array of `{kind, enabled, always, key, level, delivery, audience, actions}`, `always` `true` for a security kind, which cannot be turned off |
| `day-schedule [show [<date>]]` | `{date, start_ms, end_ms, window, plannable, wind_down_ms, schedule}`: the planner day holding now (or `<date>`'s), `window` its `{wake, sleep}` as `HH:MM` (`null` when unset), `plannable` `{start_ms, end_ms}` or `null`, `wind_down_ms` that night's wind-down or `null`, and `schedule` `{weekday: {MO: window, …}, month_day: {"1": window, "-1": window, …}, date: {"2026-12-25": window \| "none", …}}` |
| `views` | array of `{id, name, view, spec, query, contexts, context_ids, streams, sort, sort_order}`: the vault's saved views (#341), `spec` the one-line summary, `query` the filter's free text, `contexts` the contexts' current names (with any name an import could not resolve) and `context_ids` their ids |
| `prefs`, `prefs get <key>` | array of / one `{key, value, source, scope}`, `value` in the spelling `prefs set` reads |
| `tz` | `{zone, source, zone_unknown}` |
| `sync status` | `{badge, pending, known_incomplete}`, `badge` one of `synced`, `sending`, `catching_up`, `offline`, `changes_missing`, `stopped` (the client layer's `sync_status::sync_badge`) |
| `conflicts [list] [<id>] [--all]` | array of `{entity, field, class, winner, losers, edited_while_deleted, cycle}`: `class` one of `tracked`, `state`, `tombstone`, `dependency_cycle`, and with `--all` `quiet` or `merge`; for `dependency_cycle` (a `blocked_by` edge or a `parent_id` the cycle rule sets aside), `winner` is that edge's own write and `cycle` the loop it would close, as ids from `entity` back to `entity`, empty for every other class; `winner` and each of `losers` a write `{version, device, origin, at_ms, value}`, `origin` `user` or `generated`, `value` the written value as JSON (`null` for a write that left the field unset); `edited_while_deleted` an array of `{version, device, fields}`. Empty when nothing is in conflict |

`pair join` and `pair add` wait on the other device, so with `--json` each
prints one JSON object per line as it happens rather than one document at the
end: `{event: "code", code}` (on `join`: the code, exactly as `pair add`
takes it), `{event: "sas", sas}` (the six digits, ungrouped), and on success
`{event: "paired"}`, which on `join` also carries `device` (this device's id,
hex) and `relay_device` (the relay's id for it, `null` if registration
failed). A pairing that does not pair prints no outcome event and exits
non-zero with the reason on stderr.

Write commands, `export` (which has its own `json` format), `export all`
(whose `manifest.json` is its document, [`../04-storage/export-format.md`](../04-storage/export-format.md))
and the account commands print as they always have.

### Conflicts and restore

`sunrise conflicts` lists what `Query::Conflicts` reports
([`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md)
§Seeing and settling a conflict): per entity and field, the write shown and
each write kept, with its value and its version. The table prints a version
as the writing device's first eight hex digits and its `seq`
(`3567b6be/2`), and `--json` as the whole op-ref,
`<stream hex>.<device hex>.<seq>`. `conflicts resolve <id> <field> --pick
<version>` settles the field on the write that version made, by either
spelling; `--value <v>` on a new value, read as JSON when it parses as JSON
and as plain text otherwise, so `--value 'Renew passport'` and `--value 3`
both need no quoting and `--value '"3"'` is the text. `sunrise restore <id>`
undeletes a task, stream, context, routine or block. In a vault that
requires `core.field_ops` both write one `Patch` of the one field naming
every head of the entity, which settles that field alone (ADR-0065 §4); in
one still in legacy mode, a full-state op, which settles every field of the
entity on the values this device shows (ADR-0065 D4, D9, D11).

### Pairing

`sunrise pair join` and `sunrise pair add` add a device over the relay the
way the Apple apps do, through the same client-layer code: `PairingFlow`'s
transitions, run to the end by `PairingRun` against `RelaySeam`, whose calls
are the `RelayPairing` the UniFFI seam wraps
(`crates/sunrise-client-core/src/pairing/run.rs`;
[`../03-crypto/pairing-and-onboarding.md`](../03-crypto/pairing-and-onboarding.md)).
The CLI's own part (`crates/sunrise-cli/src/pair_relay.rs`) is the arguments,
the printing, the answer to the digits and the vault directory.

```
  device being added                    device with the vault
  sunrise pair join
    prints the pairing code      -->    sunrise pair add '<code>'
    both print the same six digits, and each user confirms them
    opens the vault it was handed       certifies the new device's keys
```

The device **being added** shows the code, as in the apps: it starts the
Noise handshake, and the code carries the static key the other device checks
the handshake against. `join` needs `SUNRISE_SYNC_URL` and a bearer —
`SUNRISE_SYNC_TOKEN`, a stored sign-in in its vault directory, or, with
`SUNRISE_OIDC_ISSUER` and `SUNRISE_OIDC_CLIENT_ID` set, a sign-in it runs
then, because a device with no vault cannot run `sunrise login`. It refuses a
directory that already holds a vault, and once paired it registers itself with
the relay and records the id, as `sunrise bootstrap` would. `add` needs the
vault that created the account (`identity status` says whether this one
`can_sponsor`), and a sign-in like any relay command. `--email` hashes the
account's address into the code as the four-byte tag the apps show.

Nothing is sent until the digits are confirmed. `--yes` accepts them;
`--confirm-sas <digits>` accepts only the digits this device shows (spaces and
dashes ignored); with neither, the command asks on stderr for the digits the
other device shows and reads them from stdin. Anything else declines, which
ends the pairing on both devices with no vault and no certificate. A relay
that does not answer, or will not open a session, is reported with the file
exchange — `pair offer`, `request`, `issue`, `accept` — as the way that still
works; Ctrl-C cancels and drops the relay session.

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

A delete is undone by a restore (`Command::Restore`, ADR-0065 §3), and a
restore by the delete. A revocation, a focus session, a routine skip and a
conflict resolution have no inverse; they are still written, and leave
nothing to undo. A resolution settles a conflict, and no write can bring the
concurrent writes back. An undo is a new write and
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
at-rest key. It does **not** make them one account: each still mints its own
identity, so neither reads the other's ops. Joining an account is
`sunrise pair`.

It is also how to open a vault created before per-vault keys existed. Without
it, such a vault is refused with its own typed error rather than opened by
guessing the old constant, and the error quotes that constant so the data can
be read out with `SUNRISE_VAULT_ROOT`; the refusal follows
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
time (`tomorrow 3pm`) is the user choosing it, and is written as typed.

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
