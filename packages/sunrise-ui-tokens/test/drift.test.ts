/**
 * The drift gate.
 *
 * `generated/` is committed because the two build environments that need it
 * most cannot produce it: Xcode compiles `tokens.swift` with neither Bun nor
 * mise on `PATH`. That makes each file an *input* to somebody else's build
 * rather than a report about this one, and a stale input compiles perfectly
 * while being wrong.
 *
 * This is the TypeScript analogue of `the_committed_description_is_current` in
 * `crates/sunrise-server/src/api/mod.rs`, and — unlike the CI job, which is
 * dormant while Actions billing is blocked — it runs from `pre-push` through
 * `lefthook.yaml`.
 *
 * A drift check alone is structurally blind, though: it compares the emitter
 * to itself. The second half of this file is the part that says what the
 * output must *look like*, and it is where a wrong emitter is caught.
 */

import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { GENERATED_DIR, OUTPUTS } from "../build";
import { emitCss } from "../src/emit-css";
import { emitRust } from "../src/emit-rust";
import { emitSwift } from "../src/emit-swift";
import { emitTs } from "../src/emit-ts";
import {
    type Hex,
    loadTokens,
    MOTION_CURVE_KEYS,
    RADIUS_KEYS,
    SPACE_KEYS,
    SURFACE_KEYS,
    TYPE_KEYS,
} from "../src/model";

const tokens = await loadTokens();

describe("the committed token files are current", () => {
    for (const [name, emit] of OUTPUTS) {
        it(`generated/${name}`, async () => {
            const committed = await readFile(
                new URL(name, GENERATED_DIR),
                "utf8",
            );
            expect(
                committed,
                `generated/${name} is stale; regenerate it with \`mise run tokens\``,
            ).toBe(emit(tokens));
        });
    }

    it("emits every file the package advertises", () => {
        expect(OUTPUTS.map(([name]) => name)).toEqual([
            "tokens.css",
            "tokens.ts",
            "tokens.swift",
            "tokens.rs",
        ]);
    });

    it("ends every file with exactly one trailing newline", () => {
        for (const [name, emit] of OUTPUTS) {
            const rendered = emit(tokens);
            expect(
                rendered.endsWith("\n"),
                `${name} must end with a newline`,
            ).toBe(true);
            expect(
                rendered.endsWith("\n\n"),
                `${name} must not end with a blank line`,
            ).toBe(false);
        }
    });

    it("names `mise run tokens` in every banner, so an editor is told what to run", () => {
        for (const [name, emit] of OUTPUTS) {
            expect(emit(tokens), name).toContain("mise run tokens");
            expect(emit(tokens), name).toContain("Do not edit");
        }
    });
});

/**
 * What each emitter's output has to *look like*.
 *
 * The drift block above compares the committed file to `emit(tokens)`, and both
 * sides come from the same function — so it proves somebody ran
 * `mise run tokens` and proves nothing at all about whether the emitter is
 * right. A wrong emitter regenerates and stays green.
 *
 * These checks are **systematic rather than exemplary**, and that is the whole
 * design. An earlier version pinned a handful of hardcoded examples — CSS
 * radius only for `pill`, Swift `Typography` only for `sizeXs`, easing only for
 * `fast` — so whole scales could be tripled, zeroed or fed the wrong
 * sub-object and stay green. Instead, each target rebuilds the *complete*
 * expected declaration list from the model, in emit order, with that target's
 * own serialisation spelled out here rather than imported from the emitter,
 * and compares it with `toEqual`.
 *
 * One `toEqual` over an ordered list is what catches all of: a wrong value, a
 * missing token, an extra one, a duplicated block, a reordering, and — because
 * every list is scoped to its own block — a light/dark swap, which whole-file
 * `toContain` cannot see because each theme's assertion is satisfied by the
 * other theme's block.
 *
 * These read the emitted *string*, never the model. Asserting light ≠ dark on
 * `tokens.light` / `tokens.dark` is what let a `color.dark` swap through once:
 * the model was fine, the output was not.
 */

/** `#rrggbb` split into its three 0-255 channels. */
function channels(hex: Hex): [number, number, number] {
    const at = (index: number) =>
        Number.parseInt(hex.slice(index, index + 2), 16);
    return [at(1), at(3), at(5)];
}

