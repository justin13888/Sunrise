/**
 * The TypeScript emitter.
 *
 * This target is not in the doc's original three (`css` / `swift` / `kt` /
 * `rs`). It is here because `packages/sunrise-ui`'s only consumer is
 * TypeScript, and a stylesheet cannot type `spacing.md` — see
 * `docs/11-adr/0029-design-token-pipeline.md`.
 *
 * Pure — `(tokens) => string`.
 */

import {
    BANNER,
    MOTION_CURVE_KEYS,
    num,
    RADIUS_KEYS,
    SPACE_KEYS,
    type Theme,
    type Tokens,
    TYPE_KEYS,
} from "./model";

/**
 * The task-state glyphs, carried over verbatim from the hand-written
 * `packages/sunrise-ui/src/tokens.ts`.
 *
 * They have no TOML source and will not get one. A glyph is UI copy, not a
 * shared value: the Apple clients draw SF Symbols here and the CLI prints its
 * own, so there is nothing for a Swift or Rust emitter to receive. They live in
 * this emitter so `@sunrise/ui`'s public surface stays one import.
 */
const TASK_STATE_GLYPH: ReadonlyArray<readonly [string, string]> = [
    ["todo", "[ ]"],
    ["in_progress", "[·]"],
    ["done", "[x]"],
    ["cancelled", "[/]"],
];

function scaleLiteral(
    name: string,
    keys: readonly string[],
    values: Readonly<Record<string, number>>,
): string[] {
    const lines = [`export const ${name} = {`];
    for (const key of keys) {
        lines.push(`    ${key}: ${num(values[key] as number)},`);
    }
    lines.push("} as const;");
    return lines;
}

function themeLiteral(theme: Theme, indent: string): string[] {
    const lines: string[] = [];
    lines.push(`${indent}surface: {`);
    for (const [key, value] of Object.entries(theme.surface)) {
        lines.push(`${indent}    ${key}: "${value}",`);
    }
    lines.push(`${indent}},`);
    lines.push(`${indent}stream: {`);
    for (const [key, value] of Object.entries(theme.stream)) {
        lines.push(`${indent}    ${key}: "${value}",`);
    }
    lines.push(`${indent}},`);
    return lines;
}

/** Render `tokens.ts`. */
export function emitTs(tokens: Tokens): string {
    const lines: string[] = [];
    lines.push("/**");
    for (const line of BANNER) {
        lines.push(line === "" ? " *" : ` * ${line}`);
    }
    lines.push(" */");
    lines.push("");

    lines.push("/** Spacing scale, in pixels. */");
    lines.push(...scaleLiteral("spacing", SPACE_KEYS, tokens.space));
    lines.push("");
    lines.push("/** Corner radii, in pixels. */");
    lines.push(...scaleLiteral("radii", RADIUS_KEYS, tokens.radius));
    lines.push("");
    // `typography`, not `type`: `import { type } from "@sunrise/ui-tokens"`
    // collides with TypeScript's type-only import specifier.
    lines.push(
        "/** Font sizes (px), numeric weights, and unitless line heights. */",
    );
    lines.push(...scaleLiteral("typography", TYPE_KEYS, tokens.type));
    lines.push("");

    lines.push(
        "/** Durations in milliseconds with their cubic-Bézier control points. */",
    );
    lines.push("export const motion = {");
    for (const key of MOTION_CURVE_KEYS) {
        const value = tokens.motion.curves[key];
        lines.push(`    ${key}: {`);
        lines.push(`        durationMs: ${num(value.durationMs)},`);
        lines.push(`        easing: [${value.easing.map(num).join(", ")}],`);
        lines.push("    },");
    }
    lines.push("} as const;");
    lines.push("");
    lines.push(
        "/** What every duration collapses to under a reduced-motion preference. */",
    );
    lines.push(
        `export const reducedMotionDurationMs = ${num(tokens.motion.reducedDurationMs)};`,
    );
    lines.push("");

    lines.push(
        "/** The two themes. `surface` is semantic; `stream` is keyed on `StreamColor`. */",
    );
    lines.push("export const color = {");
    lines.push("    light: {");
    lines.push(...themeLiteral(tokens.light, "        "));
    lines.push("    },");
    lines.push("    dark: {");
    lines.push(...themeLiteral(tokens.dark, "        "));
    lines.push("    },");
    lines.push("} as const;");
    lines.push("");

    lines.push(
        "/** The eight `StreamColor` variant names, in declaration order. */",
    );
    lines.push("export const streamColors = [");
    for (const key of tokens.streamKeys) {
        lines.push(`    "${key}",`);
    }
    lines.push("] as const;");
    lines.push("");

    lines.push(
        "/** ASCII task-state glyphs. Not a TOML token — see `src/emit-ts.ts`. */",
    );
    lines.push("export const taskStateGlyph = {");
    for (const [key, glyph] of TASK_STATE_GLYPH) {
        lines.push(`    ${key}: "${glyph}",`);
    }
    lines.push("} as const;");
    lines.push("");
    return lines.join("\n");
}
