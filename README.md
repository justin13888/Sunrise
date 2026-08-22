# Sunrise

Sunrise is an open-source daily routine app that helps you focus on what matters. It aims to be accessible, available on all major desktop and mobile platforms, open source, and built with performant technologies.

> **Status:** Sunrise is undergoing a v1 rewrite. The architecture below is in active development.

<!-- TODO: Add screenshot and demo link -->

## Why Sunrise?

Everybody has their own way to stay organized — Sunrise gives you simple, well-thought-out tools, for free. Self-host to keep control of your data, contribute to the open-source codebase to add features, and give feedback to help everyone else.

## Features

- **Local-first & end-to-end encrypted**: A deterministic Rust core owns your data; it never leaves your devices unencrypted.
- **CRDT-based sync**: Edit offline on any device and merge without conflicts.
- **Self-hostable sync relay**: Run your own server (REST + WebSocket, OIDC, SQLite) to keep your data yours.
- **Terminal-first**: a full-featured TUI is the shipping client for v1, with non-interactive subcommands for scripting and automation. A web PWA shell exists but runs on a stub core (see ADR-0012); mobile bindings are planned.
- **Routines with recurrence**: DST-aware RRULE-based scheduling and deterministic cross-device routine generation.
- **Calendar integrations**: Google Calendar and iCalendar.

## Architecture

Sunrise is split into a shared, deterministic **Rust core** and thin **client apps**. The core is isolated so it can be unit-tested deterministically in isolation; clients stay focused on presentation.

- **Rust core** (`crates/`): a Cargo workspace covering crypto, sync, storage, the sync relay server, and the TUI.
- **Clients**: the terminal client (`crates/sunrise-tui`) is the shipping client for v1. `apps/` holds a Bun workspace for the web PWA and shared UI tokens.

### Project structure

```
crates/        Rust workspace — the shared core and server
  sunrise-core            Single-writer vault: command/query + sync state
  sunrise-crypto          Frozen v1 crypto suite (keys, envelopes, recovery, pairing)
  sunrise-sync            Sync session states, backoff, transport trait + WebSocket client
  sunrise-wire-protocol   Sync wire protocol: frames, codecs, negotiation
  sunrise-storage         SQLite + SQLCipher (op log, blob store, FTS5)
  sunrise-server          Self-host sync relay (REST + WebSocket, OIDC)
  sunrise-domain          Domain entities, validation, RRULE, routine generation
  sunrise-integrations    Google Calendar + iCalendar
  sunrise-tui             Terminal UI (Ratatui)
  sunrise-core-bindings   UniFFI facade for iOS/Android
  …and supporting crates (cbor, id, error, log, onboarding, pairing, e2e)
apps/
  web/         Web PWA (React + Vite)
packages/
  sunrise-ui/  Shared UI tokens and components
schemas/       Versioned JSON schemas
docs/          Design source of truth: product, architecture, domain, crypto, sync, ADRs + implementation notes
```

## Development

### Prerequisites