/** How many times `needle` occurs in `haystack`. */
function occurrences(haystack: string, needle: string): number {
    return haystack.split(needle).length - 1;
}

// --- CSS -------------------------------------------------------------------

/** Every `--name: value;` in one block, in order, as `"name: value"`. */
function cssDeclarations(block: string): string[] {
    return [...block.matchAll(/^ *--([a-z0-9-]+): (.+);$/gm)].map(
        (m) => `${m[1]}: ${m[2]}`,
    );
}

/** The body of `:root { … }`, of the dark block, and of the reduced block. */
function cssBlocks(css: string) {
    const root = /^:root \{\n([\s\S]*?)\n\}/m.exec(css);
    const dark =
        /@media \(prefers-color-scheme: dark\) \{\n([\s\S]*?)\n\}/.exec(css);
    const reduced =
        /@media \(prefers-reduced-motion: reduce\) \{\n([\s\S]*?)\n\}/.exec(
            css,
        );
    return { root: root?.[1], dark: dark?.[1], reduced: reduced?.[1] };
}

describe("the emitted CSS declares exactly the model, in order", () => {
    const css = emitCss(tokens);
    const blocks = cssBlocks(css);

    it("has the three blocks a consumer resolves against, and only those", () => {
        expect(blocks.root, ":root must exist").toBeDefined();
        expect(
            blocks.dark,
            "a prefers-color-scheme: dark block must exist",
        ).toBeDefined();
        expect(
            blocks.reduced,
            "a prefers-reduced-motion: reduce block must exist",
        ).toBeDefined();
        // Three `:root` selectors — the bare one plus one inside each media
        // query — and exactly two at-rules. A fourth `:root` is a duplicated
        // block; a third `@media` is a theme applying on something other than
        // the user's preference.
        expect(occurrences(css, ":root {")).toBe(3);
        expect(occurrences(css, "@media ")).toBe(2);
    });

    it("binds every token on :root with its CSS unit", () => {
        const expected: string[] = [];
        for (const key of SPACE_KEYS) {
            expected.push(`sunrise-space-${key}: ${tokens.space[key]}px`);
        }
        for (const key of RADIUS_KEYS) {
            expected.push(`sunrise-radius-${key}: ${tokens.radius[key]}px`);
        }
        for (const key of TYPE_KEYS) {
            // Sizes are lengths; weights and line heights are unitless by
            // definition — `font-weight: 400px` is not a declaration.
            const unit = key.startsWith("size_") ? "px" : "";
            expected.push(
                `sunrise-type-${key.replaceAll("_", "-")}: ${tokens.type[key]}${unit}`,
            );
        }
        for (const key of MOTION_CURVE_KEYS) {
            const { durationMs, easing } = tokens.motion.curves[key];
            expected.push(`sunrise-motion-${key}-duration: ${durationMs}ms`);
            expected.push(
                `sunrise-motion-${key}-easing: cubic-bezier(${easing.join(", ")})`,
            );
        }
        // Asserted positively, not just as "no reduced easing": dropping the
        // line entirely is otherwise green.
        expected.push(
            `sunrise-motion-reduced-duration: ${tokens.motion.reducedDurationMs}ms`,
        );
        for (const key of SURFACE_KEYS) {
            expected.push(
                `sunrise-color-${key.replaceAll("_", "-")}: ${tokens.light.surface[key]}`,
            );
        }
        for (const key of tokens.streamKeys) {
            expected.push(`sunrise-stream-${key}: ${tokens.light.stream[key]}`);
        }
        expect(cssDeclarations(blocks.root ?? "")).toEqual(expected);
    });

    it("rebinds every colour, and only colours, under prefers-color-scheme: dark", () => {
        const expected: string[] = [];
        for (const key of SURFACE_KEYS) {
            expected.push(
                `sunrise-color-${key.replaceAll("_", "-")}: ${tokens.dark.surface[key]}`,
            );
        }
        for (const key of tokens.streamKeys) {
            expected.push(`sunrise-stream-${key}: ${tokens.dark.stream[key]}`);
        }
        expect(cssDeclarations(blocks.dark ?? "")).toEqual(expected);
    });

    it("collapses every duration, and only durations, under reduced motion", () => {
        expect(cssDeclarations(blocks.reduced ?? "")).toEqual(
            MOTION_CURVE_KEYS.map(
                (key) =>
                    `sunrise-motion-${key}-duration: ${tokens.motion.reducedDurationMs}ms`,
            ),
        );
    });
});

