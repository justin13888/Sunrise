---
status: accepted
---

# Shared UI System

A pragmatic cross-platform design system: shared *tokens* and *patterns*; per-platform *components*.

## Tokens

Tokens live in `packages/sunrise-ui-tokens/tokens/` as TOML, and
`packages/sunrise-ui-tokens/build.ts` compiles them into one file per target:

```
packages/sunrise-ui-tokens/
├── tokens/
│   ├── color/
│   │   ├── light.toml
│   │   └── dark.toml
│   ├── motion.toml
│   ├── spacing.toml
│   ├── radius.toml
│   └── type.toml
├── build.ts
└── generated/
    ├── tokens.css      → apps/web, via `import "@sunrise/ui-tokens/css"`
    ├── tokens.ts       → packages/sunrise-ui re-exports it
    ├── tokens.swift    → both Apple app targets compile it
    └── tokens.rs       → no consumer yet; see below
```

Run `mise run tokens` after editing any TOML file. **The generated files are
committed**, because the two builds that need them most cannot produce them:
Xcode has neither Bun nor mise on `PATH`. Three gates keep the committed files
honest — `packages/sunrise-ui-tokens/test/drift.test.ts` (which `mise run test`
runs, and `lefthook.yaml` runs on pre-push), `mise run tokens-check` (which
additionally compiles and rustfmt-checks `tokens.rs`), and a `tokens-current`
CI job. [ADR-0029](../11-adr/0029-design-token-pipeline.md) records why the
outputs are committed rather than generated at build time, why `tokens.rs` is
an `include!`-ready file rather than a crate, and why the spacing scale below
supersedes the one `packages/sunrise-ui` used to carry.

**`tokens.kt` is not emitted.** There is no Android target to compile it and no
Kotlin consumer to read it; it lands with the Android client, not before.

### Consumers

