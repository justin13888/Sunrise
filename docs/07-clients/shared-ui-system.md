---
status: accepted
---

# Shared UI System

A pragmatic cross-platform design system: shared *tokens* and *patterns*; per-platform *components*.

## Tokens

> **Status: one 40-line TypeScript file, consumed by the deferred web app and
> nothing else.** The token pipeline below — `packages/sunrise-ui-tokens/`, its
> TOML sources, and a `build.ts` emitting `tokens.css` / `.swift` / `.kt` /
> `.rs` — was specified here and never built. No such directory, script or
> generated file exists. The colour, motion, typography and radius tables under
> [Specified, not built](#tokens-specified-not-built) are kept because they are
> still the intended palette; they are not describing anything that runs.
> [#29](https://github.com/justin13888/Sunrise/issues/29) tracks the gap.

What exists is `packages/sunrise-ui/src/tokens.ts`, hand-written, re-exported
verbatim by `src/index.ts`, and holding four constants:

| Export | Contents |
|---|---|
| `colors` | The eight `StreamColor` tints, as hex: `slate` `#475569`, `rose` `#e11d48`, `amber` `#d97706`, `emerald` `#059669`, `sky` `#0284c7`, `indigo` `#4f46e5`, `violet` `#7c3aed`, `pink` `#db2777` |
| `spacing` | `xs` 4, `sm` 8, `md` 16, `lg` 24, `xl` 32 |
| `radii` | `sm` 4, `md` 8, `lg` 12 |
| `taskStateGlyph` | `todo` `[ ]`, `in_progress` `[·]`, `done` `[x]`, `cancelled` `[/]` |

There is no surface palette — no `bg`, `fg`, `muted`, `accent`, `border`,
`danger`, `warning`, `success` or `info`, and no dark set. There are no motion
or typography tokens. `spacing` has five steps and not six: the `md = 12` and
`lg = 16` the table below specifies are `md = 16` and `lg = 24` in the file,
with no `xxl`, so a client following the specified scale and a client importing
the real one disagree at every step above `sm`.

The `colors` keys are the file's own `StreamColor` type and mirror
`StreamColor` in `crates/sunrise-domain/src/stream.rs`, which the file's header
comment names as the thing to stay in sync with. Nothing enforces that: the
Rust enum and the TypeScript object are two hand-maintained lists.

**The package has one consumer and it is deferred.** `apps/web` imports
`taskStateGlyph` and nothing else — not `colors`, not `spacing`, not `radii` —
and `apps/web` is the `localStorage` stub [ADR-0012](../11-adr/0012-web-wasm-deferred.md)
left in place of a real client. The Apple apps are Swift and consume none of it;
they carry their own values. So no shipping client reads a shared token today,
which is why the drift above has cost nothing yet and why it will cost
something the moment a second consumer appears.

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

Per-view illustrations were specified as living in `packages/sunrise-ui-shared/illustrations/`. **That package does not exist**, and neither does any shared illustration asset; the only package under `packages/` that any client imports is `packages/sunrise-ui`, described under [Tokens](#tokens). Each client draws its own empty state ([#29](https://github.com/justin13888/Sunrise/issues/29)).

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

## Tokens (specified, not built)

> **None of this section exists.** No `packages/sunrise-ui-tokens/` directory,
> no TOML sources, no `build.ts`, and no generated `tokens.css` / `tokens.swift`
> / `tokens.kt` / `tokens.rs`. It is the intended design, kept as such.
> [#29](https://github.com/justin13888/Sunrise/issues/29) tracks it. What ships
> is described under [Tokens](#tokens) above, and where the two disagree — the
> spacing scale in particular — the file is what a build actually gets.

Tokens live in `packages/sunrise-ui-tokens/tokens/`. A build script at
`packages/sunrise-ui-tokens/build.ts` emits per-target outputs (`tokens.css`,
`tokens.swift`, `tokens.kt`, `tokens.rs`). The TOML files below are the intended
source of truth.

```
sunrise-tokens/
├── color/
│   ├── light.toml
│   └── dark.toml
├── motion.toml
├── spacing.toml
├── radius.toml
└── type.toml
```

### Color (v1 values)

```toml
# color/light.toml
[surface]
bg          = "#FBFBFA"
fg          = "#1A1A1A"
muted       = "#6E6E73"
accent      = "#2563EB"
accent_text = "#FFFFFF"
border      = "#E5E5E7"
danger      = "#DC2626"
warning     = "#D97706"
success     = "#059669"
info        = "#0891B2"
```

```toml
# color/dark.toml — symmetrical, with brightness inversions
bg          = "#0F0F10"
fg          = "#FAFAFA"
muted       = "#9CA3AF"
accent      = "#60A5FA"
accent_text = "#0F0F10"
border      = "#27272A"
danger      = "#F87171"
warning     = "#FBBF24"
success     = "#34D399"
info        = "#22D3EE"
```

Color is semantic, not raw. Stream tints (`stream-1..stream-12`) palette is generated from `accent` per the build script. No hardcoded hex anywhere in client code. Light + dark modes; AA contrast minimum on text vs surface (AAA where feasible).

### Motion

```toml
# motion.toml
fast   = { duration_ms = 120, easing = "cubic-bezier(0.2, 0, 0, 1)" }   # out
med    = { duration_ms = 220, easing = "cubic-bezier(0.4, 0, 0.2, 1)" } # inout
slow   = { duration_ms = 360, easing = "cubic-bezier(0.4, 0, 0.2, 1)" }
linear = { easing = "linear" }
```

Reduced-motion preference disables transitions altogether.

### Spacing (4 px base)

```toml
# spacing.toml
xs  = 4
sm  = 8
md  = 12
lg  = 16
xl  = 24
xxl = 32
```

Avoid one-off pixel values.

### Radius

```toml
# radius.toml
sm   = 4
md   = 8
lg   = 12
pill = 9999
```

### Typography

```toml
# type.toml — Inter base; SF on Apple, Roboto on Android
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
