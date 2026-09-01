# 0019 — A native SwiftUI macOS app is the v1 client, over a UniFFI seam

**Status:** accepted

**Supersedes:** [ADR-0006](./0006-tui-framework.md) (Ratatui TUI).

**Replaces:** the Tauri 2 + React desktop specification formerly in
[`docs/07-clients/desktop.md`](../07-clients/desktop.md).

## Context

Three client stories were live in the tree at once, and only one of them was
real.

`docs/07-clients/desktop.md` specified a Tauri 2 + React desktop app across
macOS, Windows and Linux, with a tray, an updater, and a side-by-side stream
view. **It never existed.** `apps/desktop/src-tauri` had no `main.rs`, no
`tauri.conf.json`, and Tauri was not a dependency anywhere; the shell was cut by
decision and the spec was never amended. It remained the largest single
contradiction between `docs/` and the code.

What did exist was a 20k-LOC Ratatui TUI, shipped as the v1 client under
ADR-0006, and `docs/07-clients/tui.md` described it accurately. It worked. Seven
views, full CRUD over every entity, undo/redo, saved views, the weekly review,
and non-interactive subcommands. It was also, measured honestly:

* **~16k lines of rendering** — keymap, input line, hit-testing, render,
  runtime — against roughly 4k lines of logic that was not about terminals at
  all, and which no other client could reach because it lived in the terminal
  crate.
* **The reason the workspace pinned rustc 1.88** (ratatui 0.28's transitive
  `instability`/`darling`), carried an unmaintained-crate advisory suppression
  (`RUSTSEC-2024-0436`, `paste`, via ratatui), and needed a licence
  clarification for `icy_sixel` (via `ratatui-image`).
* **A dead end for four of the parity matrix's MUSTs** — the calendar grid,
  attachment viewing, drag-and-drop and lock-screen-class capture surfaces are
  not things a terminal does.

Meanwhile the epic's actual target user runs a Mac. Continuing to spend the
client budget on a terminal renderer, while shipping no graphical client at all,
was the wrong allocation — and maintaining *two* clients before the first one
was finished would have been worse.

Before committing, an executed spike built the whole pipeline end to end: a
UniFFI-annotated crate, a generated Swift binding, an xcframework, and a SwiftUI
app compiled under Swift 6 `-strict-concurrency=complete`. `mise run macos-app` went
from clean to BUILD SUCCEEDED with zero warnings from generated code, and 25
runtime assertions passed against a real core. Everything in the Decision below
was observed, not reasoned about.

## Decision

**The v1 graphical client is a native SwiftUI app for macOS**, in `apps/apple/`,
talking to `sunrise-core` through a **UniFFI seam** in
`crates/sunrise-core-bindings`. Every other client — iOS, Android, Web, and the
TUI — is **deferred**, with no date.

**The Ratatui client is deleted**, not frozen. A client kept "for later" that
nobody runs rots into a build tax; deleting it is the honest form of deferral,
and git preserves it.

**The headless surface survives as `sunrise-cli`.** The thirteen non-interactive
subcommands — `capture`, `today`, `inbox`, `next`, `focus`, `done`, `streams`,
`contexts`, `routines`, `search`, `review`, `export`, `sync --once` — move to
their own crate, with the binary named `sunrise`. This is deliberate and it is
load-bearing in two ways:

1. It keeps every layer of the stack drivable and testable **with no client at
   all**, which is what `crates/sunrise-cli/tests/cli.rs` does against the real
   binary and a real vault.
2. It is the honest remainder of what the TUI promised the SSH user. Capture,
   triage, review and export over SSH still work. Living in the terminal all day
   does not.

**Everything client-agnostic came out of the TUI before it went**, into the
crates where any client can reach it:

| What | Where it went |
|---|---|
| Plain-English recurrence, and its inverse | `sunrise-domain::recur` / `rrule::rrule_summary` |
| The annotate grammar (`#stream @ctx !N %energy ~30m ^when due:when`) | `sunrise-domain::annotate` |
| Routine list projection | `sunrise-domain::routine_gen::routine_rows` |
| Domain phrasing (`plan_reason`, `activity_phrase`, `relative_day`, …) | `sunrise-domain::phrase` |
| Undo/redo by inverse command; saved views | `sunrise-client-core` |

## The seam

`sunrise-core-bindings` exports one opaque async handle, the full command list,
the full query list, and a change stream. It is not a subset: the core's command
and query enums *are* the contract, and a seam that offered fewer would quietly
decide what clients are allowed to do.

`sunrise-domain` **does not depend on uniffi**. Unit enums cross via
`#[uniffi::remote(Enum)]`, which works across crate boundaries; ids, instants
and civil dates cross via `custom_type!` over the `Display`/`FromStr` pairs they
already had; structured types are mirrored in the bindings crate.

Five constraints the spike surfaced are part of this decision, because getting
any of them wrong produces a crash rather than a compile error:

* **No process-global runtime, no `block_on`.** UniFFI supplies the runtime;
  `block_on` inside its async context deadlocks. The handle captures
  `Handle::current()` in the async constructor.
* **`async_runtime = "tokio"` does not put *sync* exported methods on the
  runtime.** A bare `tokio::spawn` in one compiles and then panics — which
  reaches Swift as a trapped `rustPanic`, i.e. an app crash, not an error.
* **The exported task record is `TaskItem`, not `Task`.** A generated Swift
  `Task` shadows `_Concurrency.Task` and breaks every `Task { }` in the app.
* **`ChangeListener::on_lagged` is required.** The channel behind the change
  stream holds 256 events and is lossy past that; the spike measured
  `seen = 257, lagged = 4743` flooding 5000 events past a slow consumer, which
  is exactly what a sync catch-up burst looks like. A bridge implementing only
  `on_change` shows stale data after every burst, silently. `on_lagged` means
  "re-run your queries".
