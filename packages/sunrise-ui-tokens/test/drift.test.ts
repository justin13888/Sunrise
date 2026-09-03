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
import { type Hex, loadTokens, SPACE_KEYS, SURFACE_KEYS } from "../src/model";

const tokens = await loadTokens();

/** `#rrggbb` as the Swift emitter writes it. */
function rgbLiteral(hex: Hex): string {
    const at = (index: number) =>
        Number.parseInt(hex.slice(index, index + 2), 16);
    return `RGB(red: ${at(1)} / 255, green: ${at(3)} / 255, blue: ${at(5)} / 255)`;
}

/** `#rrggbb` as the Rust emitter writes it. */
function rustLiteral(hex: Hex): string {
    return `[0x${hex.slice(1, 3)}, 0x${hex.slice(3, 5)}, 0x${hex.slice(5, 7)}]`;
}

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
 * right. Every one of these mutations regenerated cleanly and left that block
 * green: `px` on the unitless type tokens, `prefers-color-scheme: dark`
 * swapped for `min-width: 0px`, the stream tints dropped from the CSS, Swift
 * dividing colour channels by 256, the whole `Space` enum omitted,
 * `SURFACE_DARK_*` carrying the light values, `color.dark` carrying the light
 * theme, and `taskStateGlyph.done` flipping to `[X]`.
 *
 * These assertions read the *emitted string*, never the model. Asserting light
 * ≠ dark on `tokens.light` / `tokens.dark` is what let the `color.dark`
 * mutation through: the model was fine, the output was not.
 */
describe("the emitted CSS has the right shape", () => {
    const css = emitCss(tokens);

    it("puts px on lengths and leaves weights and line heights unitless", () => {
        expect(css).toContain("--sunrise-space-md: 12px;");
        expect(css).toContain("--sunrise-radius-pill: 9999px;");
        expect(css).toContain("--sunrise-type-size-xs: 11px;");
        expect(css).toContain("--sunrise-type-weight-regular: 400;");
        expect(css).toContain("--sunrise-type-line-tight: 1.25;");
        // The mutation this catches spells them `400px` / `1.25px`, which is
        // not a `font-weight` or a `line-height` at all.
        expect(css).not.toMatch(
            /--sunrise-type-(weight|line)-[a-z]+: [\d.]+px;/,
        );
    });

    it("states durations in milliseconds", () => {
        expect(css).toContain("--sunrise-motion-fast-duration: 120ms;");
        expect(css).toContain(
            "--sunrise-motion-fast-easing: cubic-bezier(0.2, 0, 0, 1);",
        );
        expect(css).not.toContain("--sunrise-motion-reduced-easing");
    });

    it("carries both media queries, spelled the way a browser matches them", () => {
        expect(css).toContain("@media (prefers-color-scheme: dark) {");
        expect(css).toContain("@media (prefers-reduced-motion: reduce) {");
        // Exactly two at-rules: a third would mean a theme applying
        // unconditionally, which is how the `min-width: 0px` mutation read.
        expect(css.match(/@media /g)).toHaveLength(2);
    });

    it("emits both themes, and emits them differently", () => {
        const dark =
            /@media \(prefers-color-scheme: dark\) \{([\s\S]*?)\n\}/.exec(css);
        expect(dark, "the dark block must exist").not.toBeNull();
        const darkBlock = dark?.[1] ?? "";
        for (const key of SURFACE_KEYS) {
            const custom = `--sunrise-color-${key.replaceAll("_", "-")}`;
            expect(css, `${custom} must be bound on :root`).toContain(
                `${custom}: ${tokens.light.surface[key]};`,
            );
            expect(darkBlock, `${custom} must be rebound in dark`).toContain(
                `${custom}: ${tokens.dark.surface[key]};`,
            );
        }
    });

    it("emits the stream tints in both themes", () => {
        const dark =
            /@media \(prefers-color-scheme: dark\) \{([\s\S]*?)\n\}/.exec(css);
        for (const key of tokens.streamKeys) {
            expect(css).toContain(
                `--sunrise-stream-${key}: ${tokens.light.stream[key]};`,
            );
            expect(dark?.[1] ?? "").toContain(
                `--sunrise-stream-${key}: ${tokens.dark.stream[key]};`,
            );
        }
    });
});

