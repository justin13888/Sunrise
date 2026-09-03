/**
 * What the token set must be true of, regardless of what anybody edits.
 *
 * Three of these turn a sentence in a doc into something that fails:
 *
 * - `docs/07-clients/shared-ui-system.md` says the stream palette mirrors
 *   `StreamColor`. This file reads `crates/sunrise-domain/src/stream.rs` and
 *   fails when the two lists diverge, which is the enforceable form of the
 *   "stay in sync with" comment `packages/sunrise-ui/src/tokens.ts` used to
 *   carry and nothing checked.
 * - `docs/10-cross-cutting/accessibility.md` §Color and contrast asks for AA
 *   on interactive elements and AAA on body text. Those are ratios, so they
 *   are assertions.
 * - The same doc's Reduce Motion requirement is a `prefers-reduced-motion`
 *   block that has to exist in the emitted CSS.
 */

import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { emitCss } from "../src/emit-css";
import { emitSwift } from "../src/emit-swift";
import {
    buildTokens,
    type Hex,
    loadTokens,
    MOTION_CURVE_KEYS,
    parseMotion,
    parseSpacing,
    parseTheme,
    parseType,
    SURFACE_KEYS,
    TokenError,
} from "../src/model";

const tokens = await loadTokens();

/** The path to the domain enum the stream palette must agree with. */
const STREAM_RS = new URL(
    "../../../crates/sunrise-domain/src/stream.rs",
    import.meta.url,
);

