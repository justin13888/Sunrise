# 0029 — Design tokens are compiled from TOML, committed, and drift-checked

**Status:** accepted

**Amends:** [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md)
§Tokens (the spacing scale, the stream palette, and the removal of `tokens.kt`
from the emitted set).

## Context

`docs/07-clients/shared-ui-system.md` §Tokens has specified a design-token
build — `packages/sunrise-ui-tokens/`, TOML sources, a `build.ts` emitting
`tokens.css` / `.swift` / `.kt` / `.rs` — since it was written, and none of it
existed. [#29](https://github.com/justin13888/Sunrise/issues/29) is that gap.
What existed was `packages/sunrise-ui/src/tokens.ts`: 39 hand-written lines
holding eight stream colours, a spacing scale, four radii and four glyphs, with
a header comment asking a reader to keep it in step with `StreamColor` in
`crates/sunrise-domain/src/stream.rs` and nothing enforcing that.

Building it forces four decisions that a future contributor would reasonably
wonder about. Three are recorded here; the fourth (TypeScript is a fourth
emitter target, not one of the doc's three) follows from the first two and is
noted under Consequences.

## Decision

### 1. `tokens.rs` is an `include!`-ready file, not a crate

The Rust output lives at `packages/sunrise-ui-tokens/generated/tokens.rs` and
is not a Cargo workspace member. `Cargo.toml` and `Cargo.lock` are untouched.

There is no Rust consumer today and that is not an oversight:
[`../01-architecture/shared-core.md`](../01-architecture/shared-core.md) §What
the core does *not* do opens with "It does not decide UI text, copy, or icons",
and [`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)
specifies the CLI as "plain text on stdout, one record per line, no colour and
no glyph art". So the file is emitted for the consumer that does not exist yet,
and shaped so that adding one costs a single line:

```rust
include!("../../packages/sunrise-ui-tokens/generated/tokens.rs");
```

That constraint is load-bearing on the file's *form*: an `include!` expands
into an existing module body, where an inner doc comment (`//!`) is a hard
parse error (`E0753`). The banner is therefore `//` line comments and every
item carries an outer `///`. The module is primitives and arrays only, so it
compiles inside a `#![no_std]` crate unchanged.

`mise run tokens-check` compiles it with `rustc --emit=metadata` and runs
`rustfmt --check` over it. Those two checks are the whole reason it is a Rust
file rather than a text file with a `.rs` extension: nothing else would notice
it stop compiling.

**Rejected: a `crates/sunrise-tokens` crate.** It would have no path to any
shipping binary and would therefore fail `.github/scripts/orphan-crate-gate.py`
— a gate whose diagnosis would be *correct*. Exempting it would silence a true
finding to make room for a file nobody reads. It would also touch
`Cargo.lock`.

**Rejected: a module inside `sunrise-domain`.** Barred by `shared-core.md`
above: a palette is a presentation decision, and the core does not make those.

**Reverses when:** the first real Rust consumer appears. It adds the `include!`
and nothing else.

### 2. The generated files are committed and drift-checked

`generated/` is in the repository. The generator is not run during any
consumer's build.

The repository already contains both patterns, and the discriminator between
them is whether the consumer's build environment can run the generator. The
UniFFI bindings are generated at build time (`mise.toml` `apple-xcframework`,
invoked from the `SunriseFFI` aggregate target) and gitignored, because Xcode
*can* be made to run cargo — `apps/apple/project.yml` does exactly that, and
has to inject `/opt/homebrew/bin` and `$HOME/.cargo/bin` onto `PATH` to manage
it. `schemas/openapi.v1.json` is generated, committed, and guarded by
`the_committed_description_is_current`, because `spargen` reads a *file*.

Tokens are the second case, and harder: Xcode's environment has neither Bun nor
mise, and there is no equivalent of the cargo injection because there is no
Bun to inject — a contributor who has Xcode does not necessarily have Bun at
all. So `tokens.swift` must already exist when Xcode opens the project.

Committing a generated file is only safe with a gate, so there are three,
deliberately overlapping:

| Gate | Catches | Runs |
|---|---|---|
| `packages/sunrise-ui-tokens/test/drift.test.ts` | TOML edited, `mise run tokens` not re-run | `mise run test`, and `lefthook.yaml` on pre-push |
| `mise run tokens-check` | the same, plus `tokens.rs` no longer compiling or formatting | by hand |
| `.github/workflows/ci.yml` `tokens-current` | the same, on every pull request | CI |

**Rejected: an Xcode script phase plus a Vite plugin, with `generated/`
gitignored.** It puts a Bun installation on the critical path of `xcodebuild`
and buys nothing the drift gate does not already give.

### 3. The doc's six-step spacing scale supersedes the shipped five-step one

`spacing` becomes `xs 4, sm 8, md 12, lg 16, xl 24, xxl 32`. The scale
`packages/sunrise-ui/src/tokens.ts` carried — `xs 4, sm 8, md 16, lg 24, xl 32`
— is gone. `md` changes from 16 to 12, `lg` from 24 to 16, `xl` from 32 to 24,
and `xxl` is new.

The evidence is the Apple app's own literals, which are the only large sample
of what this product actually spaces things by. Counting the non-zero integer
arguments to `.padding(...)` and `spacing:`:

```
$ grep -rhoE '\.padding\((\.[a-z]+, )?[0-9]+\)|spacing: [0-9]+' apps/apple \
    --include='*.swift' | grep -oE '[0-9]+' | grep -v '^0$' | sort -n | uniq -c
   3 1      10 4      15 10      18 16      3 24
   9 2      25 6      42 12       1 18      3 40
  10 3      45 8       7 14       2 20
```

193 sites. 118 of them (61%) land on a step of the six-step scale; 76 (39%)
land on a step of the five-step one. The decisive value is `12` — the
second-most-used number in the app, at 42 sites — which the five-step scale
cannot express at all.

The rename is free in practice: `packages/sunrise-ui`'s one consumer,
`apps/web`, imports `taskStateGlyph` and nothing else.

**Rejected: amend the doc to say `md = 16`.** That would settle the
disagreement in favour of the scale with less evidence behind it, and would
leave `12` unnameable.

## Consequences

- **TypeScript becomes a fourth emitter target**, which the doc's original
  three did not include. It follows from the two decisions above: `tokens.css`
  cannot type `spacing.md`, and #29's headline complaint is that `tokens.ts` is
  hand-written. `packages/sunrise-ui/src/tokens.ts` is now a naming layer over
  the generated object.
- **`tokens.kt` is not emitted.** There is no Android target to compile it and
  no Kotlin consumer to read it. It lands with the Android client.
- **The stream palette is enumerated, not generated.** The doc specified
  `stream-1..stream-12` derived from `accent`; `StreamColor` in
  `crates/sunrise-domain/src/stream.rs` is a serde-stable eight-variant enum
  whose lowercase names are persisted in the vault and parsed back leniently by
  `from_str_lossy`, so renumbering the palette would be a storage-format change
  rather than a design change. `test/invariants.test.ts` reads
  `StreamColor::as_str` out of the Rust and fails when the two lists diverge —
  which is the enforceable form of the comment `tokens.ts` used to carry.
- **Easing is four control points, not a CSS string.** Only one of the four
  emitters speaks CSS; storing `cubic-bezier(0.2, 0, 0, 1)` would make the
  Swift and Rust emitters parse it back out.
- **Contrast is a test.** `test/invariants.test.ts` computes WCAG 2.1 ratios
  over the surface palette, which turns
  [`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)
  §Color and contrast from an intention into a failure.
- **The generator parses TOML with `smol-toml`, not `Bun.TOML`.** Vitest runs
  the drift test under Node, and `@vitest/coverage-v8` cannot run under Bun at
  all, so a Bun-only loader would have put the gate out of reach of
  `mise run test` and `mise run test-coverage`. The package is runtime-agnostic
  as a result.
- **Only two Apple call sites were migrated**, both places where a hard-coded
  value was already a documented defect: `StreamColor.tint` (the one place the
  app decided a stream's colour) and `DropHighlight`. The 193 raw spacing
  literals counted above are left alone; 75 of them are off-scale, and each is
  a design judgement rather than a substitution.

## What would force revisiting this

1. **A Rust consumer.** Decision 1's `include!` shape is the cheap path; a
   second Rust consumer in a different crate would argue for a real crate and
   an orphan-gate conversation.
2. **An Android client.** It adds `tokens.kt` and a fifth emitter, and makes
   the Kotlin half of the doc true.
3. **A design system with real components.** These are tokens, not a component
   library; `packages/sunrise-ui` is still a naming layer and nothing more.