describe("the emitted TypeScript has the right shape", () => {
    const ts = emitTs(tokens);

    it("emits the light theme under light and the dark theme under dark", () => {
        const light = / {4}light: \{([\s\S]*?)\n {4}\},/.exec(ts);
        const dark = / {4}dark: \{([\s\S]*?)\n {4}\},/.exec(ts);
        expect(light, "a `light:` block must exist").not.toBeNull();
        expect(dark, "a `dark:` block must exist").not.toBeNull();
        for (const key of SURFACE_KEYS) {
            expect(light?.[1] ?? "").toContain(
                `${key}: "${tokens.light.surface[key]}",`,
            );
            expect(dark?.[1] ?? "").toContain(
                `${key}: "${tokens.dark.surface[key]}",`,
            );
        }
        expect(light?.[1]).not.toBe(dark?.[1]);
    });

    it("round-trips the task-state glyphs its one consumer reads", () => {
        expect(ts).toContain('todo: "[ ]",');
        expect(ts).toContain('in_progress: "[·]",');
        expect(ts).toContain('done: "[x]",');
        expect(ts).toContain('cancelled: "[/]",');
    });

    it("states durations in milliseconds and keeps the scales whole", () => {
        expect(ts).toContain("durationMs: 120,");
        expect(ts).toContain("export const reducedMotionDurationMs = 0;");
        for (const key of SPACE_KEYS) {
            expect(ts).toContain(`    ${key}: ${tokens.space[key]},`);
        }
    });
});

describe("the emitted Swift has the right shape", () => {
    const swift = emitSwift(tokens);

    it("divides colour channels by 255", () => {
        expect(swift).toContain(
            "static let bg = RGB(red: 251 / 255, green: 251 / 255, blue: 250 / 255)",
        );
        expect(swift).not.toContain("/ 256");
    });

    it("emits every scale, whole", () => {
        expect(swift).toContain("enum Space {");
        for (const key of SPACE_KEYS) {
            expect(swift).toContain(
                `static let ${key}: CGFloat = ${tokens.space[key]}`,
            );
        }
        expect(swift).toContain("enum Radius {");
        expect(swift).toContain("static let pill: CGFloat = 9999");
        expect(swift).toContain("enum Typography {");
        expect(swift).toContain("static let sizeXs: CGFloat = 11");
    });

    it("states durations in seconds, not milliseconds", () => {
        expect(swift).toContain("duration: 120 / 1000,");
        expect(swift).toContain(
            "static let reducedDuration: TimeInterval = 0 / 1000",
        );
        expect(swift).not.toMatch(/duration: \d+,\s*$/m);
    });

    it("emits both themes, and emits them differently", () => {
        const light = /enum Light \{([\s\S]*?)\n {8}\}/.exec(swift);
        const dark = /enum Dark \{([\s\S]*?)\n {8}\}/.exec(swift);
        expect(light?.[1]).toBeDefined();
        expect(dark?.[1]).toBeDefined();
        expect(light?.[1]).not.toBe(dark?.[1]);
        expect(swift).toContain(
            `static let bg = ${rgbLiteral(tokens.dark.surface.bg)}  // ${tokens.dark.surface.bg}`,
        );
    });

    it("lists the stream names a hand-written enum can be checked against", () => {
        expect(swift).toContain(
            `static let names: [String] = [${tokens.streamKeys.map((k) => `"${k}"`).join(", ")}]`,
        );
    });
});

describe("the emitted Rust has the right shape", () => {
    const rust = emitRust(tokens);

    it("keeps the dark surface constants dark", () => {
        for (const key of SURFACE_KEYS) {
            const name = key.toUpperCase();
            expect(rust).toContain(
                `pub const SURFACE_LIGHT_${name}: [u8; 3] = ${rustLiteral(tokens.light.surface[key])};`,
            );
            expect(rust).toContain(
                `pub const SURFACE_DARK_${name}: [u8; 3] = ${rustLiteral(tokens.dark.surface[key])};`,
            );
        }
    });

    it("states durations in milliseconds and types floats as floats", () => {
        expect(rust).toContain("pub const MOTION_FAST_DURATION_MS: u32 = 120;");
        expect(rust).toContain(
            "pub const MOTION_REDUCED_DURATION_MS: u32 = 0;",
        );
        expect(rust).not.toContain("MOTION_REDUCED_EASING");
        expect(rust).toContain("pub const TYPE_LINE_TIGHT: f32 = 1.25;");
        expect(rust).toContain(
            "pub const MOTION_FAST_EASING: [f32; 4] = [0.2, 0.0, 0.0, 1.0];",
        );
    });

    it("stays `include!`-ready: no inner doc comment", () => {
        // `//!` in expansion position is a hard parse error (E0753), so the
        // banner has to be `//` and every item an outer `///`.
        expect(rust).not.toMatch(/^\/\/!/m);
        expect(rust.split("\n")[0]).toMatch(/^\/\/ Generated by/);
    });

    it("emits every scale, whole", () => {
        for (const key of SPACE_KEYS) {
            expect(rust).toContain(
                `pub const SPACE_${key.toUpperCase()}: u32 = ${tokens.space[key]};`,
            );
        }
    });
});
