# 0064 — Sunrise releases on two version trains, core and app, and nothing publishes to a registry

**Status:** accepted

**Amends** [ADR-0042](./0042-v0-forever.md) §1. "The product version" becomes
two train versions, the core train's and the app train's. Everything else in
ADR-0042 stands: both numbers stay `0.MINOR.PATCH` forever, neither carries
compatibility meaning, and compatibility stays on the versioned surfaces of
§2.

**Implemented by** `.github/workflows/release.yml` (the tag patterns and the
per-train jobs), `.github/scripts/publish-gate.py` (the "nothing publishes"
half, and the app train's single version), and `core_build_info` in
`crates/sunrise-core-bindings/src/build_info.rs` (the app reporting the core
it embeds).

## Context

Until now one number versioned everything. `[workspace.package] version` in
`Cargo.toml` was checked against a `vX.Y.Z` tag by `release.yml`'s `verify`
job, and every artifact of that tag carried it: the CLI and server tarballs,
the server image, the macOS `.dmg` and its appcast item, the iOS upload and the
web deployment. `apps/apple/project.yml`'s `MARKETING_VERSION` and the
`package.json` versions were separate copies that nothing compared, and the
release overrode `MARKETING_VERSION` from the tag to hide the difference.

Three things made that one number wrong.

1. **The two halves change for different reasons and at different rates.**
   Protocol work is coming that moves the core: the server, the CLI and the
   protocol surfaces. A UI fix in the Mac app moves none of that. One number
   means either a core release for every UI fix, re-pushing an identical server
   image under a new tag, or an app release held back until the core is ready.
2. **One tag means one all-or-nothing run.** A core release needed the eight
   Apple secrets, because the `.dmg` was built by the same tag, and an app
   release rebuilt and re-pushed the server image. The `verify` preflight
   exists because a half-published tag (image up, `.dmg` failed) is worse than
   none. Splitting the trains removes the coupling that made it possible.
3. **"Which core is this app?" had no answer in the app.** The bindings
   used `CARGO_PKG_VERSION` only as the `app_version` of the device record
   they register with the relay. Nothing the app could show said which core it linked or which
   protocol surfaces that core spoke.

The registry question was implicit in the same file. `release.yml` ends with
"publish targets deliberately absent": crates.io, npm and Google Play. Every
crate in the workspace said `publish = false` and every `package.json` said
`"private": true`. Nothing checked it, and `legacy/` held four `package.json`
files with no `private` key at all.

## Decision

### 1. Two trains, each with one source of truth

| Train | What it covers | Source of truth | Tag | Artifacts |
|---|---|---|---|---|
| **core** | Everything in the Cargo workspace (every crate, including `sunrise-server`, `sunrise-cli`, `sunrise-core-bindings` and `sunrise-core-wasm`), `schemas/`, and the protocol constants | `[workspace.package] version` in the root `Cargo.toml` | `core-vX.Y.Z[-pre]` | CLI and server tarballs, the `sunrise-server` container image |
| **app** | `apps/apple`, `apps/web`, `apps/docs`, `packages/sunrise-ui`, `packages/sunrise-ui-tokens`, `packages/sunrise-i18n` | `version` in the root `package.json` | `app-vX.Y.Z[-pre]` | the macOS `.dmg` and the appcast, the iOS upload, the web deployment |

Every crate inherits the core version, so it has one copy. The app train has
copies that cannot be inherited, because Bun, npm and Xcode each read their own
file: every `apps/*/package.json` and `packages/*/package.json` `version`, and
`apps/apple/project.yml`'s `MARKETING_VERSION`. Each must equal the root
`package.json`, and the publish gate (§4) fails CI when one does not.

`schemas/generated/openapi.v1.json`'s `info.version` is the core version. The
document describes the server, and the server is core.

The core train starts at `0.2.0`, for the protocol changes above. The app
train stays at `0.1.0`.

### 2. A tag releases one train

`release.yml` triggers on `core-v*` and `app-v*` and no longer on a bare `v*`.
`verify` reads the train from the prefix and checks the version against that
train's source: `Cargo.toml` for core, and the root `package.json` plus every
`MARKETING_VERSION` for app. `binaries` and `image` run only for core.
`macos-app`, `appcast`, `ios-release` and `web-pages` run only for app, and the
eight macOS secrets are required only for an app tag. The GitHub Release is
named by train: "Sunrise core 0.2.0", "Sunrise app 0.1.0".

A core Release is created with `make_latest=false`. The macOS app's
`SUFeedURL` resolves through GitHub's `releases/latest` alias, so a core
release that became `latest` would point every installed copy at a Release
with no `appcast.xml`. Only an app release moves the alias, and the appcast job
refuses to publish if `latest` is ever a core release.

The appcast is built from app releases only. `v0.1.0-rc.1`, tagged before the
split, is read as an app release: it is the only tag in the old scheme, and it
is where the macOS app was first released. It also stays the predecessor for
the first release notes of each train.

The manual dry run builds the unsigned `.dmg` and nothing else, so a dispatch
is an app-train run and takes its version from the root `package.json`.

### 3. The app reports the core it embeds

`sunrise-core-bindings` exports `core_build_info()`, a record of the core
version (`CARGO_PKG_VERSION`) and `WIRE_PROTO_V`, `ENVELOPE_FORMAT_V`,
`DOC_SCHEMA_V`, `CRYPTO_SUITE_V` and `STORAGE_V`, read from
`crates/sunrise-cbor/src/version.rs`. It is additive. Nothing existing in the
seam changes. It is for display and diagnostics: a client MUST NOT branch on
it, for the same reason ADR-0042 §1 gives for `Hello.client_app_v`.

### 4. Nothing publishes, and a gate says so

- Every tracked `Cargo.toml` with a `[package]` table says `publish = false`
  in its own `[package]`. Inheriting it is refused. `tools/uniffi-bindgen`,
  `fuzz/` and `crates/sunrise-e2e/baseline-driver` are outside the root
  workspace, and the baseline driver is built inside another commit's
  workspace by design, so what a manifest inherits depends on where it lands.
- Every tracked `package.json` says `"private": true`.
- The app train's copies agree (§1).

`.github/scripts/publish-gate.py` checks all three, with its contract in
`test_publish_gate.py`. CI runs them as "Publish gate" and "Publish gate
contract".

Internal path dependencies in `[workspace.dependencies]` carry no `version`.
With every crate private, a version requirement on a path dependency is read by
nothing, and it is one more copy of the core number to drift. `deny.toml` sets
`allow-wildcard-paths = true` under `[bans]`, which is what lets cargo-deny
accept a path dependency of a private crate without a version. A `*` on
anything resolved from a registry is still denied.

`legacy/`, the pre-rewrite prototype, is deleted. It was outside both
workspaces, nothing built or tested it, and its four `package.json` files were
the only manifests in the tree without `"private": true`. History keeps it.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep one product version** | This is the coupling §Context describes. Every UI fix becomes a server re-release, or every app release waits for the core. Every release needs every credential. |
| **One train per artifact (server, CLI, macOS, iOS, web, docs)** | The server and CLI are built from one workspace at one `Cargo.lock`, and the Apple apps and the web client share `packages/`. More numbers than independent build inputs only adds numbers that move together. |
| **Version the app train from `project.yml`** | Xcode's file is the only one in the app train that Bun cannot read and that the web and docs builds do not touch. The root `package.json` is the one file every app-train build already has. |
| **Inherit the app version instead of copying it** | Nothing inherits across Bun, npm and Xcode. A gate that compares the copies is the only mechanism all three tools accept. |
| **Keep `version = "0.1.0"` on internal path dependencies** | It is required only for publishing. Here it would be a second copy of the core version in 24 places, and none of them is read. |
| **Keep `legacy/` as reference** | Git history is the reference. A tree nothing builds is a tree every gate has to remember to skip, and the citation and docs-link gates carried that exception. |
| **Two trains, one gate, one exported build-info record (chosen)** | Each version question has one file that answers it, and CI fails when a copy disagrees. |

## Consequences

- **Tags change shape.** The next releases are `core-v0.2.0` and `app-v0.1.0`
  (or prereleases of them). A `vX.Y.Z` tag no longer triggers anything.
- **A core release needs no Apple credentials, and an app release pushes no
  image.** The `verify` preflight for the macOS secrets runs only for an app
  tag. "Image pushed, no Release" can still happen within the core train, if a
  `binaries` leg fails after `image` has pushed. It can no longer be caused by
  the `.dmg`.
- **The Releases page interleaves two trains.** Release names and tag prefixes
  say which is which, and `latest` is always an app release (§2).
- **The cross-version merge matrix (ADR-0057) takes core releases.** The
  invariant binds the core. "A release cut at or after the floor joins the
  matrix" now means a `core-v*` release.
- **Bumping the app train is several edits, and the gate lists them.** The root
  `package.json`, every app-train `package.json` and `MARKETING_VERSION`. A
  bump that misses one fails CI and names the file.
- **The About box can say which core it runs.** `core_build_info()` is
  exported. Showing it in the Apple apps is a separate change, and this record
  does not make it.

## What would force revisiting this

1. **An artifact that needs to release on its own cadence within a train.** For
   example, the web client deploying on every merge while the Apple apps cut
   tagged releases. That would be a third train with its own ADR, not a looser
   rule for the app train.
2. **A reason to publish to a registry.** For example, a crate other projects
   consume, or an npm package of the design tokens. That reverses §4 for that
   package. The publish gate would gain a named, reasoned exemption, and the
   package would need its own version, probably as a third train.
3. **A store that requires a 1.x marketing version.** ADR-0042's own revisit
   item 1. It would now affect the app train only.