// --- TypeScript ------------------------------------------------------------

/** The body of `export const <name> = { … } as const;`. */
function tsConst(ts: string, name: string): string {
    const found = new RegExp(
        `^export const ${name} = \\{\\n([\\s\\S]*?)\\n\\} as const;$`,
        "m",
    ).exec(ts);
    expect(found, `export const ${name} must exist`).not.toBeNull();
    return found?.[1] ?? "";
}

/** The body of a nested `<key>: { … },` at a known indent. */
function tsNested(block: string, key: string, indent: number): string {
    const pad = " ".repeat(indent);
    const found = new RegExp(
        `^${pad}${key}: \\{\\n([\\s\\S]*?)\\n${pad}\\},$`,
        "m",
    ).exec(block);
    expect(found, `a nested \`${key}\` must exist`).not.toBeNull();
    return found?.[1] ?? "";
}

/** Every `key: value,` in one block, in order. */
function tsEntries(block: string): string[] {
    return [...block.matchAll(/^ *([A-Za-z0-9_]+): (.+),$/gm)].map(
        (m) => `${m[1]}: ${m[2]}`,
    );
}

describe("the emitted TypeScript declares exactly the model, in order", () => {
    const ts = emitTs(tokens);

    it("exports each name once", () => {
        for (const name of [
            "spacing",
            "radii",
            "typography",
            "motion",
            "color",
            "streamColors",
            "reducedMotionDurationMs",
            "taskStateGlyph",
        ]) {
            expect(occurrences(ts, `export const ${name} `), name).toBe(1);
        }
    });

    it("emits the three numeric scales whole", () => {
        expect(tsEntries(tsConst(ts, "spacing"))).toEqual(
            SPACE_KEYS.map((key) => `${key}: ${tokens.space[key]}`),
        );
        expect(tsEntries(tsConst(ts, "radii"))).toEqual(
            RADIUS_KEYS.map((key) => `${key}: ${tokens.radius[key]}`),
        );
        expect(tsEntries(tsConst(ts, "typography"))).toEqual(
            TYPE_KEYS.map((key) => `${key}: ${tokens.type[key]}`),
        );
    });

    it("states every duration in milliseconds with its own curve", () => {
        const motion = tsConst(ts, "motion");
        for (const key of MOTION_CURVE_KEYS) {
            const { durationMs, easing } = tokens.motion.curves[key];
            expect(tsEntries(tsNested(motion, key, 4)), key).toEqual([
                `durationMs: ${durationMs}`,
                `easing: [${easing.join(", ")}]`,
            ]);
        }
        expect(ts).toContain(
            `export const reducedMotionDurationMs = ${tokens.motion.reducedDurationMs};`,
        );
    });

    it("emits each theme's colours under that theme's key", () => {
        const color = tsConst(ts, "color");
        for (const name of ["light", "dark"] as const) {
            const theme = tsNested(color, name, 4);
            expect(tsEntries(tsNested(theme, "surface", 8)), name).toEqual(
                SURFACE_KEYS.map(
                    (key) => `${key}: "${tokens[name].surface[key]}"`,
                ),
            );
            expect(tsEntries(tsNested(theme, "stream", 8)), name).toEqual(
                tokens.streamKeys.map(
                    (key) => `${key}: "${tokens[name].stream[key]}"`,
                ),
            );
        }
    });

    it("lists the stream names in declaration order", () => {
        const found =
            /^export const streamColors = \[\n([\s\S]*?)\n\] as const;$/m.exec(
                ts,
            );
        expect(found, "streamColors must exist").not.toBeNull();
        expect(
            [...(found?.[1] ?? "").matchAll(/^ *"(\w+)",$/gm)].map((m) => m[1]),
        ).toEqual([...tokens.streamKeys]);
    });

    it("round-trips the task-state glyphs its one consumer reads", () => {
        expect(tsEntries(tsConst(ts, "taskStateGlyph"))).toEqual([
            'todo: "[ ]"',
            'in_progress: "[·]"',
            'done: "[x]"',
            'cancelled: "[/]"',
        ]);
    });
});

// --- Swift -----------------------------------------------------------------