- [Bun](https://bun.sh) — JS/TS package manager and runtime
- [Rust](https://rustup.rs) — toolchain version is pinned in `rust-toolchain.toml`
- [just](https://github.com/casey/just) — command runner; all project tasks live in the `justfile`
- [lefthook](https://github.com/evilmartians/lefthook) — git hooks manager

### Getting started

```bash
just setup    # install JS dependencies (bun install) and git hooks (lefthook install)
```

Then confirm the toolchain is wired up by running the full automated gate (see [End-to-end QA](#end-to-end-qa) for the complete walkthrough):

```bash
just validate     # JS/TS: Biome CI + typecheck + coverage
just rust-test    # Rust: unit tests + cross-crate end-to-end tests
```

### Try it in 30 seconds

The terminal client ships non-interactive subcommands, so you can drive a real
vault without launching the UI:

```bash
export SUNRISE_VAULT=$(mktemp -d)
cargo run -q -p sunrise-tui -- capture 'Renew passport #inbox ^+6h !1 ~1h'
cargo run -q -p sunrise-tui -- capture 'Email Sara about Q3'
cargo run -q -p sunrise-tui -- today
cargo run -q -p sunrise-tui -- inbox
cargo run -q -p sunrise-tui -- next               # the planner's ranked picks
cargo run -q -p sunrise-tui -- search passport
cargo run -q -p sunrise-tui -- review             # this week, folded
cargo run -q -p sunrise-tui -- export trends json # to stdout, for jq
cargo run -q -p sunrise-tui             # ...then the interactive TUI
```

Capture syntax is `#stream @context ^when !priority ~duration *due:when*`.
Anything the parser cannot resolve is reported on stderr and left in the title,
so no input is ever silently dropped. `^when` takes `today`, `tonight`,
`tomorrow`, weekday names (optionally `next friday`), `YYYY-MM-DD`, `+3d` /
`+2w` / `+6h` / `+90m`, and an optional trailing time (`9am`, `14:30`).

Inside the TUI: `1`–`7` (or `gt` / `gi` / `gs` / `g/` / `gf` / `gr` / `gv`)
switch views; `c` capture, `A` annotate (`!1 %high ~30m @ctx`), `e` edit,
`d` defer, `s` schedule, `D` delete, `m` move to stream, `Space` mark,
`b` link blockers, `x` complete, `u` undo, `L` activity, `/` search,
`:` command mode, and `?` shows every binding and command.

### End-to-end QA

This is the exact human test script to exercise every surface of the codebase, top to bottom. The automated suites are the source of truth for correctness; the manual app runs are for visual/interaction QA. Run each command from the repo root.

> **Maturity note (v1 rewrite):** the Rust **core**, **sync relay server**, and **TUI** run for real today. Cross-device sync is proven end to end by the `sunrise-e2e` convergence tests, including a paired-device test that transfers the vault root over a Noise handshake rather than sharing a key literal. The **web** client backs onto a `localStorage` stub — the real WASM `sunrise-core` build is deferred by decision, see [ADR-0012](docs/11-adr/0012-web-wasm-deferred.md). The Tauri **desktop** shell was removed: it never ran, and keeping a client that does not work is worse than not claiming one.

#### 1. Toolchain check

Confirm the toolchains are present. The exact Rust version is pinned in `rust-toolchain.toml`; `rustup` will honor it automatically inside the repo.

```bash
cargo --version     # Rust toolchain (pinned via rust-toolchain.toml)
bun --version       # JS/TS runtime + package manager
just --version      # command runner (all tasks live in the justfile)

just setup          # one-time: bun install + lefthook install (git hooks)
```

#### 2. Automated gates (the source of truth)

Run the full JS/TS gate, the full Rust suite, and the lint/format checks. A green run here is the "is everything correct?" answer.

```bash
bun run validate                          # Biome CI + typecheck + Vitest coverage
cargo test --workspace --all-targets      # entire Rust workspace — 411 tests pass
cargo clippy --workspace --all-targets -- -D warnings   # pedantic-clean
cargo fmt --check                         # formatting clean
```

`just validate && just rust-test` is the same pass wrapped in `just` recipes (what `just pre-push` mirrors for the git hook). The `sunrise-e2e` crate is the cross-crate release-gate proof: it boots the server binary and hits `/health`, `/meta`, `/metrics`, and `/api/v1/accounts`, runs two independent `Core` vaults side by side to prove vault-lock isolation, and — in `two_core_relay_convergence` — drives two synced `Core`s through the relay to prove live convergence, offline catch-up, LWW conflict resolution, and routine dedup.

#### 3. Live sync demo (server + two clients)

The TUI wires live sync through three optional env vars: `SUNRISE_SYNC_URL` starts the WebSocket sync driver, and `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` perform the dev two-file device-cert exchange (both instances share the fixed dev vault root, so stream keys derive identically). All three unset = fully offline (`sync: off` in the status line).

```bash
# Terminal 0 — run the self-host relay:
cargo run -p sunrise-server
# → "sunrise-server listening on 127.0.0.1:8443" (plain HTTP, in-memory store)

# Terminal 1 — instance A (exports its cert, trusts B's):
SUNRISE_VAULT=/tmp/vault-a \
SUNRISE_SYNC_URL=ws://127.0.0.1:8443/sync \
SUNRISE_EXPORT_CERT_FILE=/tmp/a.cert \
SUNRISE_TRUST_CERT_FILE=/tmp/b.cert \
cargo run -p sunrise-tui

# Terminal 2 — instance B (exports its cert, trusts A's):
SUNRISE_VAULT=/tmp/vault-b \
SUNRISE_SYNC_URL=ws://127.0.0.1:8443/sync \
SUNRISE_EXPORT_CERT_FILE=/tmp/b.cert \
SUNRISE_TRUST_CERT_FILE=/tmp/a.cert \
cargo run -p sunrise-tui
```

Cert trust is a two-sided file exchange: the **first** launch of each instance only exports its cert (the peer's file doesn't exist yet); **restart both** so each picks up the peer cert and submits `TrustDevice`. Then capture a task (`c`) in one instance and watch it appear in the other; the status line shows `sync: live` (green) with the pending count. The same flow is proven headlessly by `cargo test -p sunrise-tui --test live_sync` and, more thoroughly (offline catch-up, LWW conflicts, routine dedup), by:

```bash
cargo test -p sunrise-e2e --test two_core_relay_convergence -- --nocapture
```

> The server config is default-only (ephemeral, in-memory). The `-c sunrise.toml` flag in the binary's docstring is not wired up yet, so flags/config files have no effect.

#### 4. Terminal client — TUI feature tour (manual E2E)

The TUI opens a real encrypted vault and is the quickest way to exercise the core command/query loop by hand. Use a throwaway vault so QA never touches real data:

```bash
SUNRISE_VAULT=$(mktemp -d) cargo run -p sunrise-tui
```

It opens (creating if needed) the vault at `$SUNRISE_VAULT` (default `~/.sunrise/vault`), unlocked with a fixed single-user dev key.

**Views** (`1`–`7`, or the `g` chords):

| Key | View | What it is |
| --- | ---- | ---------- |
| `1` / `gt` | Today | Overdue / Due today / Scheduled / Upcoming / Anytime, grouped |
| `2` / `gi` | Inbox | Untriaged captures; `t` runs a one-at-a-time triage pass |
| `3` / `gs` | Browse | Sidebar of **Streams** and **Contexts**; tasks of whichever is open |
| `4` / `g/` | Search | FTS5 over titles and bodies, re-run on every keystroke |
| `5` / `gf` | Focus | The ranked planner, then the running session |
| `6` / `gr` | Routines | Recurring templates, with cadence, next occurrence and streak |
| `7` / `gv` | Review | Weekly review, daily glance, 12-week trends, saved snapshots |

**Selection and movement**

| Input | Action |
| ----- | ------ |
| `j`/`k`, `↑`/`↓` | Move the cursor |
| `gg` / `G`, `Home` / `End` | First / last row |
| `PgUp`/`PgDn`, `^F`/`^B`, `^D`/`^U` | Page and half-page |
| `Tab` | Cycle panes (Browse) or panels (Review) |
| `h` / `l` | Sidebar / tasks (Browse) |
| `Space` | Mark a row — a non-contiguous multi-selection |
| `V` then `j`/`k` | Visual range |
| `^Space` / `Esc` | Clear the marks |

**Acting on tasks** — every operator applies to the marks, else the visual run, else the cursor.

| Input | Action |
| ----- | ------ |
| `c` | Capture (`#stream @context ^when !1-5 ~30m *due:when*`), with a live preview |
| `A` | Annotate: `!1 %high ~45m @home #travel due:friday`; `-` clears a facet |
| `e` / `E` | Edit the title / the note body in `$EDITOR` |
| `x` | Toggle done (re-opens a completed task) |
| `d` / `s` | Defer / schedule (prompts) |
| `m` / `D` | Move to a stream / delete (confirms) |
| `b` / `B` | Make the marked tasks block this one / clear its blockers |
| `L` | Activity feed — what actually happened to this task or stream |
| `u` / `^R` | Undo / redo |
| `F` | Start a focus session on the current pick |

**Managing the vault** — in the Browse sidebar, `e` renames the row, `D` deletes it, `a` archives it and `p` pauses a stream. `S` creates a stream, `C` a context, `R` a routine (`water the plants #home ~10m | every 2 days`). In the Routines view `e` edits the cadence, `E` renames, `s` skips the next occurrence.

**Command mode** (`:`; Tab completes, `↑` recalls):

| Command | Action |
| ------- | ------ |
| `:q` | Quit |
| `:view <name>` | Switch view by name or number |
| `:capture <text>` | Capture without leaving the current view |
| `:filter @ctx…` | Narrow every list to those contexts (bare `:filter` clears) |
| `:focus plan` / `stats` / `energy <l\|m\|h>` / `length <p\|e\|u>` | Planner and calibration |
| `:export <trends\|activity\|focus\|streaks> [json\|csv] [path]` | Write a stats dataset |
| `:save <name>` / `:go <name>` / `:views` | Save this view, query and filter; recall it; list them |
| `:open <tsk_…>` / `:devices` / `:preview <path>` | Jump to a task / list devices / show an image |
| `:help` | The full key and command reference (`?` opens the same overlay) |

QA flow: capture a few tasks (`c`), annotate one (`A !1 %high ~30m`), mark two with `Space` and defer them together (`d`), complete one (`x`) and undo it (`u`), tour the views, build a dependency (`Space` on a blocker, then `b` on its dependent) and check the planner hides the blocked task (`5`), run a search (`/`), open the Review view (`7`) and save a snapshot (`Enter`), then quit and re-launch with the same `SUNRISE_VAULT` to confirm data persisted.

**Mouse** is off by default because capturing it steals the terminal's own text selection; `SUNRISE_MOUSE=1 cargo run -p sunrise-tui` turns it on. The wheel scrolls and a click moves the cursor — clicks never mutate.

**Image preview:** the `images` feature is on by default, so `:preview <path>` (from the Focus view) renders a PNG or JPEG inline — using the terminal's graphics protocol where available, halfblocks otherwise. Point it at any sample image, e.g. `:preview ~/Pictures/sample.png`. To build without image support: `cargo run -p sunrise-tui --no-default-features`.

#### 5. Benchmarks (manual)

Populate this platform's performance baselines. `bench/baseline.json` already carries `linux-x86_64` numbers; `bench-baseline` runs the criterion suite (submit / query_today@10k / fts@10k / ws-handshake) and merges the results back for your platform.

```bash
just bench            # run the criterion suite only
just bench-baseline   # run benches, then update bench/baseline.json for this platform
```

#### 6. Chaos suite (manual)

Four fault-injection scenarios — heavy drop, corruption, delay, and partition — each proving the cores reconverge after the transport heals:

```bash
cargo test -p sunrise-e2e --test chaos -- --nocapture
```

#### 7. Web PWA (manual E2E)

```bash
bun run --filter @sunrise/web dev     # dev server at http://localhost:5174
bun run --filter @sunrise/web build   # production build
bun run --filter @sunrise/web preview # serve the production build
```

> **Stub caveat (by decision — [ADR-0012](docs/11-adr/0012-web-wasm-deferred.md)):** the web Core is a `localStorage`-backed stub (`apps/web/src/wasm.ts`) mirroring the real Core's surface behind a `loadCore()` seam. The WASM `sunrise-core` build is deferred on an MSRV blocker. Use the web app for UI/PWA-shell QA only — it does **not** exercise real persistence, CRDT, or crypto. Data lives in browser storage; clear it via DevTools to reset.

### Common tasks

All project commands are centralized in the [`justfile`](justfile). Run `just` (or `just --list`) to see everything:

| Command              | Description                                          |
| -------------------- | ---------------------------------------------------- |
| `just check`         | Lint & format check, no writes (Biome)               |
| `just fix`           | Lint & format with autofix (Biome)                   |
| `just ci`            | Strict CI lint check, no writes (Biome)              |
| `just typecheck`     | Type-check every JS/TS workspace package             |
| `just test`          | Run the JS/TS test suite once                        |
| `just test-coverage` | Run the JS/TS test suite with coverage               |
| `just rust-fmt`      | Format Rust code in place                            |
| `just rust-clippy`   | Lint Rust with Clippy (warnings denied)              |
| `just rust-check`    | Type-check the Rust workspace                        |
| `just rust-test`     | Run the Rust test suite                              |
| `just validate`      | Full local validation: Biome CI + typecheck + coverage |

### Git hooks

Git hooks are managed by [lefthook](https://github.com/evilmartians/lefthook) and defined in `lefthook.yaml`, which simply calls `just` recipes so there is a single source of truth:

- **pre-commit** — `just fix`, `just typecheck`, and (for staged `*.rs` files) `just rust-fmt-check` + `just rust-clippy`. Run the lot with `just pre-commit`.
- **pre-push** — `just ci`, `just typecheck`, `just test`, and `just rust-test`. Run the lot with `just pre-push`.

## License

Sunrise is licensed under the [AGPL-3.0 License](LICENSE).
