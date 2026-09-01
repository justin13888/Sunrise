# 0026 — The rustc pin moves to 1.91.1

**Status:** accepted

**Amends:** [ADR-0012](./0012-web-wasm-deferred.md) (its revisit trigger has
fired), [ADR-0019](./0019-swiftui-macos-client.md) (the stated reason for the
`uniffi-bindgen` quarantine).

## Context

`rust-toolchain.toml` read this, and had read it since the TUI was deleted:

```toml
# Pinned for reproducible builds. Bumped from 1.87.0 to 1.88.0 for ratatui
# 0.28's transitive `instability`/`darling`; ratatui is gone (ADR-0019) but the
# pin stays — it is a reproducibility floor, not a dependency workaround, and
# dropping back would be an unforced change. Any further bump requires an ADR
# and a CI matrix update.
channel = "1.88.0"
```

Every sentence of that is true, and it was the right call when it was written.
It is also the reason nobody looked at the number again for a year, during which
three unrelated pieces of the tree arranged themselves around it:

1. **`tools/uniffi-bindgen` carries a second lockfile to hold `cargo-platform`
   at 0.3.2**, because 0.3.3 requires 1.91. ADR-0019 records the quarantine and
   gives this as its reason. `mise run apple-xcframework` passes `--locked` with
   a comment saying the pin is what `--locked` protects.
2. **[ADR-0012](./0012-web-wasm-deferred.md) stopped the web WASM spike on it.**
   `rusqlite` 0.40 is the only line integrating `sqlite-wasm-rs`; it resolves
   `libsqlite3-sys` 0.38.1, whose `build.rs` uses `cfg_select!`, stable since
   1.91. At 1.88 that fails on the **native** `bundled-sqlcipher` build, before
   wasm is attempted — a direct failure of the spike's hard gate. The ADR names
   "the workspace MSRV moves to Rust ≥ 1.91" as its own revisit trigger.
3. **`sunrise-core` depends on `fs4` to do what `std` does.**
   `std::fs::File::try_lock` stabilized in 1.89. `Cargo.toml`'s own comment on
   the dependency said so: "Supersedable by `std::fs::File::try_lock` once MSRV
   reaches 1.89 — fs4's API is a deliberate 1:1 mirror, so that migration is a
   find-and-replace."

So the pin's defence had quietly inverted. It was kept because dropping *back*
would be an unforced change; by the time three things were waiting on it,
keeping it was the unforced change. Two of the three are load-bearing
complexity — a second lockfile and a third-party crate — carried purely to
stand in for a compiler.

A fourth problem is not about the number but about the record: the reason for
the `uniffi-bindgen` quarantine was restated in five places
(`tools/uniffi-bindgen/Cargo.toml`, `crates/sunrise-core-bindings/Cargo.toml`,
`mise.toml`, `README.md`, `docs/01-architecture/dependencies.md`), all five
named a dependency chain that does not exist, and nothing checked any of them. See Consequences.

## Decision

**Pin `rust-toolchain.toml` to `channel = "1.91.1"` and set the root
`Cargo.toml`'s `rust-version` to the identical string.**

1.91 is the *lowest* release that discharges all three items above: 1.89 would
retire `fs4` alone, and nothing below 1.91 clears either `cargo-platform` or
`cfg_select!`. The patch level is pinned exactly, as before — a floor that
floats within a minor is not a reproducibility floor.

`rust-version` is set to `1.91.1`, not `1.91.0`. It is not a publishing promise
here — every crate in the workspace is `publish = false` — it is the input
clippy reads to decide which MSRV-gated lints apply, and the number a reader
compares against the toolchain file. One number is easier to keep true than two,
and CI now asserts they are the same string.

