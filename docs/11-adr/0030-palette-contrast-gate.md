# 0030 — Every palette colour clears a stated contrast threshold, and the loader enforces it

**Status:** accepted

**Amends:** [ADR-0029 — Design tokens are compiled from TOML, committed, and
drift-checked](./0029-design-token-pipeline.md) §Consequences, whose "Seven
light-theme colours do not clear AA against `bg`" bullet is settled here, and
[`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md) §Color
(seven light values, and the standing question about what stream tints are for).

## Context

[`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)
§Color and contrast asks for 4.5:1 on text and on interactive elements, and
prefers 7:1 for body text. ADR-0029 made that computable and then found the
palette did not meet it: seven light-theme colours are below 4.5:1 on
`bg` — `warning` 3.08, `info` 3.56, `success` 3.64 in `[surface]`, and `amber`
3.08, `emerald` 3.64, `sky` 3.96, `pink` 4.44 in `[stream]`. It recorded the
numbers and changed nothing, on the grounds that raising them is a design
decision rather than a pipeline one.

What made that safe to defer is also what made it certain to rot.
`test/invariants.test.ts` asserted **three** pairs — `fg`/`bg`, `muted`/`bg`,
`accent_text`/`accent` — chosen by hand. The other eighteen colours in the two
themes were nobody's: not asserted, not exempted, not enumerated anywhere. A
palette is not a set of independent colours, so "the pairs somebody remembered"
is not a coverage model; it is the reason seven values shipped below the bar
the repository's own doc sets, and the reason an eighth could be added tomorrow
without anything noticing.

