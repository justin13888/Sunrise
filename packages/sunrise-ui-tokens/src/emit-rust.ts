/**
 * The Rust emitter.
 *
 * The output is a flat const module, not a crate and not a module inside one —
 * `docs/11-adr/0029-design-token-pipeline.md` has the argument. It is written
 * to be `include!`-ready, which is a real constraint on the shape: an
 * `include!` expands into an existing module body, and an inner doc comment
 * (`//!`) in expansion position is a hard parse error (`E0753`). So the file
 * banner is `//` line comments and every item carries an outer `///`.
 *
 * Nothing here needs `std`: the module is primitives and arrays only, so it
 * compiles inside a `#![no_std]` crate unchanged.
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

/** Rust rejects `0` where an `f32` is expected, so floats always carry a point. */
function float(value: number): string {
    return Number.isInteger(value) ? `${num(value)}.0` : num(value);
}

/** `#rrggbb` as a `[u8; 3]` literal, in the same hex the source declares. */
function rgb(hex: string): string {
    const component = (at: number) => `0x${hex.slice(at, at + 2)}`;
    return `[${component(1)}, ${component(3)}, ${component(5)}]`;
}

function themeConsts(prefix: string, theme: Theme, what: string): string[] {
    const lines: string[] = [];
    for (const [key, value] of Object.entries(theme.surface)) {
        lines.push(`/// ${what} \`${key}\`, as sRGB \`${value}\`.`);
        lines.push(
            `pub const ${prefix}_${key.toUpperCase()}: [u8; 3] = ${rgb(value)};`,
        );
    }
    return lines;
}

function streamConsts(prefix: string, theme: Theme, what: string): string[] {
    const lines: string[] = [];
    for (const [key, value] of Object.entries(theme.stream)) {
        lines.push(
            `/// ${what} tint for \`StreamColor::${key[0]?.toUpperCase()}${key.slice(1)}\`, as \`${value}\`.`,
        );
        lines.push(
            `pub const ${prefix}_${key.toUpperCase()}: [u8; 3] = ${rgb(value)};`,
        );
    }
    return lines;
}

/** Render `tokens.rs`. */
export function emitRust(tokens: Tokens): string {
    const lines: string[] = [];
    for (const line of BANNER) {
        lines.push(line === "" ? "//" : `// ${line}`);
    }
    lines.push("//");
    lines.push(
        "// There is no Rust consumer today, and that is deliberate rather than an",
    );
    lines.push(
        "// omission: the CLI is specified as plain text with no colour",
    );
    lines.push(
        "// (docs/10-cross-cutting/accessibility.md), and the shared core does not",
    );
    lines.push(
        "// decide presentation (docs/01-architecture/shared-core.md). This file is",
    );
    lines.push(
        "// checked by `mise run tokens-check`, which compiles it and rustfmts it, so",
    );
    lines.push("// the first consumer adds one line and nothing else:");
    lines.push("//");
    lines.push(
        '//     include!("../../packages/sunrise-ui-tokens/generated/tokens.rs");',
    );
    lines.push("");

    lines.push("// Spacing scale, in logical pixels.");
    for (const key of SPACE_KEYS) {
        lines.push(`/// Spacing step \`${key}\`.`);
        lines.push(
            `pub const SPACE_${key.toUpperCase()}: u32 = ${num(tokens.space[key])};`,
        );
    }
    lines.push("");

    lines.push("// Corner radii, in logical pixels.");
    for (const key of RADIUS_KEYS) {
        lines.push(`/// Corner radius \`${key}\`.`);
        lines.push(
            `pub const RADIUS_${key.toUpperCase()}: u32 = ${num(tokens.radius[key])};`,
        );
    }
    lines.push("");

    lines.push("// Font sizes, numeric weights, and unitless line heights.");
    for (const key of TYPE_KEYS) {
        const value = tokens.type[key];
        const name = `TYPE_${key.toUpperCase()}`;
        lines.push(`/// Typography token \`${key}\`.`);
        if (key.startsWith("line_")) {
            lines.push(`pub const ${name}: f32 = ${float(value)};`);
        } else {
            lines.push(`pub const ${name}: u32 = ${num(value)};`);
        }
    }
    lines.push("");

    lines.push(
        "// Motion durations, in milliseconds, with their cubic-Bézier control points.",
    );
    for (const key of MOTION_CURVE_KEYS) {
        const motion = tokens.motion.curves[key];
        const name = key.toUpperCase();
        lines.push(`/// Duration of the \`${key}\` motion token.`);
        lines.push(
            `pub const MOTION_${name}_DURATION_MS: u32 = ${num(motion.durationMs)};`,
        );
        lines.push(
            `/// Cubic-Bézier control points of the \`${key}\` motion token.`,
        );
        lines.push(
            `pub const MOTION_${name}_EASING: [f32; 4] = [${motion.easing.map(float).join(", ")}];`,
        );
    }
    lines.push(
        "/// What every duration collapses to under a reduced-motion preference.",
    );
    lines.push(
        `pub const MOTION_REDUCED_DURATION_MS: u32 = ${num(tokens.motion.reducedDurationMs)};`,
    );
    lines.push("");

    lines.push(...themeConsts("SURFACE_LIGHT", tokens.light, "Light-theme"));
    lines.push("");
    lines.push(...themeConsts("SURFACE_DARK", tokens.dark, "Dark-theme"));
    lines.push("");
    lines.push(...streamConsts("STREAM_LIGHT", tokens.light, "Light-theme"));
    lines.push("");
    lines.push(...streamConsts("STREAM_DARK", tokens.dark, "Dark-theme"));
    lines.push("");
    return lines.join("\n");
}