Clippy on 1.91.1 with `rust-version = "1.91.1"` reports exactly three new
findings, all `manual_is_multiple_of`, all mechanical: `Segment::break_after`'s
long-break cycle test in `sunrise-domain`, and two modulo-based sampling
predicates in `sunrise-bench`'s fixture generator. All three are rewritten with
`.is_multiple_of(..)`. No `allow` is added anywhere, which was a precondition:
a bump paid for with a lint suppression buys a compiler and sells a guarantee.

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep 1.88.0** | Preserves three workarounds to avoid one version bump: a second lockfile, a redundant third-party crate, and ADR-0012's deferral held open by a blocker that has a one-line fix. The original rationale — "dropping back would be an unforced change" — argues *for* moving, not against: it says the number should follow need, and the need arrived. |
| **Track stable, unpinned** (`channel = "stable"`) | Discards the reproducibility floor that is the pin's entire purpose. Builds would differ between two developers on the same commit, and a lint introduced upstream would red CI on a commit that changed nothing. `docs/03-crypto/primitives.md` lists the pin as an implemented reproducible-build requirement; this would un-implement it. |
| **Pin 1.98.0 (current stable)** | Measured, not assumed. With `rust-version` raised to match, clippy reports **sixteen** findings against the 1.88 baseline rather than three — thirteen beyond the ones this bump already fixes: five `unused_async_trait_impl`, four `map(..).unwrap_or(..)` on a `Result`, two `Duration` unit-readability, one `sort_unstable_by_key`, one byte-string literal. Eight of the thirteen are mechanical. The other five are the problem: **four of them land on `Core::open`, `Core::submit`, `Core::apply_remote` and `Core::query`** — the whole public surface of the core, `async` by design because the UniFFI seam is async and because a method that does not await today will tomorrow. Clearing them means adding a lint `allow` or desugaring the public API; the fifth sits on a trait impl in `sunrise-server`. Neither price is worth paying for six weeks: stable ships on a six-week cadence, so "current stable" stops being current in about four. |
| **Pin 1.89.0** | Clears `fs4` and nothing else. `cargo-platform` and `cfg_select!` both need 1.91, so the bindgen quarantine's second lockfile and ADR-0012's deferral would both survive — two of the three reasons to move, unaddressed, and another ADR owed the next time anyone looks. |
| **Pin 1.91.1** (chosen) | The lowest number that clears all three. Three mechanical clippy sites, no `allow`, no API change, and it is precisely ADR-0012's stated trigger rather than an approximation of it. |

## Consequences

