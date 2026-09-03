/**
 * The CSS emitter: custom properties on `:root`, plus the two media queries
 * that make the tokens respond to a user preference rather than a build flag.
 *
 * Pure — `(tokens) => string` — so the drift test can compare it against the
 * committed file without touching the filesystem.
 */

import {
    BANNER,
    MOTION_KEYS,
    num,
    RADIUS_KEYS,
    SPACE_KEYS,
    type Theme,
    type Tokens,
    TYPE_KEYS,
} from "./model";

/** `accent_text` is one token; `--sunrise-color-accent-text` is its name. */
function customName(key: string): string {
    return key.replaceAll("_", "-");
}

function surfaceDeclarations(theme: Theme, indent: string): string[] {
    const lines: string[] = [];
    for (const [key, value] of Object.entries(theme.surface)) {
        lines.push(`${indent}--sunrise-color-${customName(key)}: ${value};`);
    }
    for (const [key, value] of Object.entries(theme.stream)) {
        lines.push(`${indent}--sunrise-stream-${key}: ${value};`);
    }
    return lines;
}

/** Render `tokens.css`. */
export function emitCss(tokens: Tokens): string {
    const lines: string[] = [];
    lines.push("/*");
    for (const line of BANNER) {
        lines.push(line === "" ? " *" : ` * ${line}`);
    }
    lines.push(" */");
    lines.push("");
    lines.push(":root {");
    for (const key of SPACE_KEYS) {
        lines.push(`    --sunrise-space-${key}: ${num(tokens.space[key])}px;`);
    }
    lines.push("");
    for (const key of RADIUS_KEYS) {
        lines.push(
            `    --sunrise-radius-${key}: ${num(tokens.radius[key])}px;`,
        );
    }
    lines.push("");
    for (const key of TYPE_KEYS) {
        const value = tokens.type[key];
        // Sizes carry a unit; weights and line heights are unitless by
        // definition, and `font-weight: 400px` is simply not a declaration.
        const rendered = key.startsWith("size_")
            ? `${num(value)}px`
            : num(value);
        lines.push(`    --sunrise-type-${customName(key)}: ${rendered};`);
    }
    lines.push("");
    for (const key of MOTION_KEYS) {
        const motion = tokens.motion[key];
        lines.push(
            `    --sunrise-motion-${key}-duration: ${num(motion.durationMs)}ms;`,
        );
        lines.push(
            `    --sunrise-motion-${key}-easing: cubic-bezier(${motion.easing.map(num).join(", ")});`,
        );
    }
    lines.push("");
    lines.push(...surfaceDeclarations(tokens.light, "    "));
    lines.push("}");
    lines.push("");
    lines.push(
        "/* The dark theme rebinds only the colour customs; nothing else changes. */",
    );
    lines.push("@media (prefers-color-scheme: dark) {");
    lines.push("    :root {");
    lines.push(...surfaceDeclarations(tokens.dark, "        "));
    lines.push("    }");
    lines.push("}");
    lines.push("");
    lines.push("/*");
    lines.push(
        " * Reduced motion is a policy, not a shorter duration: every duration",
    );
    lines.push(
        " * collapses to `reduced`, so a transition that reads a token cannot",
    );
    lines.push(
        " * animate at all. The easing customs are left alone — a curve over zero",
    );
    lines.push(" * milliseconds is not observable.");
    lines.push(" */");
    lines.push("@media (prefers-reduced-motion: reduce) {");
    lines.push("    :root {");
    for (const key of MOTION_KEYS) {
        lines.push(
            `        --sunrise-motion-${key}-duration: ${num(tokens.motion.reduced.durationMs)}ms;`,
        );
    }
    lines.push("    }");
    lines.push("}");
    lines.push("");
    return lines.join("\n");
}