| Target | What reads it |
|---|---|
| Web | `apps/web/src/main.tsx` imports `@sunrise/ui-tokens/css`, so the custom properties and both media queries are in the bundle. `packages/sunrise-ui` re-exports the typed object as `colors`, `spacing`, `radii`, `typography`, `motion`, `surface` and `taskStateGlyph` |
| macOS / iOS | `apps/apple/project.yml` adds `tokens.swift` to both app targets. `apps/apple/Sunrise/Design/Tokens.swift` is the hand-written adapter: it resolves light/dark through `@Environment(\.colorScheme)` and Reduce Motion through `@Environment(\.accessibilityReduceMotion)`, and it is the one place a `StreamColor` becomes a `Color` |
| Rust | Nothing, yet. `tokens.rs` is an `include!`-ready const module — the CLI is specified as plain text with no colour ([`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)), and the shared core does not decide presentation ([`../01-architecture/shared-core.md`](../01-architecture/shared-core.md)), so there is no consumer to write. It is compiled and rustfmt-checked by `mise run tokens-check` so the first one inherits a working file |

### Color

Colour is semantic, not raw: a client asks for `accent`, never for a blue.
Light and dark carry identical key sets, and
`packages/sunrise-ui-tokens/test/invariants.test.ts` fails if they stop doing
so.

```toml
# tokens/color/light.toml
[surface]
bg          = "#fbfbfa"
fg          = "#1a1a1a"
muted       = "#6e6e73"
accent      = "#2563eb"
accent_text = "#ffffff"
border      = "#e5e5e7"
danger      = "#dc2626"
warning     = "#d97706"
success     = "#059669"
info        = "#0891b2"
```

```toml
# tokens/color/dark.toml — symmetrical, with brightness inversions
[surface]
bg          = "#0f0f10"
fg          = "#fafafa"
muted       = "#9ca3af"
accent      = "#60a5fa"
accent_text = "#0f0f10"
border      = "#27272a"
danger      = "#f87171"
warning     = "#fbbf24"
success     = "#34d399"
info        = "#22d3ee"
```

Contrast is asserted rather than intended: body text on background clears AAA
(7:1), and both muted text and `accent_text`-on-`accent` clear AA (4.5:1), in
both themes — which is what makes
[`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)
§Color and contrast a test rather than a wish.

**Stream tints are eight named values, not a ramp generated from `accent`.**
This supersedes the earlier `stream-1..stream-12` specification. `StreamColor`
in `crates/sunrise-domain/src/stream.rs` is a serde-stable eight-variant enum
whose lowercase names are persisted in the vault and read back by a lossy
parser, so renumbering the palette would be a storage-format change rather than
a design change. The `[stream]` table in each theme is keyed on those names,
and `test/invariants.test.ts` reads `StreamColor::as_str` out of the Rust and
fails when the two lists diverge:

```toml
# tokens/color/light.toml
[stream]
slate   = "#475569"
rose    = "#e11d48"
amber   = "#d97706"
emerald = "#059669"
sky     = "#0284c7"
indigo  = "#4f46e5"
violet  = "#7c3aed"
pink    = "#db2777"
```

The dark set is the same eight names, lifted: `#94a3b8`, `#fb7185`, `#fbbf24`,
`#34d399`, `#38bdf8`, `#818cf8`, `#a78bfa`, `#f472b6`.

### Motion

```toml
# tokens/motion.toml
[fast]
duration_ms = 120
easing = [0.2, 0.0, 0.0, 1.0]   # out
```

`med` is 220 ms and `slow` 360 ms, both on `[0.4, 0.0, 0.2, 1.0]` (in-out);
`linear` is 0 ms on `[0.0, 0.0, 1.0, 1.0]`.

Easing is four cubic-Bézier control points rather than a `cubic-bezier(...)`
string, so that only the CSS emitter has to know CSS. `[reduced]` is the
no-motion policy — `duration_ms = 0`, enforced by the loader — and it is what
every other duration collapses to: the emitted CSS carries a
`prefers-reduced-motion: reduce` block, and the Swift adapter returns `nil`
instead of an `Animation`.

### Spacing (4 px base)

```toml
# tokens/spacing.toml
xs  = 4
sm  = 8
md  = 12
lg  = 16
xl  = 24
xxl = 32
```

Six steps, and this is the scale that won: `packages/sunrise-ui/src/tokens.ts`
carried a five-step one (`md` 16, `lg` 24, `xl` 32, no `xxl`) and now
re-exports this instead. Avoid one-off pixel values.

### Radius

```toml
# tokens/radius.toml
sm   = 4
md   = 8
lg   = 12
pill = 9999
```

### Typography

```toml
# tokens/type.toml — Inter base; SF on Apple, Roboto on Android
size_xs   = 11
size_sm   = 13
size_base = 15
size_lg   = 17
size_xl   = 22
size_2xl  = 28
weight_regular  = 400
weight_medium   = 500
weight_semibold = 600
weight_bold     = 700
line_tight  = 1.25
line_normal = 1.5
```

- macOS uses the OS system font (SF).
- Web uses `system-ui` / `Inter` fallback.
- No font *family* is a token: each platform uses its own system face, which is
  a rendering choice rather than a shared value.

## Three-state view contract

Every view MUST implement three states. This is the canonical table; per-view files reference this section rather than duplicating it.

| State | Trigger | Visual | Action |
|---|---|---|---|
| `loading` | Initial vault read or async fetch in flight > 200 ms | Skeleton placeholder of 3 list rows; no spinner unless > 1 s, then small inline spinner; no modal. | None auto; user can navigate away. |
| `empty` | View has no entities to render after load. | Centered illustration glyph + 1-line copy + 1 primary action button (e.g. "Capture your first task"). Copy is per-view from `i18n` table `view.<name>.empty.*`. | Primary action triggers the view's main affordance. |
| `error` | Async load failed, or sync session error blocks data. | Inline banner at top of view: icon + 1-line `error.<ErrorCode>.title` + 1 retry button. View renders cached/stale data below if available. | Retry re-runs the failed operation. |

**There is no `conflict` state.** Its data source was asserted absent by
[`../05-sync/conflict-resolution.md`](../05-sync/conflict-resolution.md)
§Merge journal — removed: `merge_journal` was dropped by
[ADR-0018](../11-adr/0018-storage-baseline-reset.md), and
`baseline_omits_the_dead_schema` in `crates/sunrise-storage/src/db.rs` fails if
it returns. Nothing records that a concurrent edit lost, so a view has nothing to
raise a toast about. This is not an omission to be filled in later by the UI: the
state cannot exist until a journal does, and reinstating one needs an ADR
superseding 0018's removal. [ADR-0014](../11-adr/0014-entity-level-lww-merge.md)
§What would force revisiting this, trigger 2 (per-field merge becoming
user-visible), is the likeliest place both come back together.

### Per-view empty-state copy

| View | Empty copy | Empty action |
|---|---|---|
| Today | "Nothing scheduled for today. Add a task or take it easy." | "Add a task" |
| Inbox | "Inbox zero. Capture something quickly with ⌘N." | "Capture" |
| Stream | "No tasks in <stream> yet." | "Add task" |
| Calendar | "No blocks for this week." | "Add block" |
| Search | "No results for '<query>'." | "Clear search" |
| Reviews | "Not enough data yet — come back in a week." | (none) |
| Focus | (focus mode never empty; uses idle screen) | — |

Per-view illustrations were specified as living in `packages/sunrise-ui-shared/illustrations/`. **That package still does not exist**, and neither does any shared illustration asset — the token pipeline under [Tokens](#tokens) closed the other half of [#29](https://github.com/justin13888/Sunrise/issues/29) and left this one open. `packages/` holds `sunrise-ui-tokens` and `sunrise-ui`, and neither carries an image. Each client draws its own empty state.

## Pattern catalog

Patterns are described once and implemented natively per platform:

| Pattern | Behavior |
|---|---|
| **Quick capture sheet** | Single text field; `Esc` cancels; `Enter` commits and clears; modifier `Cmd/Ctrl+Enter` commits and closes |
| **Task row** | Checkbox + title + meta row (stream tint, due/scheduled, contexts); right-side actions on hover/long-press |
| **Stream chip** | Color dot + name; consistent across all surfaces |
| **Today header** | Date + day-of-week + a count summary |
| **Detail pane** | Slides in from trailing edge; never modal blocking; closes with `Esc` |
| **Empty state** | See "Three-state view contract" above |

## Component implementations

**macOS and iOS both ship** ([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)), and their two
columns are very largely the same source file rather than two implementations
that agree. Android and Web keep their rows so the shape is recorded for when
they are scheduled; nothing behind those two columns exists.

| Pattern | macOS | iOS | Android *(deferred)* | Web *(deferred)* |
|---|---|---|---|---|
| Task row | SwiftUI `TaskRowView` | SwiftUI `TaskRowView` — literally the Mac's | Compose `TaskRow()` | React `<TaskRow>` |
| Quick capture | Borderless `NSPanel` | Sheet, `.presentationDetents([.height(280), .medium])` | BottomSheet | Modal |
| Detail pane | Sliding panel | `NavigationStack` push | NavigationCompose push | Sliding panel |

## Density

Three densities: `comfortable`, `default`, `compact`. User-selectable. Defaults:

- macOS: `default`.
- iOS / Android: `comfortable`.
- Web: `default`.

## Iconography

A small custom icon set (~40 icons) shipped as SVG → rendered platform-native:

- macOS / iOS: SF Symbols where available, custom otherwise.
- Android: Material symbols where available.
- Web: same SVG.

## Accessibility

- Every interactive element has a programmatic name.
- Color is never the *only* signal (icons + text accompany).
- All flows reachable by keyboard / VoiceOver / TalkBack.
- See [`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md).

## What is not shared

- The actual rendering library. Native components, native idioms.
- Animation choices beyond motion tokens. iOS feels like iOS.
- Navigation transitions. Each platform owns its idiom.
