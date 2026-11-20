# Sunrise

**Languages:** English · [Translating Sunrise](#translating-sunrise)

Sunrise is an open-source daily routine app that helps you focus on what matters. It aims to be accessible, available on all major desktop and mobile platforms, open source, and built with performant technologies.

> **Status:** Sunrise is near ready for first alpha release since rewrite. Focused on Apple devices and UX refinement.

<!-- TODO: Add screenshot and demo link -->

## Why Sunrise?

Everybody has their own way to stay organized — Sunrise gives you simple, well-thought-out tools, for free. Self-host to keep control of your data, contribute to the open-source codebase to add features, and give feedback to help everyone else.

## Features

- **Local-first & end-to-end encrypted**: A deterministic Rust core owns your data; it never leaves your devices unencrypted.
- **Offline-first sync that converges**: Every write commits locally first and syncs as an encrypted op. Concurrent edits are resolved by entity-level last-writer-wins ordered by a **hybrid logical clock**, so a device with a skewed wall clock cannot win every conflict ([ADR-0014](docs/11-adr/0014-entity-level-lww-merge.md), [ADR-0016](docs/11-adr/0016-hlc-timestamps.md)).
- **Self-hostable sync relay**: Run your own server (REST + SSE sync, OIDC, SQLite) to keep your data yours. The relay only ever sees ciphertext.
- **Scriptable**: `sunrise` is a one-shot CLI — capture, edit, defer, triage, review, export and sync from a shell, a cron job, or over SSH. Each vault is a separate account with its own key, so one machine can hold several. The graphical clients are native SwiftUI apps over one shared view layer ([ADR-0019](docs/11-adr/0019-swiftui-macos-client.md)): **macOS**, and an **iOS/iPadOS** tab shell that builds, tests and runs its UI tests in CI on every push to `master` and nightly. Android and Web are deferred.
- **Routines with recurrence**: DST-aware RRULE-based scheduling and deterministic cross-device routine generation, driven by plain English (`every 2 weeks on tue`, `weekdays`, `monthly on the last day`).
- **Calendar interchange**: import and export `.ics` (RFC 5545) from either client — `sunrise ical import` / `export`, or File → Import Calendar… (⌘⇧I) and Export Calendar ▸ Today | This Week on macOS — so time blocks move in and out of any calendar app. Imports are idempotent: re-importing the same file updates the blocks it already made rather than duplicating them. Anything the subset does not model is **reported, never dropped silently**. A Google Calendar provider is implemented and tested but is **not wired into any client** ([ADR-0020](docs/11-adr/0020-v1-must-demotions.md), [#930](https://github.com/justin13888/Sunrise/issues/930)).

## Architecture

Sunrise is split into a shared, deterministic **Rust core** and thin **client apps**. The core is isolated so it can be unit-tested deterministically in isolation; clients stay focused on presentation.

- **Rust core** (`crates/`): a Cargo workspace of crates covering domain, crypto, sync, storage, the sync relay server, the CLI, and the FFI seam. CI fails if any crate is unreachable from a shipping binary.
- **Clients**: the `sunrise` CLI and the SwiftUI apps in `apps/apple`, which link the core through UniFFI (`crates/sunrise-core-bindings`). `apps/apple/Sunrise/` compiles into both products; `macOS/` and `iOS/` hold only the surfaces that do not cross — the menu bar, the global hotkey and the borderless capture panel on one side, the tab shell on the other. Both are built, linted `--strict` and tested in CI. Run either with `mise run macos-run` / `mise run ios-run`. `apps/web` is a PWA that runs the core, compiled to wasm (`crates/sunrise-core-wasm`, built by `mise run web-wasm`), in a worker over OPFS, with no sync yet, and falls back to a `localStorage` stub where the browser cannot host it ([ADR-0055](docs/11-adr/0055-web-wasm-core.md)). `packages/sunrise-ui-tokens` compiles the design tokens from TOML into CSS, TypeScript, Swift and Rust — both Apple targets compile the Swift one, so a stream's colour is now decided in one place for every client ([ADR-0029](docs/11-adr/0029-design-token-pipeline.md)).

### Project structure

```
crates/        Rust workspace — the shared core, the server, the clients' core
  sunrise-core            Single-writer vault: command/query + sync state
  sunrise-crypto          Frozen v1 crypto suite (keys, envelopes, recovery, pairing)
  sunrise-sync            Sync session states, backoff, transport trait + SSE/POST client
  sunrise-wire-protocol   Sync wire protocol: frames, codecs, negotiation
  sunrise-storage         SQLite + SQLCipher (op log, blob store, FTS5)
  sunrise-server          Self-host sync relay (REST + SSE sync, OIDC)
  sunrise-domain          Entities, validation, RRULE, routine generation, the
                          capture/annotate grammars, and the shared phrasing
  sunrise-client-core     The client layer every client links: undo/redo, saved
                          views, the edit gate, account bootstrap, recovery,
                          background sync
  sunrise-relay-client    The generated REST client for the relay
  sunrise-cli             The `sunrise` command-line client
  sunrise-core-bindings   UniFFI seam — Swift today, Kotlin later
  sunrise-auth            Client-side OIDC relying party (PKCE, token storage)
  sunrise-pairing         Noise XX device pairing + SAS confirmation
  sunrise-integrations    iCalendar (live) + Google Calendar (not wired; calendar read and write is #924)
  sunrise-core-wasm       The web client's wasm seam over sunrise-core, SQLite in OPFS (ADR-0055)
  sunrise-private-file    The one owner-only file write every secret on disk goes through,
                          and the blob chunk store the vault and the relay share
  sunrise-sqlcipher       The wait for SQLCipher to initialise before a process's first key
  sunrise-telemetry       Redacted OpenTelemetry tracing for the relay
  sunrise-http-sig        Per-request device signatures over canonical JSON
  sunrise-test-seed       The one SUNRISE_FUZZ_SEED every randomised harness resolves
  sunrise-crypto-test-vectors  Frozen byte-exact vectors for the v1 crypto suite
  …and supporting crates (cbor, id, error, log, bench, e2e)
tools/
  uniffi-bindgen/  Binding generator, deliberately outside the workspace
  kotlin-smoke/    JVM smoke test of the generated Kotlin bindings
bench/         Performance baselines per platform (baseline.json)
deploy/        Shipped reverse-proxy configs (Caddy, nginx) and the test that boots them
fuzz/          Fuzz targets and seeds, outside the workspace
mutants/       Per-crate mutation-coverage floors (baseline.json)
tests/         Chaos harness notes and wire-protocol fixtures
apps/
  apple/       Native SwiftUI clients over the UniFFI seam — see ADR-0019
  web/         Web PWA (React + Vite) — local WASM core, no sync yet; Cloudflare Pages deploy wired, not enabled (#11)
  docs/        The documentation site: VitePress over docs/ — see ADR-0054
i18n/          The string catalog: en.toml, and a <locale>.toml per translation
packages/
  sunrise-ui-tokens/  TOML design tokens → CSS / TS / Swift / Rust — see ADR-0029
    generated/        `mise run tokens` writes it; `tokens-current` re-runs and diffs it
  sunrise-i18n/       i18n/*.toml → TS / Swift + String Catalog / Rust — see ADR-0054
    generated/        `mise run i18n` writes it; `i18n-current` re-runs and diffs it
  sunrise-ui/         Names the generated tokens for the web app
schemas/       Versioned JSON schemas, hand-written
  generated/   `mise run openapi` writes it; `openapi-current` re-runs and diffs it
docs/          Design source of truth: product, architecture, domain, crypto, sync, ADRs + implementation notes
```

A lowercase **`generated/`** directory is a convention rather than a label. It holds output that is
written by a generator, committed to the repository, and regenerated by a CI job that then runs
`git diff --exit-code` over it — the three above are the whole membership today. Nothing in one is ever
edited by hand, and `biome.jsonc` keys its formatter exemption off the directory name, so a new
generated artefact needs no config change: put it in a `generated/` directory and the formatter
leaves it alone from the commit that adds it.

Gitignored build output is *not* a member — `apps/apple/Generated/` is written by `mise run macos-app`
and never committed, so it has no drift gate to protect and no formatter reaches it either way.

## Development

### Prerequisites

- [Bun](https://bun.sh) — JS/TS package manager and runtime
- [Rust](https://rustup.rs) — toolchain version is pinned in `rust-toolchain.toml`
- [mise](https://mise.jdx.dev) — task runner; all project tasks live in `mise.toml`
- [lefthook](https://github.com/evilmartians/lefthook) — git hooks manager

Building the Apple clients additionally needs Xcode, [XcodeGen](https://github.com/yonaskolb/XcodeGen),
SwiftLint at the version `mise.toml`'s `[tools]` pins (`mise install` fetches
it on macOS, and `apple-preflight` refuses any other version), and the Rust
target for every slice the xcframework carries:

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
```

The host triple (`aarch64-apple-darwin`) is already installed; the two iOS ones
are not installed by default. Nothing else is needed — the core builds for iOS
with no manifest change.

### Getting started

```bash
mise install      # the Node (and, on macOS, the SwiftLint) that mise.toml pins
mise run setup    # install JS dependencies (bun install) and git hooks (lefthook install)
```

`mise install` comes first. `mise run test` runs vitest under the pinned Node, the
same one CI's Bun jobs install. With no Node on `PATH`, vitest falls back to Bun,
where `navigator.languages` is undefined, and the run fails a test that CI
passes.

Then confirm the toolchain is wired up by running the full automated gate (see [Manual QA](#manual-qa) for the complete walkthrough):

```bash
mise run validate     # JS/TS: Biome CI + typecheck + coverage
mise run rust-test    # Rust: unit tests + cross-crate end-to-end tests
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
changes landing. A new title goes through `sunrise retitle <id> <text>` instead.

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

### Manual QA

The automated gates above decide correctness. The manual walk through every
surface — headless, live sync, the Apple apps, benchmarks, chaos and the web
PWA — is [`docs/10-cross-cutting/testing.md`](docs/10-cross-cutting/testing.md#manual-end-to-end-qa).
Pairing two vaults and syncing them through a local relay is
[`docs/07-clients/overview.md`](docs/07-clients/overview.md#two-devices-on-one-machine).

### Apple clients

**To see an app on screen, use a `-run` task.** No other Apple task launches
anything:

```bash
mise run macos-run            # build the macOS app and open it
mise run ios-run              # build the iOS app and launch it on the simulator
```

Every Apple task, and what each one does *not* do, is in
[`docs/07-clients/mobile-ios.md`](docs/07-clients/mobile-ios.md#the-apple-tasks);
the macOS app's build and UI tests are in
[`docs/07-clients/desktop.md`](docs/07-clients/desktop.md#build).

CI runs the Apple jobs on a push to `master`, nightly, and on
`gh workflow run ci.yml --ref <branch>`, but **not on a pull request**, where
they report `skipped` (ADR-0028). So before merging a change that touches
anything the apps are built from — `crates/`, `tools/`, `schemas/`, any
`Cargo.toml`/`Cargo.lock`, a `.rs` file, `rust-toolchain*`, `.cargo/`,
`apps/apple/`, `mise.toml`, the generated `tokens.swift`, `Strings.swift` or
`Localizable.xcstrings`, or the CI workflow and actions — run
`mise run apple-app` on a Mac.

### Common tasks

All project commands are centralized in [`mise.toml`](mise.toml). Run `mise tasks` to see everything:

| Command                  | Description                                            |
| ------------------------ | ------------------------------------------------------ |
| `mise run check`             | Lint & format check, no writes (Biome)                 |
| `mise run fix`               | Lint & format with autofix (Biome)                     |
| `mise run ci`                | Strict CI lint check, no writes (Biome)                |
| `mise run typecheck`         | Type-check every JS/TS workspace package               |
| `mise run test`              | Run the JS/TS test suite once                          |
| `mise run test-coverage`     | Run the JS/TS test suite with coverage                 |
| `mise run rust-fmt`          | Format Rust code in place                              |
| `mise run rust-clippy`       | Lint Rust with Clippy (warnings denied), workspace and `fuzz/` |
| `mise run rust-check`        | Type-check the Rust workspace                          |
| `mise run rust-test`         | Run the Rust test suite                                |
| `mise run log-fields`        | Run the log-event/field gate, building first what it reads from |
| `mise run rust-test-changed` | Run the Rust tests the pushed changes can affect, by hand (the pre-push hook runs no tests) |
| `mise run test-changed`      | Run the JS/TS test suite when the pushed changes can affect it |
| `mise run rust-doc`          | Build the rustdoc for the workspace and `fuzz/`, every rustdoc warning denied |
| `mise run orphan-crates`     | Fail if any crate is unreachable from a shipping binary |
| `mise run swift-logic`       | Fail on non-view logic in Swift under `apps/apple` that is not on the shrinking allowlist |
| `mise run docs-links`        | Fail on a markdown link or heading anchor that resolves to nothing |
| `mise run doc-comments`      | Fail on a doc comment mangled into a shape rustdoc misreads |
| `mise run observability-catalog` | Fail if observability.md's extracted event or metric block has drifted |
| `mise run proptest-persistence`  | Fail if a `tests/` proptest does not set `failure_persistence` |
| `mise run apple-xcframework` | Build the Swift bindings + `SunriseCore.xcframework`   |
| `mise run macos-run`         | Build the macOS app and **open it**                    |
| `mise run ios-run`           | Build the iOS app and **launch it on the simulator**   |
| `mise run macos-app`         | Build, SwiftLint `--strict` and test the macOS app     |
| `mise run ios-app`           | The same against the iOS simulator                     |
| `mise run validate`          | Full local validation: Biome CI + typecheck + coverage |
| `mise run i18n`              | Regenerate the string catalog bindings from `i18n/*.toml` |
| `mise run i18n-check`        | Fail if the committed string bindings have drifted     |
| `mise run i18n-plurals`      | Plural-coverage check under en, pl and ar (or the locales named) |

### Git hooks

Git hooks are managed by [lefthook](https://github.com/evilmartians/lefthook) and defined in `lefthook.yaml`, which simply calls mise tasks so there is a single source of truth:

- **pre-commit** — `mise run fix`, `mise run typecheck`, and (for staged `*.rs` files) `mise run rust-fmt-check` + `mise run rust-clippy-workspace`. The `fuzz/` clippy (`mise run rust-clippy-fuzz-staged`) runs only when the staged files reach it, as `.github/scripts/fuzz-closure-touched.py` decides: anything under `fuzz/`, a file in a workspace crate the fuzz package compiles, or `Cargo.toml`, `Cargo.lock`, `.cargo/`, `rust-toolchain*` or `clippy.toml`. Run the lot, both clippy halves always, with `mise run pre-commit`.
- **pre-push** — `mise run ci`, `mise run typecheck`, `mise run rust-fmt-check` and `mise run rust-clippy-workspace`: the pre-commit checks over the whole tree, and no test suite. The tests run in CI on every push to `master`, nightly and on dispatch, not on a pull request (below). Run them locally before pushing with `mise run pre-push`, an explicit opt-in that runs the hook's checks and both suites whole, or scope them to what the push can affect with `mise run rust-test-changed` and `mise run test-changed`, as `.github/scripts/prepush-scope.py` decides from the changed files: the changed crates, every crate depending on one, and the crates whose sources name a changed file, with the `event_catalog` gate always run in full (`mise run log-fields`); the Vitest suite whenever a file it reads changed. A change to `Cargo.toml`, `Cargo.lock`, `.cargo/`, `rust-toolchain*` or `clippy.toml` widens the Rust run to the whole suite, and `SUNRISE_FULL_PREPUSH=1` widens both.

CI splits the same way. A pull request runs the fast checks: fmt and clippy for both cargo workspaces (`Rust (ubuntu-latest)`), Biome, the Bun typecheck, `cargo-deny`, and the small gate jobs. `master` is the development branch and a release is what stabilises it, so the slow suites run after the merge, on every push to `master`, nightly and on `gh workflow run ci.yml --ref <branch>`: the Rust tests, doc tests and rustdoc, Windows, the Kotlin seam, the wasm core, storage fixtures, the OpenAPI, token and deploy gates, the Bun tests, the web build, the per-change fuzz run, the Docker image, and the macOS and Apple jobs. On a pull request those jobs report `skipped`.

### Documentation site

`docs/` is also a website. `apps/docs` renders it with VitePress, unchanged — the
Markdown GitHub shows is the Markdown the site builds:

```bash
bun run --filter @sunrise/docs dev     # http://localhost:5173
bun run --filter @sunrise/docs build   # static site in apps/docs/dist
```

### Releasing

A release is cut by pushing a tag on one of two version trains; [`docs/07-clients/releasing.md`](docs/07-clients/releasing.md#two-version-trains) says which tag and what the pipeline does with it ([ADR-0064](docs/11-adr/0064-two-version-trains.md)).

## Translating Sunrise

Every user-visible string lives in one catalog, [`i18n/en.toml`](i18n/en.toml),
as ICU MessageFormat with CLDR plural rules; the Apple apps, the web client, the
CLI and the docs site read it through generated bindings
([`docs/10-cross-cutting/i18n.md`](docs/10-cross-cutting/i18n.md),
[ADR-0054](docs/11-adr/0054-string-catalog-pipeline.md)). English is the only
language shipped, and community translations are accepted once a surface is
stable.

- **The apps and the CLI:** copy the keys you are translating from
  `i18n/en.toml` into `i18n/<locale>.toml` (`pl.toml`, `pt-BR.toml`), then run
  `mise run i18n` and `mise run i18n-plurals <locale>`. A key you leave out
  falls back to English. The check fails a plural that is missing one of your
  language's CLDR categories.
- **The docs site:** a translated page lives at `docs/<locale>/…`, mirroring the
  English path.
- **This README:** a translation is `README.<locale>.md` beside it, linked from
  the **Languages** line at the top.

## License

Sunrise is licensed under the [AGPL-3.0 License](LICENSE).