* **The bindgen tool is quarantined outside the workspace**, with
  `cargo-platform` pinned to 0.3.2. UniFFI's default features pull 0.3.3, which
  requires rustc 1.91 and hard-fails the workspace's 1.88 pin.

## What we give up

* **The terminal client, entirely.** Anyone using the TUI daily loses it. The
  CLI covers capture, triage, review and export; it does not cover living in the
  app. The parity matrix records the TUI column as deferred rather than
  pretending otherwise.
* **Windows and Linux desktop, for now.** They were specified and never built,
  so nothing shipped is being withdrawn — but the specification claimed them,
  and this ADR stops claiming them.
* **Cross-platform UI reuse.** A SwiftUI view has no path to Android or Web. The
  UniFFI seam is deliberately Kotlin-ready (the same scaffolding generates it),
  so what is *shared* is the core, which was already the plan under
  [ADR-0007](./0007-mobile-strategy.md); the UI was never going to be shared.
* **~430 tests.** They tested the renderer, the keymap, the hit-tester and the
  input line. What they proved was that a terminal drew correctly, and there is
  no longer a terminal. The client-agnostic coverage moved with the code it
  covers.
* **A client anyone can run over SSH on a headless box.** `sunrise` runs there;
  a SwiftUI app does not.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep the TUI as v1 and defer the GUI** | It is what the branch already did, and it is why the parity matrix has MUSTs a terminal cannot satisfy. It also leaves the desktop spec contradicting the code indefinitely. |
| **Keep the TUI *and* build the macOS app** | Two clients, one of them with a 16k-line renderer, before either is finished. Every domain change would land twice, and the TUI's dependency tax (rustc pin, advisory ignore, licence clarification) would persist for a client with no users. |
| **Build the Tauri desktop app that was specified** | Ships a webview and a second language runtime to render a task list, for three platforms at once, and the spec's own tray/updater/multi-window surface is a quarter of the work before any task is drawn. It also keeps the React/TS client stack alive, which [ADR-0012](./0012-web-wasm-deferred.md) already deferred. |
| **Compose Multiplatform / React Native / Flutter** | Considered and rejected in [ADR-0007](./0007-mobile-strategy.md) for reasons that have not changed: the integrations that matter (global hotkey, menu bar, notification actions, Spotlight) are exactly the ones a cross-platform toolkit makes hardest. |
| **`swift-bridge` instead of UniFFI** | Worse async and callback story, and no Android path — and the bindings crate's stated scope includes Kotlin. |
| **A hand-written C ABI** | 2500+ lines of marshalling to own by hand across 22 commands and 22 queries, to replace generated code. |
| **Keep the JSON facade in `sunrise-core-bindings`** | It is what was there: `serde_json` strings over a process-global runtime with `block_on` at every call site. It types nothing, it deadlocks inside a UniFFI async context, and every result would be parsed twice. |
| **A SwiftUI app for iOS first** | The spike only built `aarch64-apple-darwin`. iOS slices are unproven, and the target user's primary device is a Mac. |

## Consequences

* **`crates/sunrise-tui` is gone**, and with it `ratatui`, `crossterm`,
  `ratatui-image` and `image` from `[workspace.dependencies]`. The
  RUSTSEC-2024-0436 suppression and the `icy_sixel` licence clarification go
  with them; `deny.toml` now carries **no advisory suppressions at all**.
* **The rustc 1.88 pin stays.** It was raised for ratatui, but it is a
  reproducibility floor, not a workaround, and dropping back would be an
  unforced change.
* **`mise run apple-xcframework`** builds the slices, generates the Swift bindings
  in `--library` mode, and **rewrites** the generated module map — UniFFI's own
  is named `<lib>FFI.modulemap` and emits `use Darwin` / `use _Builtin_*` lines
  that do not resolve inside an xcframework.
* **The parity matrix's Desktop column becomes macOS.** iOS, Android, Web and
  TUI are marked deferred. No MUST is silently dropped: a deferred client has no
  MUSTs to regress.
* **`docs/07-clients/tui.md` is deleted** rather than left as an accepted spec
  for a client that does not exist. Its two durable parts — the capture
  subcommands and the recurrence phrasing — are documented where they now live.
* **`sunrise-core` stays reachable without a GUI**, through `sunrise-cli`. That
  is a standing requirement, not a transitional one: the day the CLI stops being
  able to drive the whole stack is the day the core's tests stop describing a
  usable system.

## What would force revisiting this

1. **iOS shipping.** The spike proved macOS only. An iOS slice needs its own
   spike before it is planned, not after.

   **Amended:** this happened. `apps/apple` now builds a second product,
   `SunriseiOS`, from the same `Sunrise/` sources plus an `iOS/` directory for
   the surfaces a phone has and a Mac does not; CI runs it as its own `ios-app`
   job, compiling the shared `SunriseTests/` suite against the iOS product a
   second time and running `SunriseiOSUITests` on the simulator. The seam held — the iOS app links the same
   `sunrise-core-bindings` xcframework — which is the thing the spike existed to
   establish. **It does not make iOS a v1 client:** the parity MUSTs are still
   macOS's, and no MUST has been transferred. What is settled is that a second
   Apple platform costs UI work and not a second core, which is what would have
   forced revisiting this ADR had it gone the other way.
2. **A second desktop platform becoming a requirement.** SwiftUI does not go
   there, and that is the point at which a cross-platform toolkit is worth
   re-costing — with the shared core intact either way.
3. **The CLI ceasing to be enough for headless use.** If capture-and-review over
   SSH stops covering the researcher persona, the answer is a better CLI, not a
   second interactive client.