/** WCAG 2.1 relative luminance of an `#rrggbb` colour. */
function luminance(hex: Hex): number {
    const channel = (at: number) => {
        const value = Number.parseInt(hex.slice(at, at + 2), 16) / 255;
        return value <= 0.03928
            ? value / 12.92
            : ((value + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
}

/** WCAG 2.1 contrast ratio, between 1 and 21. */
function contrast(a: Hex, b: Hex): number {
    const [light, dark] = [luminance(a), luminance(b)].sort(
        (x, y) => y - x,
    ) as [number, number];
    return (light + 0.05) / (dark + 0.05);
}

describe("the two themes are the same shape", () => {
    it("declares identical [surface] keys", () => {
        expect(Object.keys(tokens.dark.surface)).toEqual(
            Object.keys(tokens.light.surface),
        );
        expect(Object.keys(tokens.light.surface)).toEqual([...SURFACE_KEYS]);
    });

    it("declares identical [stream] keys", () => {
        expect(Object.keys(tokens.dark.stream)).toEqual(
            Object.keys(tokens.light.stream),
        );
        expect(Object.keys(tokens.light.stream)).toEqual([
            ...tokens.streamKeys,
        ]);
    });

    it("gives light and dark different values for every surface token but the near-blacks", () => {
        // `accent_text` is white on light and the dark background on dark, so
        // the pair genuinely differs; every other key must too, or one theme is
        // an unedited copy of the other.
        for (const key of SURFACE_KEYS) {
            expect(tokens.dark.surface[key], key).not.toBe(
                tokens.light.surface[key],
            );
        }
    });
});

describe("the stream palette is keyed on the domain enum", () => {
    /**
     * Both halves of the enum, because either one alone can be fooled.
     *
     * Reading only the `as_str` arms misses a variant added with a catch-all:
     * `Teal` plus `_ => "slate"` compiles, and `#[serde(rename_all =
     * "lowercase")]` still persists `"teal"` into the vault, so the palette
     * would be missing a colour that storage can hold. Reading only the
     * declaration misses the opposite — an arm renamed without its variant.
     */
    it("matches StreamColor in crates/sunrise-domain/src/stream.rs", async () => {
        const source = await readFile(STREAM_RS, "utf8");

        const declaration = /pub enum StreamColor \{([\s\S]*?)\n\}/.exec(
            source,
        );
        expect(
            declaration,
            "the StreamColor declaration must still be findable in stream.rs",
        ).not.toBeNull();
        const declared = [
            ...(declaration?.[1] ?? "").matchAll(/^ {4}([A-Z]\w*),$/gm),
        ].map((m) => (m[1] as string).toLowerCase());

        const body =
            /pub const fn as_str\(self\) -> &'static str \{([\s\S]*?)\n {4}\}/.exec(
                source,
            );
        expect(
            body,
            "StreamColor::as_str must still be findable in stream.rs",
        ).not.toBeNull();
        const spelled = [
            ...(body?.[1] ?? "").matchAll(/Self::\w+ => "(\w+)",/g),
        ].map((m) => m[1]);

        expect(
            declared.length,
            "stream.rs must declare at least one variant",
        ).toBeGreaterThan(0);
        // The two lists must agree with each other before either is worth
        // comparing to the palette: a mismatch here is a catch-all arm.
        expect([...spelled].sort()).toEqual([...declared].sort());
        expect([...tokens.streamKeys].sort()).toEqual([...declared].sort());
    });
});

describe("contrast meets docs/10-cross-cutting/accessibility.md", () => {
    for (const theme of ["light", "dark"] as const) {
        const { surface } = tokens[theme];

        it(`${theme}: body text on background is AAA (>= 7:1)`, () => {
            expect(contrast(surface.fg, surface.bg)).toBeGreaterThanOrEqual(7);
        });

        it(`${theme}: muted text on background is AA (>= 4.5:1)`, () => {
            expect(contrast(surface.muted, surface.bg)).toBeGreaterThanOrEqual(
                4.5,
            );
        });

        it(`${theme}: accent text on accent is AA (>= 4.5:1)`, () => {
            expect(
                contrast(surface.accent_text, surface.accent),
            ).toBeGreaterThanOrEqual(4.5);
        });
    }
});

describe("reduced motion is expressible on every target", () => {
    it("the CSS zeroes every duration under prefers-reduced-motion", () => {
        const css = emitCss(tokens);
        const block =
            /@media \(prefers-reduced-motion: reduce\) \{([\s\S]*?)\n\}/.exec(
                css,
            );
        expect(
            block,
            "the CSS must carry a prefers-reduced-motion block",
        ).not.toBeNull();
        for (const key of MOTION_CURVE_KEYS) {
            expect(block?.[1]).toContain(
                `--sunrise-motion-${key}-duration: 0ms;`,
            );
        }
    });

    it("the Swift carries a `reduced` duration the adapter can return nil for", () => {
        expect(emitSwift(tokens)).toContain(
            "static let reducedDuration: TimeInterval = 0 / 1000",
        );
        expect(tokens.motion.reducedDurationMs).toBe(0);
    });
});

describe("the Swift output stays compilable under strict concurrency", () => {
    const swift = emitSwift(tokens);

    it("imports Foundation and CoreGraphics and nothing else", () => {
        const imports = [...swift.matchAll(/^import (\w+)$/gm)].map(
            (m) => m[1],
        );
        expect(imports.sort()).toEqual(["CoreGraphics", "Foundation"]);
    });

    it("makes every value type Sendable, so a `static let` is legal", () => {
        for (const type of ["RGB", "Easing", "MotionToken"]) {
            expect(swift).toContain(`struct ${type}: Sendable, Equatable {`);
        }
    });
});

describe("the model refuses sources it cannot emit from", () => {
    const good = {
        spacing: { xs: 4, sm: 8, md: 12, lg: 16, xl: 24, xxl: 32 },
        radius: { sm: 4, md: 8, lg: 12, pill: 9999 },
        type: {
            size_xs: 11,
            size_sm: 13,
            size_base: 15,
            size_lg: 17,
            size_xl: 22,
            size_2xl: 28,
            weight_regular: 400,
            weight_medium: 500,
            weight_semibold: 600,
            weight_bold: 700,
            line_tight: 1.25,
            line_normal: 1.5,
        },
        motion: {
            fast: { duration_ms: 120, easing: [0.2, 0, 0, 1] },
            med: { duration_ms: 220, easing: [0.4, 0, 0.2, 1] },
            slow: { duration_ms: 360, easing: [0.4, 0, 0.2, 1] },
            linear: { duration_ms: 0, easing: [0, 0, 1, 1] },
            reduced: { duration_ms: 0 },
        },
        light: { surface: tokens.light.surface, stream: { slate: "#475569" } },
        dark: { surface: tokens.dark.surface, stream: { slate: "#94a3b8" } },
    };

    it("accepts a well-formed set", () => {
        expect(buildTokens(structuredClone(good)).streamKeys).toEqual([
            "slate",
        ]);
    });

    it("rejects a non-table source", () => {
        expect(() => parseSpacing(42)).toThrow(TokenError);
        expect(() => parseSpacing([4, 8])).toThrow(/expected a table/);
    });

    it("rejects a missing step", () => {
        expect(() =>
            parseSpacing({ xs: 4, sm: 8, md: 12, lg: 16, xl: 24 }),
        ).toThrow(/missing xxl/);
    });

    it("rejects a step nobody emits", () => {
        expect(() => parseSpacing({ ...good.spacing, xxxl: 64 })).toThrow(
            /unexpected xxxl/,
        );
    });

    it("rejects a fractional or negative length", () => {
        expect(() => parseSpacing({ ...good.spacing, md: 12.5 })).toThrow(
            /non-negative integer/,
        );
        expect(() => parseSpacing({ ...good.spacing, md: -4 })).toThrow(
            /non-negative integer/,
        );
    });

    it("rejects a line height that is not a positive number", () => {
        expect(() => parseType({ ...good.type, line_tight: 0 })).toThrow(
            /positive number/,
        );
    });

    it("rejects a colour that is not lowercase #rrggbb", () => {
        const surface = { ...tokens.light.surface, bg: "#FBFBFA" };
        expect(() =>
            parseTheme("light", { surface, stream: { slate: "#475569" } }),
        ).toThrow(/lowercase #rrggbb/);
        expect(() =>
            parseTheme("light", {
                surface: tokens.light.surface,
                stream: { slate: "red" },
            }),
        ).toThrow(/lowercase #rrggbb/);
    });

    it("rejects a stream key that is not a lowercase variant name", () => {
        expect(() =>
            parseTheme("light", {
                surface: tokens.light.surface,
                stream: { "stream-1": "#475569" },
            }),
        ).toThrow(/not a lowercase StreamColor variant name/);
    });

    it("rejects an empty stream palette", () => {
        expect(() =>
            parseTheme("light", { surface: tokens.light.surface, stream: {} }),
        ).toThrow(/at least one stream tint/);
    });

    it("rejects easing that is not four finite control points", () => {
        const motion = structuredClone(good.motion);
        motion.fast.easing = [0.2, 0, 0];
        expect(() => parseMotion(motion)).toThrow(/four control points/);
        motion.fast.easing = [0.2, 0, 0, Number.POSITIVE_INFINITY];
        expect(() => parseMotion(motion)).toThrow(/finite numbers/);
    });

    it("rejects an easing CSS would refuse", () => {
        // The abscissae are constrained to [0, 1]; the ordinates are free,
        // which is what lets a curve overshoot. `cubic-bezier(1.5, …)` is
        // invalid at computed-value time, and a browser drops the declaration
        // without saying so.
        const motion = structuredClone(good.motion);
        motion.fast.easing = [1.5, 0, 0, 1];
        expect(() => parseMotion(motion)).toThrow(/x1 must be in \[0, 1\]/);
        motion.fast.easing = [0.2, 0, -0.3, 1];
        expect(() => parseMotion(motion)).toThrow(/x2 must be in \[0, 1\]/);
        motion.fast.easing = [0.2, -0.6, 0, 1.8];
        expect(() => parseMotion(motion)).not.toThrow();
    });

    it("rejects a `reduced` policy that still animates", () => {
        const motion = structuredClone(good.motion);
        motion.reduced.duration_ms = 120;
        expect(() => parseMotion(motion)).toThrow(/duration_ms must be 0/);
    });

    it("rejects an easing written on `reduced`, rather than ignoring it", () => {
        const motion = structuredClone(good.motion) as Record<string, unknown>;
        motion.reduced = { duration_ms: 0, easing: [0, 0, 1, 1] };
        expect(() => parseMotion(motion)).toThrow(/unexpected easing/);
    });

    it("rejects themes whose stream palettes disagree", () => {
        const sources = {
            ...structuredClone(good),
            dark: { surface: tokens.dark.surface, stream: { rose: "#fb7185" } },
        };
        expect(() => buildTokens(sources)).toThrow(/different \[stream\] keys/);
    });
});