/** The body of `enum <name> {` at a known indent, brace-matched. */
function swiftEnum(source: string, name: string, indent: number): string {
    const pad = " ".repeat(indent);
    const open = `${pad}enum ${name} {\n`;
    expect(occurrences(source, open), `${name} must be declared once`).toBe(1);
    const start = source.indexOf(open) + open.length;
    const end = source.indexOf(`\n${pad}}`, start);
    expect(end, `${name} must be closed`).toBeGreaterThan(start);
    return source.slice(start, end);
}

/** Every `static let name = value` / `static let name: T = value`, in order. */
function swiftLets(block: string): string[] {
    return [
        ...block.matchAll(/^ *static let (\w+)(: [\w[\]]+)? = (.+)$/gm),
    ].map((m) => `${m[1]}${m[2] ?? ""} = ${m[3]}`);
}

describe("the emitted Swift declares exactly the model, in order", () => {
    const swift = emitSwift(tokens);

    it("imports Foundation and CoreGraphics and nothing else", () => {
        expect(
            [...swift.matchAll(/^import (\w+)$/gm)].map((m) => m[1]).sort(),
        ).toEqual(["CoreGraphics", "Foundation"]);
    });

    it("makes every value type Sendable, so a `static let` is legal", () => {
        for (const type of ["RGB", "Easing", "MotionToken"]) {
            expect(swift).toContain(`struct ${type}: Sendable, Equatable {`);
        }
    });

    it("emits the three CGFloat scales whole", () => {
        expect(swiftLets(swiftEnum(swift, "Space", 4))).toEqual(
            SPACE_KEYS.map((key) => `${key}: CGFloat = ${tokens.space[key]}`),
        );
        expect(swiftLets(swiftEnum(swift, "Radius", 4))).toEqual(
            RADIUS_KEYS.map((key) => `${key}: CGFloat = ${tokens.radius[key]}`),
        );
        expect(swiftLets(swiftEnum(swift, "Typography", 4))).toEqual(
            TYPE_KEYS.map(
                (key) =>
                    `${key.replace(/_(.)/g, (_, c: string) => c.toUpperCase())}: CGFloat = ${tokens.type[key]}`,
            ),
        );
    });

    it("states durations in seconds, with each curve's own control points", () => {
        const motion = swiftEnum(swift, "Motion", 4);

        // The block-scoped list first, exactly as the other enums are checked.
        // The value regex below reads only what it matches, so a stray
        // `static let legacyFast: TimeInterval = 999 / 1000` would otherwise
        // sit inside `Motion` unnoticed.
        expect(swiftLets(motion)).toEqual([
            ...MOTION_CURVE_KEYS.map((key) => `${key} = MotionToken(`),
            // Seconds, not milliseconds — `/ 1000`, like every curve above.
            `reducedDuration: TimeInterval = ${tokens.motion.reducedDurationMs} / 1000`,
        ]);

        const found = [
            ...motion.matchAll(
                /static let (\w+) = MotionToken\(\n *duration: (\S+) \/ 1000,\n *easing: Easing\(x1: (\S+), y1: (\S+), x2: (\S+), y2: (\S+)\)\n *\)/g,
            ),
        ].map((m) => m.slice(1, 7).join(" "));
        expect(found).toEqual(
            MOTION_CURVE_KEYS.map((key) => {
                const { durationMs, easing } = tokens.motion.curves[key];
                return [key, durationMs, ...easing].join(" ");
            }),
        );
    });

    it("emits each theme's colours under that theme's nested enum", () => {
        const surface = swiftEnum(swift, "Surface", 4);
        const stream = swiftEnum(swift, "Stream", 4);
        for (const [name, theme] of [
            ["Light", "light"],
            ["Dark", "dark"],
        ] as const) {
            const rgb = (hex: Hex) => {
                const [red, green, blue] = channels(hex);
                // `/ 255`, never `/ 256`: the comment carries the source hex so
                // a swapped block is visible in the failure diff too.
                return `RGB(red: ${red} / 255, green: ${green} / 255, blue: ${blue} / 255)  // ${hex}`;
            };
            expect(swiftLets(swiftEnum(surface, name, 8)), name).toEqual(
                SURFACE_KEYS.map(
                    (key) =>
                        `${key.replace(/_(.)/g, (_, c: string) => c.toUpperCase())} = ${rgb(tokens[theme].surface[key] as Hex)}`,
                ),
            );
            expect(swiftLets(swiftEnum(stream, name, 8)), name).toEqual(
                tokens.streamKeys.map(
                    (key) =>
                        `${key} = ${rgb(tokens[theme].stream[key] as Hex)}`,
                ),
            );
        }
    });

    it("lists the stream names in declaration order, and nothing else", () => {
        // `enum Stream` holds `names` plus the two nested theme enums, whose
        // members are checked above. Cutting at the first nested `enum` leaves
        // the direct members, so this is a list assertion rather than a
        // whole-file `toContain` an addition could slip past.
        const stream = swiftEnum(swift, "Stream", 4);
        const direct = stream.slice(0, stream.indexOf("\n        enum "));
        expect(swiftLets(direct)).toEqual([
            `names: [String] = [${tokens.streamKeys.map((key) => `"${key}"`).join(", ")}]`,
        ]);
    });
});