[#75](https://github.com/justin13888/Sunrise/issues/75) asks for both halves:
raise the colours, and make the raising stick.

## Decision

### 1. The gate is a rule table in the loader, not assertions in a test

`packages/sunrise-ui-tokens/src/contrast.ts` holds a table of every pair that is
drawn one on the other, each with the ratio it owes and the role that picks that
ratio, plus a short list of keys that are exempt *with the reason*.
`model.ts`'s `parseTheme` calls it, so a palette below threshold is a file the
generator refuses to compile.

Placing it there rather than beside it is what gives it reach. One call site
means `mise run tokens` fails, and therefore so do `mise run tokens-check`, the
`tokens-current` CI job and `test/drift.test.ts` — each of which runs the
generator — without any of them knowing contrast exists. `build.ts` catches
`TokenError` and prints the message alone on stderr, because `tokens-check`
discards stdout and a stack trace through `smol-toml` buries the sentence the
editor needs.

The table is **exhaustive**, and that is the load-bearing property rather than
its contents: every key a theme declares must appear as a rule's foreground, or
as the background itself, or in the exemption list, and a key that is none of
those is itself a failure. That is the assertion that would have caught the
seven, and the one that catches the next token added to `[surface]`.

**Rejected: more `it(...)` blocks in `invariants.test.ts`.** It is where the
three existing assertions live, so it is the cheap answer, and it fails the
issue's actual requirement in two ways. `mise run tokens` would still *write* a
failing palette — the four generated files, which Xcode and Vite consume as
inputs, would carry colours the repository considers broken until somebody ran
the test suite. And a test cannot enforce exhaustiveness over the token set
without duplicating the token set, which is the same hand-maintained list that
produced the gap.

**Rejected: a standalone script, or a fourth `mise` task.** `mise run
tokens-check` is where this repository puts token gates and lefthook and CI
already reach it. A `tokens-contrast` task would be a gate whose failure mode is
that nobody calls it.

`test/invariants.test.ts` keeps a role, changed: it imports the rule *table* and
recomputes the ratios from its own transcription of WCAG 2.1, so the claim
about which pairs matter and the arithmetic that checks them do not come from
the same code. Asserting the palette with `contrastFailures` would compare the
gate to itself — precisely the blindness ADR-0029 documents for the drift check.
It also asserts that the loader really does refuse a short palette, and that the
message names the token, the ratio it got and the ratio it needed.

**Reverses when:** nothing foreseeable. Moving the table back out of the loader
reintroduces the failure mode it exists to close.

### 2. Roles pick thresholds; `border` is exempt and says so

| Pair | Threshold | Why |
|---|---|---|
| `fg` on `bg` | 7:1 | body text; the doc prefers AAA and the palette already cleared it |
| `muted` on `bg` | 4.5:1 | secondary text |
| `accent` on `bg` | 4.5:1 | the interactive foreground, which the doc holds to AA |
| `accent_text` on `accent` | 4.5:1 | text on an accent fill |
| `danger`, `warning`, `success`, `info` on `bg` | 4.5:1 | status text |
| every `[stream]` tint on `bg` | 4.5:1 | a stream label colour — see decision 3 |
| `border` | *exempt* | a hairline separator; WCAG 2.1 1.4.11 exempts decoration |

`bg` is the background rather than a foreground, and `accent_text` is never
drawn on `bg` — it is white on light and the dark background on dark, so
measuring it there would assert a pair no client can produce.

`border` is the one colour that cannot be made to pass and is not being made to.
It is 1.21:1 on the light background and 1.29:1 on the dark one; 1.4.11's 3:1
would need roughly `#767679` on `#fbfbfa`, which is not a hairline any more but
a rule, and the design language here is subtle separation. The claim that makes
the exemption legitimate is narrow — 1.4.11 covers "visual information required
to identify user interface components", and a decorative divider is not that —
and it is **conditional**: nothing in `apps/` reads `border` today, and the
moment it draws the boundary that identifies a control it owes 3:1, which is a
palette change rather than a rule change. Those numbers are written into
`contrast.ts` beside the exemption rather than left to be rediscovered, and the
test asserts that every exemption carries a reason at all.

**Rejected: holding `border` to 3:1 and darkening it.** It would trade a real
design decision for a green number against a requirement that does not currently
apply, which is the inverse of the mistake this ADR is fixing.

**Rejected: dropping `border` from the model.** It is a specified token with a
Swift consumer in `Sunrise/Design/Tokens.swift`; deleting a token to avoid
measuring it is not a contrast decision.

### 3. Stream tints are held to text contrast, not to 3:1

All eight tints must clear 4.5:1 against their theme background, as if they were
label colours, even though today they are drawn as an
`Image(systemName: "circle.fill")` beside a name in `BrowseSidebar` and
`StreamEditorView` — decoration, which 1.4.11 would let off at 3:1.

ADR-0029 left this open and the issue asks for it to be settled. Three things
settle it toward the stricter reading:

- **The two tables already share values.** `warning` *is* `amber` and `success`
  *is* `emerald`, by hex, in both themes. Status colours are text, so a 3:1 tint
  set and a 4.5:1 surface set would put two near-identical ambers and two
  near-identical greens into one palette — a worse design than one of each, to
  buy a decorative dot a lightness nobody asked for.
- **3:1 is not a margin here.** `amber` at 3.08 clears 3:1 by 0.08. A threshold
  the shipped palette sits 2.6% above is a gate that reports "fine" right up to
  the point where it reports a crisis.
- **The requirement follows the use, and the use is not fixed.** A stream's
  identity colour drawn on its name is the obvious next screen, and nothing in
  `shared-ui-system.md` ruled it out. Holding the palette to text contrast means
  that screen does not need a palette audit first — which is the trap the issue
  names, closed rather than documented.

`shared-ui-system.md` §Color now says this, with the caveat that it constrains
the palette and does not license colour as a lone signal.

**Rejected: 3:1, with a note that a tint used as a label needs re-checking.**
That is the state this ADR is replacing: a true sentence in a doc, enforced by
whoever remembers it.

### 4. The failing colours move one step down their own Tailwind ramp

Seven light values change. Each moves from the 600 step to the 700 step of the
same hue — the *lightest* step that clears 4.5:1 on `#fbfbfa`:

| token | was | ratio | now | ratio |
|---|---|---|---|---|
| `surface.warning` | `#d97706` amber-600 | 3.08 | `#b45309` amber-700 | 4.85 |
| `surface.success` | `#059669` emerald-600 | 3.64 | `#047857` emerald-700 | 5.30 |
| `surface.info` | `#0891b2` cyan-600 | 3.56 | `#0e7490` cyan-700 | 5.17 |
| `stream.amber` | `#d97706` amber-600 | 3.08 | `#b45309` amber-700 | 4.85 |
| `stream.emerald` | `#059669` emerald-600 | 3.64 | `#047857` emerald-700 | 5.30 |
| `stream.sky` | `#0284c7` sky-600 | 3.96 | `#0369a1` sky-700 | 5.73 |
| `stream.pink` | `#db2777` pink-600 | 4.44 | `#be185d` pink-700 | 5.83 |

Nothing else moves. `danger` 4.66, `accent` 4.99, `muted` 4.90, `rose` 4.54,
`violet` 5.50, `indigo` 6.07 and `slate` 7.32 already clear it, and the dark
theme clears every rule by a wide margin — its tightest pair is `indigo` at
6.42:1.

The rule the palette follows is what keeps it coherent, and it is stated in
`light.toml` so the next editor inherits it: *every value is a Tailwind ramp
step, and the step is the lightest one that clears its contrast rule on this
theme's background.* Before this change that rule was "600, uniformly", which
is a simpler sentence that the accessibility spec had already falsified for
seven of the hues. After it the light palette is 600 where 600 is legible and
700 where it is not, the two hue coincidences survive (`warning` = `amber`,
`success` = `emerald`), and no two tokens collide that did not collide before —
`info` is cyan-700 and `sky` is sky-700, still distinct, as `danger` red-600 and
`rose` rose-600 still are.

**Rejected: hand-mixed colours tuned to land just over 4.5.** They would clear
the gate and leave the palette with no rule behind it, which is how a design
language stops being one.

**Rejected: raising `bg` toward white to buy headroom.** It moves every ratio in
the theme at once, including the three that already pass, to avoid changing the
seven that do not.

## Consequences

- **`mise run tokens` can now fail on the content of the TOML, not just its
  shape.** The message names the file, the table, the token, the ratio measured
  and the ratio required, and every failure in a theme is reported together —
  darkening a background breaks several pairs at once, and finding that out one
  exception at a time is the slow way.
- **The measured ratio is printed rounded *down*.** Rounding to nearest would
  print "4.50:1, below the 4.5:1 required" for anything in `[4.495, 4.5)`, which
  reads as a bug in the gate rather than a fault in the palette.
- **Light `muted` at 4.90 is still the tightest passing surface pair**, and
  `rose` at 4.54 the tightest tint. Both are now asserted rather than merely
  true, so a future edit to `bg` fails loudly instead of silently spending the
  headroom. That was the closing warning in
  [#75](https://github.com/justin13888/Sunrise/issues/75).
- **Seven colours changed in four committed generated files.** They are inputs
  rather than reports (ADR-0029 §2), so the change is visible in `tokens.css`,
  `tokens.ts`, `tokens.swift` and `tokens.rs` and reaches Xcode and Vite without
  either running the generator.
- **No component was restyled.** The Swift call sites that read `Surface.*` and
  `StreamColor.tint` pick up the new values unchanged; the ones still drawing
  `Color.accentColor` are the migration ADR-0029 scoped out and are unaffected
  either way.
- **The exemption list is the thing to watch.** It is the only way a colour
  escapes measurement, so it is deliberately one entry long with its numbers
  written down. A second entry should be harder to add than a rule.

## What would force revisiting this

1. **`border` drawing a control boundary.** It inherits 1.4.11's 3:1 and needs a
   rule and a new value — decision 2's exemption is conditional and says so.
2. **A third theme, or a user-chosen background.** The rules are keyed on `bg`
   per theme, which survives a third theme unchanged, but a background the user
   picks makes contrast a runtime property and not a build-time one.
3. **A large-text token.** WCAG allows 3:1 for text at 18.66px bold or 24px
   regular. `type.toml` has the sizes, so a rule could be conditioned on one —
   but no token today is large-text-only, and a threshold that depends on how a
   colour is used is a rule the palette alone cannot check.
