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
spec/          Product, architecture, domain, and protocol specs
docs/          Implementation notes
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
