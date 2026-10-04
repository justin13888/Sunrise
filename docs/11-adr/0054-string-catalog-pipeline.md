# 0054 — User-visible strings are compiled from one TOML catalog into committed bindings, in a MessageFormat subset every client renders the same way

**Status:** accepted

**Amends** [`../10-cross-cutting/i18n.md`](../10-cross-cutting/i18n.md)
§String catalog and §Plural-rule test coverage: it names the generator, the
message subset, the key namespace, and what the plural matrix checks for a
locale that has no translation yet. Kotlin bindings are deferred with the
Android client.

**Follows** [ADR-0029](./0029-design-token-pipeline.md), whose pipeline
shape — TOML in, committed generated files out, drift-gated — this adopts
for strings.

**Tracked by** [#12](https://github.com/justin13888/Sunrise/issues/12).

## Context

`docs/10-cross-cutting/i18n.md` has specified a string catalog since it was
written — one `i18n/en.toml`, ICU MessageFormat with CLDR plurals, bindings
generated into Swift, Kotlin and TypeScript at build time, and a CI matrix over
`en`, `pl` and `ar` in which a missing plural arm fails the build. None of it
existed. Every user-visible string in the Apple apps, the web client and the
CLI was a hard-coded English literal, and at least two of them —
`"\(count) other device(s)"` in `DeviceListSection.swift` and
`"{} other device(s)"` in the CLI — were the "1 task / 3 tasks" bug the spec
names.

Building it forces decisions the spec leaves open. They are recorded here.

## Decision

### 1. One generator, in TypeScript, emitting four committed files

`packages/sunrise-i18n/build.ts` (`mise run i18n`) reads `i18n/*.toml` and
writes, under `packages/sunrise-i18n/generated/`:

| File | Consumer | Carries surfaces |
|---|---|---|
| `messages.ts` | `apps/web`, `apps/docs` | `common`, `web`, `docs` |
| `Strings.swift` | both Apple apps (typed `L10n` accessors) | `common`, `apple` |
| `Localizable.xcstrings` | both Apple apps (the messages) | `common`, `apple` |
| `strings.rs` | `sunrise-cli`, by `include!` | `common`, `cli` |

The files are committed for ADR-0029 §2's reason: Xcode's environment has no
Bun, and a `cargo build` of the CLI should not need one. Three gates overlap,
as the tokens' do: `packages/sunrise-i18n/test/emit.test.ts` (vitest, on
`pre-push`), `mise run i18n-check`, and the `i18n-current` CI job.

**Rejected: a Rust generator.** The catalog's checks read CLDR through
`Intl.PluralRules`, and the TypeScript binding interprets the same parts the
generator produced; a Rust generator would have put a second CLDR reading, and
a second MessageFormat parser, beside the first. **Rejected: a `build.rs` in
the CLI parsing the TOML.** It would be a second parser, and the Apple build
could not use it.

### 2. The message subset is what a String Catalog can say

Values are ICU MessageFormat, parsed by `@formatjs/icu-messageformat-parser`,
restricted to: text, `{name}`, `{name, number}`, and `{name, plural, …}` over
an integer with the six CLDR category arms, an optional `=0` arm, and `#`.
`select`, `selectordinal`, `date`, `time`, number styles, `offset:`, exact arms
other than `=0`, and a plural nested in a plural arm are refused with a
message naming the construct.

Each refusal is a real ICU feature that Apple's String Catalog has no
equivalent for. The alternative — a Swift runtime formatter instead of the
catalog — would need CLDR plural rules in Swift, which Foundation does not
expose, so it would mean shipping a rules table Foundation already has. A
message that renders one way on the web and another on the Mac is worse than
one that cannot be written, so the subset is the intersection.

`=0` becomes the String Catalog's `zero` variation, which Foundation applies to
0 in every language. In a locale whose CLDR rules already have `zero` (Arabic,
Latvian) the two would collide, so there a translation writes the `zero` arm
and `=0` is refused.

**Reverses when:** a message needs a refused construct. Adding one is a
parser case, a runtime case and an emitter case per binding, and the Apple
emitter decides whether it is possible at all.

