# 0077 — The toolchain floats on stable; `rust-version` stays the floor

**Status:** accepted

**Amends** [ADR-0026](./0026-msrv-bump.md) (its exact `channel` pin and the
four-way equality CI asserted; its choice of 1.91.1 as the floor stands) and
[ADR-0061](./0061-crypto-audit-scope.md) §Consequences ("The pinned nightly
ages": `fuzz-pr` now uses the floating nightly).

## Context

ADR-0026 pinned `rust-toolchain.toml` to `channel = "1.91.1"` and required
`rust-version` and the `Dockerfile`'s `ARG RUST_VERSION` to be byte-identical
to it. That made every compiler upgrade an ADR, and so in practice no upgrade
happened: the tree compiled on a release that was already several behind
stable on the day it was chosen. ADR-0026 itself measured 1.98.0 and rejected
it because raising `rust-version` to match turned on thirteen more MSRV-gated
clippy lints.

Those two numbers do different jobs and the pin welded them together:

* the **compiler** CI and developers build with, which buys new syntax, std
  APIs, diagnostics, lints and compile-time improvements by being new; and
* the **floor** (`rust-version`), which decides which MSRV-gated lints clippy
  applies and what a reader takes as the lowest supported release.

Rust 1.x is forward-compatible: code that builds on 1.91.1 builds on every
later 1.x, barring the rare soundness fix. What a new stable can add is a new
warning, and this tree denies warnings, so a new lint can red a build. That is
the one real cost, and it is a cost of doing the work late rather than a reason
not to do it.

## Decision

* `rust-toolchain.toml` names `channel = "stable"`. Components and profile are
  unchanged.
* The `Dockerfile` builds on `rust:1-slim-bookworm` (`ARG RUST_VERSION=1`), the
  floating tag for the latest 1.x, and no longer copies `rust-toolchain.toml`
  into the image (its `stable` would make rustup download a second toolchain
  beside the image's own).
* `rust-version` stays `1.91.1` in the root and `fuzz/` manifests. It is the
  floor, ADR-0026's three reasons for it still hold, and it is not raised just
  because stable moved. MSRV-gated lints therefore stay where ADR-0026 put them.
* The `fuzz-pr` job uses the floating `nightly`, like the nightly `fuzz` job.
  Its failures to install or build were already warnings rather than red
  checks.
* The first step of the Rust gate (`.github/actions/rust-checks/action.yml`)
  now asserts that the channel is `stable`, that the Dockerfile's
  `ARG RUST_VERSION` is `1`, and that the `rustc` a checkout resolves to is not
  below `rust-version`. A literal version reappearing in either file fails it,
  and so does an override that did not take effect.
* A new lint that fires on a new stable is fixed in the change that meets it,
  or in a follow-up opened the day it lands. An `allow` still needs the reason
  ADR-0026 asks for.

## Alternatives considered

| Option | Why not |
|---|---|
| **Keep the exact pin, bump it by ADR** | What the tree did. Every bump is a design decision, so it does not happen, and the lints accumulate until the bump is large. ADR-0026's 1.98.0 measurement is that cost made visible. |
| **Pin, and let Dependabot or a bot bump it** | Neither Dependabot nor Renovate's default config moves `rust-toolchain.toml` and the Dockerfile `ARG` together, and the bump PR still carries every new lint at once. It is the floating toolchain with extra steps and a lag. |
| **Float the toolchain and raise `rust-version` with it** | Turns on every MSRV-gated lint on each release and throws away a floor that means something. The floor moves when a dependency or a std API needs it, as ADR-0026 says. |

## Consequences

* CI and developers build on the current stable without anyone editing a
  file. A local `rustup update` is all a developer ever does.
* A new stable can red the Rust gate with a lint the change under review did
  not cause. The fix is in-repo and usually mechanical; it is the same work a
  pin defers.
* A tagged release is built on the stable current that day rather than a
  number in the tree. `docs/03-crypto/primitives.md` §Reproducible builds says
  so; reproducing a tag means building it with that release
  (`RUSTUP_TOOLCHAIN=1.x.y`, `--build-arg RUST_VERSION=1.x.y`).
* rust-cache already keys its entries on the `rustc` version, so a new stable
  invalidates the cache on its own.

## What would force revisiting this

1. **A stable release that miscompiles or regresses something this tree
   depends on** and cannot be worked around in-repo. Pin the previous release
   in `rust-toolchain.toml` with a comment naming the upstream issue, and
   float again once it is fixed.
2. **A nightly-only language feature becoming necessary.** Nothing uses one
   today; the fuzz harnesses need nightly only for sanitizer flags.
