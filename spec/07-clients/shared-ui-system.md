---
status: accepted
---

# Shared UI System

A pragmatic cross-platform design system: shared *tokens* and *patterns*; per-platform *components*.

## Tokens

Tokens live in `packages/sunrise-ui-tokens/tokens/`. A build script at `packages/sunrise-ui-tokens/build.ts` emits per-target outputs (`tokens.css`, `tokens.swift`, `tokens.kt`, `tokens.rs`). The TOML files below are the implementation source of truth.

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

- Mobile and desktop use the OS system font (SF on Apple, Roboto on Android).
- Web uses `system-ui` / `Inter` fallback.
- TUI uses the terminal's font (we control sizing only via cell counts).

## Four-state view contract

Every view MUST implement four states. This is the canonical table; per-view files reference this section rather than duplicating it.

| State | Trigger | Visual | Action |
|---|---|---|---|
| `loading` | Initial vault read or async fetch in flight > 200 ms | Skeleton placeholder of 3 list rows; no spinner unless > 1 s, then small inline spinner; no modal. | None auto; user can navigate away. |
| `empty` | View has no entities to render after load. | Centered illustration glyph + 1-line copy + 1 primary action button (e.g. "Capture your first task"). Copy is per-view from `i18n` table `view.<name>.empty.*`. | Primary action triggers the view's main affordance. |
| `error` | Async load failed, or sync session error blocks data. | Inline banner at top of view: icon + 1-line `error.<ErrorCode>.title` + 1 retry button. View renders cached/stale data below if available. | Retry re-runs the failed operation. |
| `conflict` | Merge applied a conflict-resolution rule the user might want to review. | Toast notification (5 s) + entry in Reviews → Recent Conflicts. | Tap toast → opens conflict-detail view. |

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

Per-view illustrations live in `packages/sunrise-ui-shared/illustrations/`.

## Pattern catalog

Patterns are described once and implemented natively per platform:

| Pattern | Behavior |
|---|---|
| **Quick capture sheet** | Single text field; `Esc` cancels; `Enter` commits and clears; modifier `Cmd/Ctrl+Enter` commits and closes |
| **Task row** | Checkbox + title + meta row (stream tint, due/scheduled, contexts); right-side actions on hover/long-press |
| **Stream chip** | Color dot + name; consistent across all surfaces |
| **Today header** | Date + day-of-week + a count summary |
| **Detail pane** | Slides in from trailing edge; never modal blocking; closes with `Esc` |
| **Empty state** | See "Four-state view contract" above |

## Component implementations

| Pattern | Desktop | iOS | Android | Web | TUI |
|---|---|---|---|---|---|
| Task row | React `<TaskRow>` | SwiftUI `TaskRow` | Compose `TaskRow()` | React `<TaskRow>` | Custom Ratatui widget |
| Quick capture | Native window | Sheet | BottomSheet | Modal | Inline prompt |
| Detail pane | Sliding panel | NavigationStack push | NavigationCompose push | Sliding panel | Side pane |

## Density

Three densities: `comfortable`, `default`, `compact`. User-selectable. Defaults:

- Desktop: `default`.
- iOS / Android: `comfortable`.
- Web: `default`.
- TUI: `compact` always (terminal lines).

## Iconography

A small custom icon set (~40 icons) shipped as SVG → rendered platform-native:

- iOS: SF Symbols where available, custom otherwise.
- Android: Material symbols where available.
- Desktop: identical SVG renderer.
- Web: same SVG.
- TUI: Unicode glyphs (graceful fallback if terminal lacks support).

## Accessibility

- Every interactive element has a programmatic name.
- Color is never the *only* signal (icons + text accompany).
- All flows reachable by keyboard / VoiceOver / TalkBack.
- See [`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md).

## What is not shared

- The actual rendering library. Native components, native idioms.
- Animation choices beyond motion tokens. iOS feels like iOS.
- Navigation transitions. Each platform owns its idiom.