// --- Rust ------------------------------------------------------------------

describe("the emitted Rust declares exactly the model, in order", () => {
    const rust = emitRust(tokens);

    it("stays `include!`-ready: no inner doc comment", () => {
        // `//!` in expansion position is a hard parse error (E0753), so the
        // banner has to be `//` and every item an outer `///`.
        expect(rust).not.toMatch(/^\/\/!/m);
        expect(rust.split("\n")[0]).toMatch(/^\/\/ Generated by/);
    });

    it("declares every constant once, in order, with the right type", () => {
        const rgb = (hex: Hex) =>
            `[u8; 3] = [0x${hex.slice(1, 3)}, 0x${hex.slice(3, 5)}, 0x${hex.slice(5, 7)}]`;
        const expected: string[] = [];
        for (const key of SPACE_KEYS) {
            expected.push(
                `SPACE_${key.toUpperCase()}: u32 = ${tokens.space[key]}`,
            );
        }
        for (const key of RADIUS_KEYS) {
            expected.push(
                `RADIUS_${key.toUpperCase()}: u32 = ${tokens.radius[key]}`,
            );
        }
        for (const key of TYPE_KEYS) {
            // `pub const X: f32 = 0;` does not compile, so a float always
            // carries a point — and a size must not.
            const value = tokens.type[key] as number;
            expected.push(
                key.startsWith("line_")
                    ? `TYPE_${key.toUpperCase()}: f32 = ${Number.isInteger(value) ? `${value}.0` : value}`
                    : `TYPE_${key.toUpperCase()}: u32 = ${value}`,
            );
        }
        for (const key of MOTION_CURVE_KEYS) {
            const { durationMs, easing } = tokens.motion.curves[key];
            const name = key.toUpperCase();
            expected.push(`MOTION_${name}_DURATION_MS: u32 = ${durationMs}`);
            expected.push(
                `MOTION_${name}_EASING: [f32; 4] = [${easing
                    .map((point) =>
                        Number.isInteger(point) ? `${point}.0` : point,
                    )
                    .join(", ")}]`,
            );
        }
        expected.push(
            `MOTION_REDUCED_DURATION_MS: u32 = ${tokens.motion.reducedDurationMs}`,
        );
        for (const [prefix, theme] of [
            ["LIGHT", "light"],
            ["DARK", "dark"],
        ] as const) {
            for (const key of SURFACE_KEYS) {
                expected.push(
                    `SURFACE_${prefix}_${key.toUpperCase()}: ${rgb(tokens[theme].surface[key] as Hex)}`,
                );
            }
        }
        for (const [prefix, theme] of [
            ["LIGHT", "light"],
            ["DARK", "dark"],
        ] as const) {
            for (const key of tokens.streamKeys) {
                expected.push(
                    `STREAM_${prefix}_${key.toUpperCase()}: ${rgb(tokens[theme].stream[key] as Hex)}`,
                );
            }
        }

        const found = [
            ...rust.matchAll(/^pub const (\w+): (.+) = (.+);$/gm),
        ].map((m) => `${m[1]}: ${m[2]} = ${m[3]}`);
        expect(found).toEqual(expected);
    });

    it("documents every constant, so a `missing_docs` crate can include! it", () => {
        const lines = rust.split("\n");
        for (const [index, line] of lines.entries()) {
            if (line.startsWith("pub const ")) {
                expect(
                    lines[index - 1],
                    `${line} must carry an outer doc comment`,
                ).toMatch(/^\/\/\/ /);
            }
        }
    });
});
