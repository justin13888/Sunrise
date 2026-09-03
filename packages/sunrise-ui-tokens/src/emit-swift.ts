/**
 * The Swift emitter.
 *
 * Two constraints shape the output, both from `apps/apple/project.yml`:
 * `SWIFT_STRICT_CONCURRENCY: complete` with `SWIFT_TREAT_WARNINGS_AS_ERRORS:
 * YES`, so every `static let` must hold a `Sendable` value; and the file is
 * compiled into both app targets, so it imports **Foundation and CoreGraphics
 * only**. No `import SwiftUI`: turning a token into a `Color` or an `Animation`
 * is a rendering decision, and it lives in the hand-written adapter at
 * `apps/apple/Sunrise/Design/Tokens.swift`.
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

/** `size_2xl` -> `size2xl`, `accent_text` -> `accentText`. */
function camel(key: string): string {
    return key.replace(/_(.)/g, (_, char: string) => char.toUpperCase());
}

/**
 * `#rrggbb` as a Swift `RGB(...)` literal.
 *
 * The components stay as `n / 255` divisions rather than pre-divided decimals:
 * the compiler folds them, and a reviewer can check one against the hex without
 * a calculator.
 */
function rgb(hex: string): string {
    const component = (at: number) =>
        Number.parseInt(hex.slice(at, at + 2), 16);
    const [red, green, blue] = [component(1), component(3), component(5)];
    return `RGB(red: ${red} / 255, green: ${green} / 255, blue: ${blue} / 255)`;
}

function themeEnum(name: string, theme: Theme, indent: string): string[] {
    const lines: string[] = [];
    lines.push(`${indent}enum ${name} {`);
    for (const [key, value] of Object.entries(theme.surface)) {
        lines.push(
            `${indent}    static let ${camel(key)} = ${rgb(value)}  // ${value}`,
        );
    }
    lines.push(`${indent}}`);
    return lines;
}

function streamEnum(name: string, theme: Theme, indent: string): string[] {
    const lines: string[] = [];
    lines.push(`${indent}enum ${name} {`);
    for (const [key, value] of Object.entries(theme.stream)) {
        lines.push(
            `${indent}    static let ${key} = ${rgb(value)}  // ${value}`,
        );
    }
    lines.push(`${indent}}`);
    return lines;
}

/** Render `tokens.swift`. */
export function emitSwift(tokens: Tokens): string {
    const lines: string[] = [];
    for (const line of BANNER) {
        lines.push(line === "" ? "//" : `// ${line}`);
    }
    lines.push("");
    lines.push("import CoreGraphics");
    lines.push("import Foundation");
    lines.push("");
    lines.push("/// The generated design tokens, as plain values.");
    lines.push("///");
    lines.push(
        "/// Nothing here knows about SwiftUI. `Sunrise/Design/Tokens.swift` is the",
    );
    lines.push(
        "/// adapter that turns these into `Color`s and `Animation`s, and it is the only",
    );
    lines.push("/// file that decides which theme is in force.");
    lines.push("enum SunriseTokens {");

    lines.push("    /// An sRGB colour with components in `0...1`.");
    lines.push("    struct RGB: Sendable, Equatable {");
    lines.push("        let red: Double");
    lines.push("        let green: Double");
    lines.push("        let blue: Double");
    lines.push("    }");
    lines.push("");
    lines.push(
        "    /// Four cubic-Bézier control points: `(x1, y1)` and `(x2, y2)`.",
    );
    lines.push("    struct Easing: Sendable, Equatable {");
    lines.push("        let x1: Double");
    lines.push("        let y1: Double");
    lines.push("        let x2: Double");
    lines.push("        let y2: Double");
    lines.push("    }");
    lines.push("");
    lines.push("    /// A duration in seconds and the curve it runs on.");
    lines.push("    struct MotionToken: Sendable, Equatable {");
    lines.push("        let duration: TimeInterval");
    lines.push("        let easing: Easing");
    lines.push("    }");
    lines.push("");

    lines.push("    /// Spacing scale, in points.");
    lines.push("    enum Space {");
    for (const key of SPACE_KEYS) {
        lines.push(
            `        static let ${key}: CGFloat = ${num(tokens.space[key])}`,
        );
    }
    lines.push("    }");
    lines.push("");

    lines.push("    /// Corner radii, in points.");
    lines.push("    enum Radius {");
    for (const key of RADIUS_KEYS) {
        lines.push(
            `        static let ${key}: CGFloat = ${num(tokens.radius[key])}`,
        );
    }
    lines.push("    }");
    lines.push("");

    lines.push(
        "    /// Font sizes (points), numeric weights, and unitless line heights.",
    );
    lines.push("    enum Typography {");
    for (const key of TYPE_KEYS) {
        lines.push(
            `        static let ${camel(key)}: CGFloat = ${num(tokens.type[key])}`,
        );
    }
    lines.push("    }");
    lines.push("");

    lines.push(
        "    /// Durations and curves, plus the one policy value that is neither.",
    );
    lines.push("    enum Motion {");
    for (const key of MOTION_CURVE_KEYS) {
        const motion = tokens.motion.curves[key];
        const [x1, y1, x2, y2] = motion.easing;
        lines.push(`        static let ${key} = MotionToken(`);
        lines.push(`            duration: ${num(motion.durationMs)} / 1000,`);
        lines.push(
            `            easing: Easing(x1: ${num(x1)}, y1: ${num(y1)}, x2: ${num(x2)}, y2: ${num(y2)})`,
        );
        lines.push("        )");
    }
    lines.push("");
    lines.push(
        "        /// What every duration collapses to under Reduce Motion. Seconds,",
    );
    lines.push(
        "        /// like every other duration here, and not a curve: nothing is drawn.",
    );
    lines.push(
        `        static let reducedDuration: TimeInterval = ${num(tokens.motion.reducedDurationMs)} / 1000`,
    );
    lines.push("    }");
    lines.push("");

    lines.push("    /// The semantic surface palette, per theme.");
    lines.push("    enum Surface {");
    lines.push(...themeEnum("Light", tokens.light, "        "));
    lines.push("");
    lines.push(...themeEnum("Dark", tokens.dark, "        "));
    lines.push("    }");
    lines.push("");

    lines.push(
        "    /// The per-`StreamColor` tints, per theme. Keyed on the domain enum.",
    );
    lines.push("    enum Stream {");
    lines.push(
        "        /// Every `StreamColor` name the token set carries, in declaration",
    );
    lines.push(
        "        /// order. It is what a hand-written list of cases can be checked against.",
    );
    lines.push(
        `        static let names: [String] = [${tokens.streamKeys.map((k) => `"${k}"`).join(", ")}]`,
    );
    lines.push("");
    lines.push(...streamEnum("Light", tokens.light, "        "));
    lines.push("");
    lines.push(...streamEnum("Dark", tokens.dark, "        "));
    lines.push("    }");
    lines.push("}");
    lines.push("");
    return lines.join("\n");
}
