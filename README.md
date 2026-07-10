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
- **Cross-platform clients**: Desktop (Tauri), web (PWA), and terminal (TUI), with mobile bindings via UniFFI.
- **Routines with recurrence**: RRULE-based scheduling and routine generation.
- **Calendar integrations**: Google Calendar and iCalendar.

## Architecture

Sunrise is split into a shared, deterministic **Rust core** and thin **client apps**. The core is isolated so it can be unit-tested deterministically in isolation; clients stay focused on presentation.

- **Rust core** (`crates/`): a Cargo workspace covering crypto, CRDT, sync, storage, the sync relay server, and the TUI.
- **Clients** (`apps/`, `packages/`): a Bun workspace for the Tauri desktop app, the web PWA, and shared UI.

### Project structure

```
crates/        Rust workspace — the shared core and server
  sunrise-core            Single-writer vault: command/query + sync state
  sunrise-crypto          Frozen v1 crypto suite (keys, envelopes, recovery, pairing)
  sunrise-crdt            Loro-backed CRDT layer
  sunrise-sync            Sync state machine, cursors, outbox, transport
  sunrise-wire-protocol   Sync wire protocol: frames, codecs, negotiation
  sunrise-storage         SQLite + SQLCipher (op log, blob store, FTS5)
  sunrise-server          Self-host sync relay (REST + WebSocket, OIDC)
  sunrise-domain          Domain entities, validation, RRULE, routine generation
  sunrise-integrations    Google Calendar + iCalendar
  sunrise-tui             Terminal UI (Ratatui)
  sunrise-core-bindings   UniFFI facade for iOS/Android
  …and supporting crates (cbor, id, error, log, onboarding, pairing, e2e)
apps/
  desktop/     Tauri 2 + React desktop app
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

### End-to-end QA

This is the exact path to exercise every surface of the codebase. Work top to bottom: the automated suites are the source of truth for correctness; the manual app runs are for visual/interaction QA. Run each Rust command from the repo root.

> **Maturity note (v1 rewrite):** the Rust core, sync relay server, and TUI run for real today. The **web** client backs onto a `localStorage` stub (the real WASM `sunrise-core` build is deferred), and the **desktop** Tauri shell is frontend-only until its native deps are pinned. Caveats are called out per surface below so QA results aren't misread.

#### 1. Automated test suites (the source of truth)

```bash
just test            # JS/TS suite (Vitest), run once
just test-coverage   # …with coverage report
bun run test:ui      # …interactive Vitest UI in the browser

just rust-test       # entire Rust workspace: unit + integration + e2e tests
cargo test -p sunrise-e2e   # just the cross-crate end-to-end tests
```

The `sunrise-e2e` crate is the release-gate proof that crates work together: it boots the server binary and hits `/health`, `/meta`, `/metrics`, and `/api/v1/accounts`, and runs two independent `Core` vaults side by side to prove vault-lock isolation. (Cross-device sync through the relay is not wired end-to-end yet.)

For a single "is everything green?" pass, run `just validate && just rust-test` (this is also what `just pre-push` mirrors for the git hook).

#### 2. Sync relay server (manual E2E)

Run the self-host server in its own terminal:

```bash
cargo run -p sunrise-server
# → "sunrise-server listening on 127.0.0.1:8443"
```

It serves **plain HTTP** on `127.0.0.1:8443` with an in-memory store by default. Probe it from another terminal:

```bash
curl http://127.0.0.1:8443/health
curl http://127.0.0.1:8443/meta
curl http://127.0.0.1:8443/metrics
curl http://127.0.0.1:8443/api/v1/accounts
```

> Configuration is currently default-only (ephemeral, in-memory). The `-c sunrise.toml` flag shown in the binary's docstring is not wired up yet, so flags/config files have no effect.

#### 3. Terminal client — TUI (manual E2E)

The TUI opens a real encrypted vault and is the quickest way to exercise the core command/query loop by hand:

```bash
# Use a throwaway vault so QA never touches your real data:
SUNRISE_VAULT=$(mktemp -d) cargo run -p sunrise-tui
```

It opens (creating if needed) the vault at `$SUNRISE_VAULT`, defaulting to `~/.sunrise/vault`, unlocked with a fixed single-user dev key. Keybindings:

| Key                 | Action                                            |
| ------------------- | ------------------------------------------------- |
| `1`–`5`             | Switch view: Today, Inbox, Stream, Search, Focus  |
| `↑`/`↓` (or `j`/`k`)| Move selection                                    |
| `c`                 | Capture a task — type a title, `Enter` to save    |
| `x` / `Space`       | Toggle the selected task complete                 |
| `/`                 | Search — type a query, `Enter` to commit          |
| `q` / `Esc`         | Quit                                              |

QA flow: capture a few tasks (`c`), complete one (`x`), switch views (`1`–`5`), search (`/`), then quit and re-launch with the same `SUNRISE_VAULT` to confirm the data persisted.

#### 4. Web PWA (manual E2E)

```bash
bun run --filter @sunrise/web dev     # dev server at http://localhost:5174
bun run --filter @sunrise/web build   # production build
bun run --filter @sunrise/web preview # serve the production build
```

> **Stub caveat:** the web Core is a `localStorage`-backed stub mirroring the real Core's surface (`queryToday`, `queryInbox`, `createTask`, `completeTask`). Use it for UI/PWA-shell QA only — it does **not** exercise real persistence, CRDT, or crypto. Data lives in browser storage; clear it via DevTools to reset.

#### 5. Desktop app — Tauri (manual E2E)

```bash
bun run --filter @sunrise/desktop dev   # frontend renderer only, http://localhost:5173
```

> **Deferred caveat:** the native Tauri shell is not yet runnable — its Tauri deps aren't pinned, and IPC falls back to a stub. The frontend renders, but `cargo tauri dev` won't drive a real window until the deps are installed (`cd apps/desktop && bun install && bun run tauri dev` once configured).

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
