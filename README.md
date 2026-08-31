# Sunrise

Sunrise is an open-source daily routine app that helps you focus on what matters. It aims to be accessible, available on all major desktop and mobile platforms, open source, and built with performant technologies.

> **Status:** Sunrise is undergoing a v1 rewrite. The architecture below is in active development.

<!-- TODO: Add screenshot and demo link -->

## Why Sunrise?

Everybody has their own way to stay organized — Sunrise gives you simple, well-thought-out tools, for free. Self-host to keep control of your data, contribute to the open-source codebase to add features, and give feedback to help everyone else.

## Features

- **Local-first & end-to-end encrypted**: A deterministic Rust core owns your data; it never leaves your devices unencrypted.
- **Offline-first sync that converges**: Every write commits locally first and syncs as an encrypted op. Concurrent edits are resolved by entity-level last-writer-wins ordered by a **hybrid logical clock**, so a device with a skewed wall clock cannot win every conflict ([ADR-0014](docs/11-adr/0014-entity-level-lww-merge.md), [ADR-0016](docs/11-adr/0016-hlc-timestamps.md)).
- **Self-hostable sync relay**: Run your own server (REST + WebSocket, OIDC, SQLite) to keep your data yours. The relay only ever sees ciphertext.
- **Scriptable**: `sunrise` is a one-shot CLI — capture, edit, defer, triage, review, export and sync from a shell, a cron job, or over SSH. Each vault is a separate account with its own key, so one machine can hold several. The graphical client is a native SwiftUI macOS app ([ADR-0019](docs/11-adr/0019-swiftui-macos-client.md)); iOS, Android and Web are deferred.
- **Routines with recurrence**: DST-aware RRULE-based scheduling and deterministic cross-device routine generation, driven by plain English (`every 2 weeks on tue`, `weekdays`, `monthly on the last day`).
- **Calendar interchange**: import and export `.ics` (RFC 5545) from either client — `sunrise ical import` / `export`, or File → Import Calendar… (⌘⇧I) and Export Calendar ▸ Today | This Week on macOS — so time blocks move in and out of any calendar app. Imports are idempotent: re-importing the same file updates the blocks it already made rather than duplicating them. Anything the subset does not model is **reported, never dropped silently**. A Google Calendar provider is implemented and tested but is **not wired into v1** ([ADR-0020](docs/11-adr/0020-v1-must-demotions.md), [#4](https://github.com/justin13888/Sunrise/issues/4)).

## Architecture

Sunrise is split into a shared, deterministic **Rust core** and thin **client apps**. The core is isolated so it can be unit-tested deterministically in isolation; clients stay focused on presentation.

- **Rust core** (`crates/`): a Cargo workspace of 21 crates covering domain, crypto, sync, storage, the sync relay server, the CLI, and the FFI seam. CI fails if any crate is unreachable from a shipping binary.
- **Clients**: two ship in v1 — the `sunrise` CLI, and a native SwiftUI **macOS app** (`apps/macos`) that links the core through UniFFI (`crates/sunrise-core-bindings`) and is built, linted and tested in CI. `apps/web` is a deferred PWA stub, and `packages/` holds shared UI tokens for it.

### Project structure

```
crates/        Rust workspace — the shared core, the server, the clients' core
  sunrise-core            Single-writer vault: command/query + sync state
  sunrise-crypto          Frozen v1 crypto suite (keys, envelopes, recovery, pairing)
  sunrise-sync            Sync session states, backoff, transport trait + WebSocket client
  sunrise-wire-protocol   Sync wire protocol: frames, codecs, negotiation
  sunrise-storage         SQLite + SQLCipher (op log, blob store, FTS5)
  sunrise-server          Self-host sync relay (REST + WebSocket, OIDC)
  sunrise-domain          Entities, validation, RRULE, routine generation, the
                          capture/annotate grammars, and the shared phrasing
  sunrise-client-core     Client-side but UI-free: undo/redo, saved views
  sunrise-cli             The `sunrise` command-line client
  sunrise-core-bindings   UniFFI seam — Swift today, Kotlin later
  sunrise-auth            Client-side OIDC relying party (PKCE, token storage)
  sunrise-pairing         Noise XX device pairing + SAS confirmation
  sunrise-integrations    iCalendar (live) + Google Calendar (deferred, see #4)
  …and supporting crates (cbor, id, error, log, onboarding, bench, e2e)
tools/
  uniffi-bindgen/  Binding generator, deliberately outside the workspace
apps/
  macos/       Native SwiftUI client over the UniFFI seam — see ADR-0019
  web/         Web PWA (React + Vite) — deferred, see ADR-0012
packages/
  sunrise-ui/  Shared UI tokens, consumed only by the deferred web app
schemas/       Versioned JSON schemas
docs/          Design source of truth: product, architecture, domain, crypto, sync, ADRs + implementation notes
```

## Development

### Prerequisites

- [Bun](https://bun.sh) — JS/TS package manager and runtime
- [Rust](https://rustup.rs) — toolchain version is pinned in `rust-toolchain.toml`
- [just](https://github.com/casey/just) — command runner; all project tasks live in the `justfile`
- [lefthook](https://github.com/evilmartians/lefthook) — git hooks manager

Building the macOS client additionally needs Xcode and [XcodeGen](https://github.com/yonaskolb/XcodeGen).

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

`sunrise` drives a real encrypted vault from the shell — no GUI, no daemon:

```bash
export SUNRISE_VAULT=$(mktemp -d)
cargo run -q -p sunrise-cli -- capture 'Renew passport #inbox ^+6h !1 ~1h'
cargo run -q -p sunrise-cli -- capture 'Email Sara about Q3'
cargo run -q -p sunrise-cli -- today
cargo run -q -p sunrise-cli -- inbox
cargo run -q -p sunrise-cli -- next               # the planner's ranked picks
cargo run -q -p sunrise-cli -- search passport
cargo run -q -p sunrise-cli -- review             # this week, folded
cargo run -q -p sunrise-cli -- export trends json # to stdout, for jq

# triage what is already captured:
cargo run -q -p sunrise-cli -- edit <id> '!1 @home ~45m'   # re-facet a task
cargo run -q -p sunrise-cli -- defer <id> tomorrow         # bumps deferred_count
cargo run -q -p sunrise-cli -- drop <id>                   # soft delete
cargo run -q -p sunrise-cli -- stream errands              # the tasks in one stream
cargo run -q -p sunrise-cli -- context home                # …and in one context
cargo run -q -p sunrise-cli -- streams move errands last   # syncs; writes sort_order
cargo run -q -p sunrise-cli -- vaults                      # accounts on this machine

# calendar interchange (RFC 5545), both directions:
cargo run -q -p sunrise-cli -- ical import meetings.ics   # or `-` for stdin
cargo run -q -p sunrise-cli -- ical export week out.ics   # or omit the path for stdout
```

`edit` takes the same token grammar as `capture` applied to a task that already
exists, with one deliberate difference: a bare word is refused rather than read
as a new title, and **one bad token rejects the whole line** — a script that
mistyped one token is better served by a non-zero exit than by four of its five
changes landing. Re-titling a task is a macOS-only operation for now.

`ical import` is idempotent: a Block's id is derived from `(source, uid)`, so
re-importing the same file updates the blocks it already made instead of
minting duplicates. Anything the RFC 5545 subset does not model — `VTODO`,
`VALARM`, `VTIMEZONE`, `RRULE`, and the rest — is **reported on stderr**, never
dropped silently.

Capture syntax is `#stream @context ^when !priority ~duration *due:when*`.
Anything the parser cannot resolve is reported on stderr and left in the title,
so no input is ever silently dropped. `^when` takes `today`, `tonight`,
`tomorrow`, weekday names (optionally `next friday`), `YYYY-MM-DD`, `+3d` /
`+2w` / `+6h` / `+90m`, and an optional trailing time (`9am`, `14:30`).

Every surface parses that line with the same function
(`sunrise_domain::capture::parse`), so a task captured from a script and one
captured from the app are the same task.

### End-to-end QA

This is the exact human test script to exercise every surface of the codebase, top to bottom. The automated suites are the source of truth for correctness; the manual runs are for visual/interaction QA. Run each command from the repo root.

> **Maturity note (v1 rewrite):** the Rust **core**, the **sync relay server**, and the **CLI** run for real today. Cross-device sync is proven end to end by the `sunrise-e2e` convergence tests, including a paired-device test that transfers the vault root over a Noise handshake rather than sharing a key literal. The **macOS** app is a real client — tasks, calendar, focus, routines, review, notes, search, attachments, pairing, multi-vault, reminders, App Intents, drag-and-drop, iCal import/export, print and PDF export, and full keyboard navigation — built, SwiftLint-`--strict`ed and tested in CI on `macos-26`. Its status against every v1 requirement is tracked capability by capability in [`docs/07-clients/parity-matrix.md`](docs/07-clients/parity-matrix.md#v1-status-audit), where **every MUST in both shipping columns is now met** — read the "what is still narrow" notes there rather than the verdict column alone. The **web** client backs onto a `localStorage` stub — the real WASM `sunrise-core` build is deferred by decision, see [ADR-0012](docs/11-adr/0012-web-wasm-deferred.md). The Tauri **desktop** shell and the Ratatui **TUI** were both removed; see [ADR-0019](docs/11-adr/0019-swiftui-macos-client.md).

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
cargo test --workspace --all-targets      # entire Rust workspace
cargo clippy --workspace --all-targets -- -D warnings   # pedantic-clean
cargo fmt --check                         # formatting clean
cargo deny check                          # advisories, bans, licences, sources
```

`just validate && just rust-test` is the same pass wrapped in `just` recipes (what `just pre-push` mirrors for the git hook). The `sunrise-e2e` crate is the cross-crate release-gate proof: it boots the server binary and hits `/health`, `/meta`, `/metrics`, and `/api/v1/accounts`, runs two independent `Core` vaults side by side to prove vault-lock isolation, and — in `two_core_relay_convergence` — drives two synced `Core`s through the relay to prove live convergence, offline catch-up, LWW conflict resolution, and routine dedup.

#### 3. Headless, no client at all

Every layer is drivable with no UI. `cargo test -p sunrise-cli` runs the real
`sunrise` binary against a real vault in a separate process, and boots a real
relay in-process to watch two replicas converge.

```bash
cargo test -p sunrise-cli     # the whole stack, headless
cargo test -p sunrise-e2e     # relay convergence, pairing, four chaos scenarios
```

#### 4. Live sync demo (server + two vaults)

`sunrise` wires live sync through four optional env vars: `SUNRISE_SYNC_URL` starts the WebSocket sync driver, `SUNRISE_EXPORT_CERT_FILE` / `SUNRISE_TRUST_CERT_FILE` perform the dev two-file device-cert exchange, and `SUNRISE_VAULT_ROOT` gives both instances the **same vault root**, so their stream keys derive identically and each can decrypt the other's op envelopes. All four unset = fully offline, with each vault on its own key.

That last variable is what makes this a demo of two *devices* rather than two accounts. Every vault otherwise gets its own random root, minted on first open and kept in the keystore (`SUNRISE_KEYSTORE`, default `$XDG_DATA_HOME/sunrise/keys`) — `SUNRISE_VAULT` names separate accounts, not separate folders. Sharing a root is what pairing will do over the wire; until then it is spelled out, exactly like the cert exchange beside it.

```bash
# Terminal 0 — run the self-host relay:
cargo run -p sunrise-server
# → "sunrise-server listening on 127.0.0.1:8443" (plain HTTP, in-memory store)

# One account, two replicas: any 64 hex characters, the same in both terminals.
export SUNRISE_VAULT_ROOT=$(head -c32 /dev/urandom | xxd -p -c64)

# Terminal 1 — vault A exports its cert and trusts B's:
SUNRISE_VAULT=/tmp/vault-a \
SUNRISE_SYNC_URL=ws://127.0.0.1:8443/sync \
SUNRISE_EXPORT_CERT_FILE=/tmp/a.cert \
SUNRISE_TRUST_CERT_FILE=/tmp/b.cert \
cargo run -p sunrise-cli -- capture 'Written on A'

# Terminal 2 — vault B exports its cert and trusts A's:
SUNRISE_VAULT=/tmp/vault-b \
SUNRISE_SYNC_URL=ws://127.0.0.1:8443/sync \
SUNRISE_EXPORT_CERT_FILE=/tmp/b.cert \
SUNRISE_TRUST_CERT_FILE=/tmp/a.cert \
cargo run -p sunrise-cli -- today
```

> Upgrading from a build before per-vault keys? Every vault was written under one constant then, so this build refuses such a vault rather than guessing it — and the refusal quotes the old root, which opens it once so the work can be moved. `sunrise vaults` lists what this machine holds keys for.

Cert trust is a two-sided file exchange: the **first** run of each vault only exports its cert (the peer's file doesn't exist yet); **run both again** so each picks up the peer cert and submits `TrustDevice`. `sunrise sync --once` drains the outbox and exits, bounded — a scheduled job that hangs because the relay is down is worse than one that fails. The same flow is proven headlessly by `cargo test -p sunrise-cli --test live_sync` and, more thoroughly (offline catch-up, LWW conflicts, routine dedup), by:

```bash
cargo test -p sunrise-e2e --test two_core_relay_convergence -- --nocapture
```

Against a relay with an OIDC issuer configured, `sunrise login` obtains a bearer and stores it mode-0600 in the vault directory; `sunrise whoami` reports its state and `sunrise logout` forgets it. A stored login feeds sync automatically, and `SUNRISE_SYNC_TOKEN` overrides it for CI:

```bash
export SUNRISE_OIDC_ISSUER=https://auth.example.com
export SUNRISE_OIDC_CLIENT_ID=sunrise
cargo run -p sunrise-cli -- login     # opens a browser, waits on a loopback redirect
```

> The server reads `sunrise.toml` (`-c <path>` → `$SUNRISE_CONFIG` → `./sunrise.toml` → `/etc/sunrise/sunrise.toml`); with no config it runs on defaults, which bind loopback in single-tenant mode with an in-memory store. Setting `[auth] oidc_issuer` + `oidc_client_id` installs the JWKS verifier. See `docs/06-server/self-hosting.md`.

#### 5. macOS client

```bash
just macos-xcframework    # cargo build → uniffi-bindgen → lipo → SunriseCore.xcframework
just macos-app            # + xcodegen, swiftlint --strict, xcodebuild test
just macos-open           # open the generated project in Xcode
just macos-uitest         # the XCUITest target, which macos-app does not run
```

`just macos-app` is exactly what CI runs on `macos-26`. Note that the UI test
target is `skipped: true` in the scheme — macOS XCUITest needs
`sudo DevToolsSecurity -enable` — so `just macos-uitest` is the only thing that
drives the real window, and it runs on a developer machine only.

`macos-xcframework` builds the release slices, generates the Swift bindings from
the built library, and packages the framework the app links. The bindings generator lives
in `tools/uniffi-bindgen`, **outside** the Cargo workspace, with its own
lockfile pinning `cargo-platform` to 0.3.2 — UniFFI's default features pull a
version requiring rustc 1.91, which would break the workspace's 1.88 pin.

`out/` and `build/` are gitignored: the Swift is generated from the Rust on
every build, so committing it would let the two drift.

#### 6. Benchmarks (manual)

Populate this platform's performance baselines. `bench/baseline.json` already carries `linux-x86_64` numbers; `bench-baseline` runs the criterion suite (submit / query_today@10k / fts@10k / ws-handshake) and merges the results back for your platform.

```bash
just bench            # run the criterion suite only
just bench-baseline   # run benches, then update bench/baseline.json for this platform
```

#### 7. Chaos suite (manual)

Four fault-injection scenarios — heavy drop, corruption, delay, and partition — each proving the cores reconverge after the transport heals:

```bash
cargo test -p sunrise-e2e --test chaos -- --nocapture
```

#### 8. Web PWA (manual E2E)

```bash
bun run --filter @sunrise/web dev     # dev server at http://localhost:5174
bun run --filter @sunrise/web build   # production build
bun run --filter @sunrise/web preview # serve the production build
```

> **Stub caveat (by decision — [ADR-0012](docs/11-adr/0012-web-wasm-deferred.md)):** the web Core is a `localStorage`-backed stub (`apps/web/src/wasm.ts`) mirroring the real Core's surface behind a `loadCore()` seam. The WASM `sunrise-core` build is deferred on an MSRV blocker. Use the web app for UI/PWA-shell QA only — it does **not** exercise real persistence, merge, or crypto. Data lives in browser storage; clear it via DevTools to reset.

### Common tasks

All project commands are centralized in the [`justfile`](justfile). Run `just` (or `just --list`) to see everything:

| Command                  | Description                                            |
| ------------------------ | ------------------------------------------------------ |
| `just check`             | Lint & format check, no writes (Biome)                 |
| `just fix`               | Lint & format with autofix (Biome)                     |
| `just ci`                | Strict CI lint check, no writes (Biome)                |
| `just typecheck`         | Type-check every JS/TS workspace package               |
| `just test`              | Run the JS/TS test suite once                          |
| `just test-coverage`     | Run the JS/TS test suite with coverage                 |
| `just rust-fmt`          | Format Rust code in place                              |
| `just rust-clippy`       | Lint Rust with Clippy (warnings denied)                |
| `just rust-check`        | Type-check the Rust workspace                          |
| `just rust-test`         | Run the Rust test suite                                |
| `just orphan-crates`     | Fail if any crate is unreachable from a shipping binary |
| `just macos-xcframework` | Build the Swift bindings + `SunriseCore.xcframework`   |
| `just macos-app`         | Build, SwiftLint `--strict` and test the macOS app     |
| `just validate`          | Full local validation: Biome CI + typecheck + coverage |

### Git hooks

Git hooks are managed by [lefthook](https://github.com/evilmartians/lefthook) and defined in `lefthook.yaml`, which simply calls `just` recipes so there is a single source of truth:

- **pre-commit** — `just fix`, `just typecheck`, and (for staged `*.rs` files) `just rust-fmt-check` + `just rust-clippy`. Run the lot with `just pre-commit`.
- **pre-push** — `just ci`, `just typecheck`, `just test`, and `just rust-test`. Run the lot with `just pre-push`.

## License

Sunrise is licensed under the [AGPL-3.0 License](LICENSE).
