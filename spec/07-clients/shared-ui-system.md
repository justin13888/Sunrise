---
status: accepted
---

# Shared UI System

A pragmatic cross-platform design system: shared *tokens* and *patterns*; per-platform *components*.

## Tokens

A single TOML/JSON spec is the source of truth, generated into Swift, Kotlin, Rust, and CSS:

```
sunrise-tokens/
├── color/
│   ├── light.toml
│   └── dark.toml
├── typography.toml
├── spacing.toml
├── radius.toml
├── motion.toml
└── elevation.toml
```

### Color

- Semantic, not raw. `surface`, `surface-muted`, `text`, `text-muted`, `accent`, `success`, `warn`, `danger`, `stream-1..stream-12` (palette for stream tints).
- Light + dark modes; AAA contrast on text vs surface.
- No hardcoded hex anywhere in client code.

### Typography

- Mobile and desktop use the OS system font.
- Web uses `system-ui` / `Inter` fallback.
- TUI uses the terminal's font (we control sizing only via cell counts).
- Five sizes: caption, body, body-strong, title, display. No more.

### Spacing

8px grid baseline. Tokens: `xs, s, m, l, xl, xxl`. Avoid one-off pixel values.

### Radius

`sharp, soft, pill`. Per platform default: macOS soft, Windows sharp, iOS soft, Android softer, Web matches platform via media queries.

### Motion

Three durations (`fast`, `med`, `slow`) and three easings (`linear`, `out`, `inout`). Reduced motion preference disables transitions altogether.

## Pattern catalog

Patterns are described once and implemented natively per platform:

| Pattern | Behavior |
|---|---|
| **Quick capture sheet** | Single text field; `Esc` cancels; `Enter` commits and clears; modifier `Cmd/Ctrl+Enter` commits and closes |
| **Task row** | Checkbox + title + meta row (stream tint, due/scheduled, contexts); right-side actions on hover/long-press |
| **Stream chip** | Color dot + name; consistent across all surfaces |
| **Today header** | Date + day-of-week + a count summary |
| **Detail pane** | Slides in from trailing edge; never modal blocking; closes with `Esc` |
| **Empty state** | Concise prose + one primary action |

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
