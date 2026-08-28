# 0006 — TUI built with Ratatui

**Status:** superseded by [0019](./0019-swiftui-macos-client.md)

> **Superseded.** There is no TUI. [ADR-0019](./0019-swiftui-macos-client.md)
> replaced the terminal client with a native SwiftUI macOS app over a UniFFI
> seam, and `crates/sunrise-tui` was deleted; `ratatui`, `crossterm`,
> `ratatui-image` and `image` left `[workspace.dependencies]` with it. The
> headless subcommands survive as `sunrise-cli`. Kept for the record of why
> Ratatui was the right choice *for a TUI* — that reasoning is unchanged, and
> would apply again if a terminal client is ever revived.

## Context

The TUI is a first-class client. We need a Rust TUI library with mature widgets, async-friendly event handling, broad terminal compatibility, and a healthy ecosystem.

## Decision

Use **Ratatui** with **Crossterm** as the backend.

## Alternatives considered

| Option | Pros | Cons |
|---|---|---|
| Cursive | Easier widgets | Heavier abstractions; less idiomatic for our async needs |
| Termwiz | Powerful low-level | Smaller ecosystem; widget set is thin |
| **Ratatui** | Active community; rich widgets; pairs well with Crossterm; popular blueprint apps available | Stateful patterns require some care |
| Re-implement on top of raw escape codes | Maximum control | Reinventing wheels; testing nightmare |

## Consequences

- We adopt the Ratatui idiom (immediate-mode rendering with state struct between frames).
- Async event loop via Tokio + Crossterm event stream.
- Snapshot tests for rendering: keep golden frames per scenario; CI diffs.
- Plugin opportunity: rich custom widgets for our specific layouts (calendar grid, focus mode).

## Risks

- Ratatui's API is still evolving; pin versions and review changelogs.
- Some terminals (older Windows, restricted SSH multiplexers) may not support all the glyphs we want; degrade gracefully (ASCII fallback paths in renderers).