* **ADR-0012's revisit trigger has fired.** Its condition was "the workspace
  MSRV moves to Rust ≥ 1.91", and it now has. That does not re-open the web
  core by itself — the trigger removes the reason the spike *stopped*, not the
  work it *sized*: `rusqlite` 0.31 → 0.40 across `sunrise-storage` and
  `sunrise-core` is nine minor versions and 100+ call sites, and it swaps the
  native SQLite/SQLCipher stack wholesale. That work is
  [#52](https://github.com/justin13888/Sunrise/issues/52), and ADR-0012's hard
  gate applies to it unchanged. Until it lands, `docs/07-clients/web.md`'s
  `localStorage` stub is still the v1 web story; only the stated reason changes.
* **ADR-0019's quarantine survives, on a different and better reason.**
  `cargo-platform` is unpinned (0.3.2 → 0.3.3) and `tools/uniffi-bindgen` stays
  outside the workspace — because of **feature unification**, not MSRV.
  `sunrise-core-bindings` depends on `uniffi` too; as a workspace member the
  generator would ask the same package for `cli` (= `bindgen` + `clap` +
  `camino`), and resolver 2 unifies features per package across one cargo
  invocation, so the seam crate would compile against a `uniffi` carrying the
  whole binding generator. Measured: 21 packages live in that directory's
  lockfile and nowhere else in the tree.
* **The five stale explanations are corrected.** The claim in
  `tools/uniffi-bindgen/Cargo.toml`, `crates/sunrise-core-bindings/Cargo.toml`,
  `mise.toml`, `README.md` and `docs/01-architecture/dependencies.md` — that
  uniffi's defaults pull `uniffi_bindgen -> cargo_metadata -> cargo-platform` —
  is wrong. uniffi 0.32's `default = ["cargo-metadata"]` takes
  `dep:cargo_metadata` **directly**; `uniffi_bindgen` sits behind a separate
  optional feature the default set never enables. This matters beyond pedantry:
  read literally, the old comment says the quarantine exists only because of the
  MSRV, which would have made this ADR look like grounds to dissolve it.
  `default-features = false` on the seam crate also stays, for the reason it
  always had — the bindings are generated in `--library` mode from the compiled
  dylib, with no `.udl` and no `build.rs`, so `cargo-metadata` is simply unused
  there.
* **`fs4` leaves the tree.** `crates/sunrise-core/src/vault_lock.rs` uses
  `std::fs::File::try_lock` / `unlock`, and `Cargo.lock` loses exactly one
  package. The syscall is unchanged — verified in the 1.91.1 std source,
  `flock(LOCK_EX | LOCK_NB)` on Apple/Linux/BSD, `LockFileEx` on Windows — so
  the flock-over-fcntl guarantee `docs/01-architecture/shared-core.md` depends
  on, and its regression test `lock_survives_contender_reading_metadata`, are
  untouched.
* **Three clippy sites changed, no `allow` added.** Raising `rust-version` turns
  on MSRV-gated lints, which is a real cost of any bump and the reason to prefer
  the lowest workable number.
* **The number is now asserted rather than repeated.** A new `msrv` job in
  `.github/workflows/ci.yml` requires `rust-toolchain.toml`'s `channel`, the
  root `Cargo.toml`'s `rust-version`, and the `rustc` a checkout actually
  resolves to be one string. That last check also proves the toolchain-file
  override takes effect at all — an assumption the `rust`, `macos-app` and
  `ios-app` jobs already depended on and none of them tested.
* **The `Dockerfile`'s `ARG RUST_VERSION` moves to 1.91.1**
  (`rust:1.91.1-slim-bookworm` is published) and the three CI comments naming
  1.88.0 name 1.91.1. The Dockerfile is deliberately **not** covered by the
  `msrv` job: it is not built in CI, and a docker build to check one `ARG` would
  cost minutes to guard a line a reader can see.
* **1.88 still appears in the tree, and every remaining mention is historical**
  — ADR-0012, ADR-0019 and ADR-0021's bodies, and the "it used to be" clauses in
  the manifests this change rewrote. ADR bodies are records of a decision at a
  moment; editing them to agree with today is how a ledger stops being worth
  reading (see [ADR-0020](./0020-v1-must-demotions.md)'s addendum). ADR-0021's
  "MSRV is unaffected: the workspace pins 1.88.0 and `spargen` needs 1.88" is
  such a record; `spargen`'s floor is below the new pin, so nothing it asserted
  has broken.

## What would force revisiting this

1. **A dependency the workspace wants requiring more than 1.91.** The same
   analysis applies, and the answer should again be the lowest release that
   clears everything then outstanding — not the newest.
2. **A clippy lint that cannot be satisfied without an `allow` or an API
   change.** That is what ruled 1.98.0 out here, and it is the condition that
   makes a bump a design decision rather than a version bump. If it ever forces
   the choice, the API change is the honest answer and the `allow` is not.
3. **#52 landing.** Once `rusqlite` 0.40 is in, `libsqlite3-sys` 0.38.1's
   `cfg_select!` becomes a real compile-time dependency on 1.91 rather than a
   prospective one, and this pin stops being reversible even in principle.
4. **A second toolchain appearing.** The `msrv` job asserts one number in three
   places. A cross-compilation or a nightly-only job that needs a different
   toolchain would need that job extended rather than bypassed, or the assertion
   silently stops meaning what it says.