### 3. The first key segment is the surface

`[web.app] title = …` is `web.app.title`. The first segment is one of
`common`, `web`, `docs`, `apple`, `cli`, and decides which bindings carry the
key (the table above), so the web bundle does not ship the CLI's prose and the
Apple catalog does not hold the web's. Segments are snake_case with every `_`
followed by a letter, which keeps the camelCase and PascalCase spellings the
bindings derive one-to-one with the key.

### 4. What the plural matrix checks

A matrix locale is checked against its own catalog when it has one. When it
does not — `pl` and `ar` today — it is checked against the *template* a
translator into it would start from: every source message with its plurals
reshaped to that locale's categories, each seeded from the source's `other`.
Either way every message must

1. compile for the locale: parse under the subset, with exactly the locale's
   categories as arms — a missing arm (ICU would render `other`, the "3 file"
   bug) and a dead arm (English `zero`) are both failures; and
2. render for the locale: for every category an integer reaches, the category's
   smallest sample count must select that category's arm through the runtime
   the web bundle ships.

Checking the English source against Polish categories directly was rejected:
it would require English messages to carry `few` and `many` arms English never
selects, which the dead-arm rule rightly calls noise. A translation that falls
back to English is formatted by English rules, because English arms are the
only ones it has — the same thing Foundation does with a missing localization.

### 5. The CLI's runtime is ICU4X; its locale comes from POSIX variables

`crates/sunrise-cli/src/i18n.rs` supplies the generated code's three calls —
the catalog locale, a CLDR cardinal category, an integer in locale digits —
from `icu_plurals` and `icu_decimal` with compiled data, which is the spec's
"CLDR via ICU4X". The locale is negotiated once from `LC_ALL`, `LC_MESSAGES`
and `LANG`, in POSIX precedence, against the catalogs present.

Only prose a person reads goes through the catalog. A line a script reads —
`sunrise vaults`, an id, a JSON document — is a record, not a message.

### 6. The docs site is VitePress over `docs/`, unchanged

`apps/docs` builds `docs/` with VitePress. It is Vite-based like `apps/web`,
has locale routing built in (a locale is a `docs/<locale>/` tree plus an entry
in `locales`), and reads its own chrome from the catalog's `docs` surface.
`docs/` is not moved or edited for it: `README.md` pages are rewritten to
directory indexes, links out of `docs/` are pointed at GitHub, raw HTML is off
(the docs use none, and `<id>` in prose is a placeholder), and a `{{` is text.
Dead links fail the build.

**Rejected: mdBook.** It has no locale routing, and a translated book would be
a second book. **Rejected: Starlight (Astro).** It would bring a second
bundler beside Vite for one site.

### 7. Right-to-left is a direction on the root, and layout is logical

Each binding exposes the negotiated locale's direction (`dir` in
`messages.ts`; the Apple apps inherit it from the system). The web client sets
`<html lang dir>` at load, and the layout it writes uses logical properties
(`paddingInline`, not `paddingLeft`), so a right-to-left locale mirrors it.

## Consequences

- **Kotlin is not emitted.** There is no Android target to compile it. It
  lands with the Android client, as `tokens.kt` does.
- **Most strings are still literals.** This lands the pipeline and moves the
  web client, the Apple device list, and the CLI's account, sync, focus and
  recovery prose onto it. The remaining Apple views and CLI output are
  [#378](https://github.com/justin13888/Sunrise/issues/378).
- **A translation is a TOML file.** An `i18n/<locale>.toml` — Polish would be
  `pl.toml` — with any subset of the keys is a localization in all three
  bindings after `mise run i18n`; the
  matrix then checks it against its own messages. Which languages ship stays
  deferred, per the spec.
- **Editing `i18n/en.toml` changes `strings.rs`,** which the CLI `include!`s.
  The log-field gate (`crates/sunrise-log/tests/event_catalog.rs`) then wants
  a rebuild before it runs, as it does after any source edit; the generator
  rewrites a file only when its bytes change, so a no-op regeneration does not
  trigger it.
