/**
 * Shared design tokens for Sunrise client UIs.
 *
 * Every value here is generated from `packages/sunrise-ui-tokens/tokens/*.toml`
 * by `mise run tokens`; this file only decides what `@sunrise/ui` calls them.
 * Editing a colour means editing the TOML — `mise run tokens-check` and
 * `packages/sunrise-ui-tokens/test/drift.test.ts` both fail otherwise.
 *
 * The stream palette is keyed on `StreamColor` in
 * `crates/sunrise-domain/src/stream.rs`. That used to be a comment asking a
 * reader to keep two hand-written lists in step; it is now
 * `packages/sunrise-ui-tokens/test/invariants.test.ts`, which reads the Rust
 * and fails when the two diverge.
 *
 * `spacing` is the six-step scale, so `md` is 12 and not 16 — see
 * `docs/11-adr/0029-design-token-pipeline.md` for why the doc's scale won over
 * the five-step one this file used to carry.
 */

import { color } from "@sunrise/ui-tokens";

/**
 * Spacing (px), corner radii (px), type, motion, and the ASCII task glyphs,
 * straight off the generated set.
 */
export {
    motion,
    radii,
    spacing,
    taskStateGlyph,
    typography,
} from "@sunrise/ui-tokens";

/**
 * The eight Stream tints, light theme.
 *
 * Light-only because that is what `colors` has always meant to its consumer.
 * `surface.dark.stream` is the dark set; a client that renders both reads
 * `surface` instead.
 */
export const colors = color.light.stream;

/** A Stream's colour name — the `StreamColor` variants, exactly. */
export type StreamColor = keyof typeof colors;

/**
 * The semantic surface palette, per theme.
 *
 * Entirely new: there was no `bg`, `fg`, `muted`, `accent`, `border`, `danger`,
 * `warning`, `success` or `info` here before, and no dark theme at all.
 */
export const surface = color;
